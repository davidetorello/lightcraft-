//! Sky detection for the Sky mask: a multi-stage detector on a 256 px grid of the frame.
//!
//! 1. **Confidence** per pixel from three local signals: *colour* (blue, bright grey or white for an overcast
//!    sky but never with a green cast, orange or pink for a sunset, and not saturated reds), *texture* (a low mean
//!    luma gradient and a low colour variance, so a panned, blurred background with red and white barriers does not
//!    pass) and *position* (a prior falling from 1 at the top to 0.25 at the bottom, along the horizon's tilt).
//! 2. **Continuity**: region growing from the confident pixels of the top rows through pixels of continuous colour,
//!    then the edges grow a few pixels (texture is a window average and rises near every boundary) and holes
//!    enclosed by the sky are filled when bright (clouds), sky-coloured or tiny (birds, wires).
//! 3. **Horizon**: when the grown sky's lower edge is straight along the tilt (sea, plain), that line is a limit.
//! 4. **Morphology**: an opening cuts thin bridges and specks; only the main region and large ones stay.
//! 5. **Reliability**: under 5 % of the frame, or a main region under 70 % of the selection (fragmented), gives no
//!    mask rather than a wrong one.
//! 6. **Edges** are snapped to the image with a guided filter on the luma (at most 1024 px), then enlarged.
//!
//! The stages and thresholds follow the author's Lumen app (`SkySegmenter`), reimplemented here in Rust; the
//! guided filter is He, Sun & Tang, "Guided Image Filtering" (2010). No other project's code was consulted.

use lightcraft_raster::Rgb32f;

/// The long side of the analysis grid.
const GRID_LONG_SIDE: usize = 256;
/// The long side of the image the edges are refined at.
const REFINE_LONG_SIDE: usize = 1024;
/// Smallest grid side worth analysing.
const MIN_SIDE: usize = 8;
/// The default sensitivity (0 selects more, 1 only the surest sky).
pub const DEFAULT_THRESHOLD: f32 = 0.5;

#[derive(Debug, Clone, PartialEq)]
pub enum Sky {
    /// Coverage 0..=1 on the analysis grid (`w × h`, rows top-down).
    Mask(Vec<f32>),
    /// No sky at the top of the frame, or almost none.
    NotFound,
    /// Something sky-like, too little or too fragmented to trust.
    Uncertain,
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// 1 inside [`low`, `high`] degrees, smooth to 0 over `soft` outside.
fn band(hue: f32, low: f32, high: f32, soft: f32) -> f32 {
    smoothstep(low - soft, low, hue) * (1.0 - smoothstep(high, high + soft, hue))
}

fn hsv(c: [f32; 3]) -> (f32, f32, f32) {
    let mx = c[0].max(c[1]).max(c[2]);
    let mn = c[0].min(c[1]).min(c[2]);
    let chroma = mx - mn;
    if chroma <= 1e-5 {
        return (0.0, 0.0, mx);
    }
    let mut hue = if mx == c[0] {
        (c[1] - c[2]) / chroma
    } else if mx == c[1] {
        2.0 + (c[2] - c[0]) / chroma
    } else {
        4.0 + (c[0] - c[1]) / chroma
    } * 60.0;
    if hue < 0.0 {
        hue += 360.0;
    }
    (hue, chroma / mx.max(1e-4), mx)
}

fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// The 4-connected neighbours of pixel `i` of a `w × h` grid.
fn neighbors(i: usize, w: usize, h: usize) -> impl Iterator<Item = usize> {
    let (x, y) = (i % w, i / w);
    [(x > 0).then(|| i - 1), (x + 1 < w).then(|| i + 1), (y > 0).then(|| i - w), (y + 1 < h).then(|| i + w)].into_iter().flatten()
}

/// Box mean over a `(2·radius + 1)²` window (clamped at the borders) through a summed-area table.
fn box_mean(values: &[f32], w: usize, h: usize, radius: usize) -> Vec<f32> {
    let mut table = vec![0.0f64; (w + 1) * (h + 1)];
    for y in 0..h {
        let mut row = 0.0f64;
        for x in 0..w {
            row += f64::from(values.get(y * w + x).copied().unwrap_or(0.0));
            let above = table.get(y * (w + 1) + x + 1).copied().unwrap_or(0.0);
            if let Some(t) = table.get_mut((y + 1) * (w + 1) + x + 1) {
                *t = above + row;
            }
        }
    }
    let at = |x: usize, y: usize| table.get(y * (w + 1) + x).copied().unwrap_or(0.0);
    let mut out = vec![0.0f32; w * h];
    for y in 0..h {
        let (y0, y1) = (y.saturating_sub(radius), (y + radius + 1).min(h));
        for x in 0..w {
            let (x0, x1) = (x.saturating_sub(radius), (x + radius + 1).min(w));
            let sum = at(x1, y1) - at(x1, y0) - at(x0, y1) + at(x0, y0);
            if let Some(o) = out.get_mut(y * w + x) {
                *o = (sum / ((x1 - x0) * (y1 - y0)) as f64) as f32;
            }
        }
    }
    out
}

/// The connected components (4-neighbour) of the pixels `included` selects.
fn components(w: usize, h: usize, included: impl Fn(usize) -> bool) -> Vec<Vec<usize>> {
    let mut seen = vec![false; w * h];
    let mut result = Vec::new();
    for start in 0..w * h {
        if seen[start] || !included(start) {
            continue;
        }
        let mut comp = vec![start];
        seen[start] = true;
        let mut head = 0;
        while let Some(&i) = comp.get(head) {
            head += 1;
            for j in neighbors(i, w, h) {
                if !seen[j] && included(j) {
                    seen[j] = true;
                    comp.push(j);
                }
            }
        }
        result.push(comp);
    }
    result
}

/// The frame on the analysis grid: gamma-encoded RGB, 0..=1, rows top-down.
struct Grid<'a> {
    w: usize,
    h: usize,
    rgb: &'a [[f32; 3]],
}

struct Signals {
    /// Colour alone (for edge growth and holes, where texture is unreliable).
    color: Vec<f32>,
    /// colour × texture × position prior.
    confidence: Vec<f32>,
}

impl Grid<'_> {
    fn px(&self, i: usize) -> [f32; 3] {
        self.rgb.get(i).copied().unwrap_or([0.0; 3])
    }

    /// 0 at the top edge, 1 at the bottom, measured across the tilted horizon.
    fn depth(&self, i: usize, tilt: f64) -> f32 {
        let x = (i % self.w) as f32 - self.w as f32 / 2.0;
        let y = (i / self.w) as f32;
        (y * tilt.cos() as f32 + x * tilt.sin() as f32) / self.h as f32
    }

    fn signals(&self, tilt: f64) -> Signals {
        let (w, h) = (self.w, self.h);
        let n = w * h;
        let lum: Vec<f32> = (0..n).map(|i| luma(self.px(i))).collect();
        let mut gradient = vec![0.0f32; n];
        for y in 1..h.saturating_sub(1) {
            for x in 1..w.saturating_sub(1) {
                let i = y * w + x;
                gradient[i] = (lum[i + 1] - lum[i - 1]).abs() + (lum[i + w] - lum[i - w]).abs();
            }
        }
        let mean_gradient = box_mean(&gradient, w, h, 2);
        // colour variance over 9×9: the sum over channels of E[c²] − E[c]²
        let mut variance = vec![0.0f32; n];
        for c in 0..3 {
            let channel: Vec<f32> = (0..n).map(|i| self.px(i)[c]).collect();
            let squares: Vec<f32> = channel.iter().map(|v| v * v).collect();
            let (mean, mean_sq) = (box_mean(&channel, w, h, 4), box_mean(&squares, w, h, 4));
            for i in 0..n {
                variance[i] += (mean_sq[i] - mean[i] * mean[i]).max(0.0);
            }
        }
        let mut color = vec![0.0f32; n];
        let mut confidence = vec![0.0f32; n];
        for i in 0..n {
            let (hue, sat, val) = hsv(self.px(i));
            let blue = band(hue, 185.0, 260.0, 15.0) * smoothstep(0.05, 0.12, sat) * (1.0 - smoothstep(0.85, 0.95, sat)) * smoothstep(0.2, 0.35, val);
            // grey skies are neutral or bluish; a green cast is foliage or out-of-focus hills
            let green_cast = band(hue, 60.0, 170.0, 10.0) * smoothstep(0.05, 0.12, sat);
            let overcast = (1.0 - smoothstep(0.12, 0.24, sat)) * smoothstep(0.42, 0.58, val) * (1.0 - green_cast);
            // sunsets: orange or pink, never a saturated red
            let warm = band(hue, 18.0, 50.0, 8.0).max(band(hue, 320.0, 352.0, 8.0))
                * smoothstep(0.08, 0.15, sat)
                * (1.0 - smoothstep(0.55, 0.7, sat))
                * smoothstep(0.4, 0.55, val)
                * 0.8;
            color[i] = blue.max(overcast * 0.9).max(warm);
            let texture = (1.0 - smoothstep(0.02, 0.07, mean_gradient[i])) * (1.0 - smoothstep(0.0012, 0.008, variance[i]));
            let position = 1.0 - 0.75 * smoothstep(0.1, 0.9, self.depth(i, tilt));
            confidence[i] = color[i] * texture * (0.4 + 0.6 * position);
        }
        Signals { color, confidence }
    }

    /// Region growing from the top rows; `None` when the top of the frame has no sky.
    fn grow(&self, confidence: &[f32], cut: f32) -> Option<Vec<f32>> {
        let (w, h) = (self.w, self.h);
        let mut sky = vec![0.0f32; w * h];
        let mut queue: Vec<usize> = Vec::new();
        let seed_rows = (h / 8).max(2);
        for y in 0..seed_rows.min(h) {
            for x in 0..w {
                let i = y * w + x;
                if confidence[i] > cut + 0.1 {
                    sky[i] = 1.0;
                    queue.push(i);
                }
            }
        }
        // at least 10 % of the top band must look like sky
        if queue.len() < seed_rows * w / 10 || queue.is_empty() {
            return None;
        }
        let mut head = 0;
        while let Some(&i) = queue.get(head) {
            head += 1;
            let here = self.px(i);
            for j in neighbors(i, w, h) {
                if sky[j] == 0.0 && confidence[j] > cut * 0.7 && dist(self.px(j), here) < 0.07 {
                    sky[j] = 1.0;
                    queue.push(j);
                }
            }
        }
        Some(sky)
    }

    /// 5 steps: the colour-variance window (9×9) keeps the growth up to 4 px short of a boundary.
    fn expand_edges(&self, sky: &mut [f32], color: &[f32]) {
        for _ in 0..5 {
            let added: Vec<usize> = (0..sky.len())
                .filter(|&i| {
                    sky[i] == 0.0 && color[i] > 0.3 && neighbors(i, self.w, self.h).any(|j| sky[j] > 0.0 && dist(self.px(j), self.px(i)) < 0.1)
                })
                .collect();
            if added.is_empty() {
                break;
            }
            for i in added {
                sky[i] = 1.0;
            }
        }
    }

    /// Fills non-sky components inside the sky that are bright (clouds), sky-coloured or tiny: enclosed ones, and
    /// bands reaching the sides (a cloud bank across the frame) when there is sky below them in most columns.
    /// Never what reaches the bottom.
    fn fill_holes(&self, sky: &mut [f32], color: &[f32]) {
        let (w, h) = (self.w, self.h);
        let holes = components(w, h, |i| sky[i] == 0.0);
        for comp in holes {
            if comp.iter().any(|&i| i / w == h - 1) {
                continue;
            }
            let open = comp.iter().any(|&i| i % w == 0 || i % w == w - 1 || i / w == h - 1);
            if open && !self.has_sky_below(&comp, sky) {
                continue;
            }
            let count = comp.len() as f32;
            let brightness = comp.iter().map(|&i| self.px(i).iter().fold(0.0f32, |a, b| a.max(*b))).sum::<f32>() / count;
            let sky_color = comp.iter().map(|&i| color[i]).sum::<f32>() / count;
            if count < sky.len() as f32 * 0.002 || brightness > 0.55 || sky_color > 0.4 {
                for i in comp {
                    sky[i] = 1.0;
                }
            }
        }
    }

    fn has_sky_below(&self, comp: &[usize], sky: &[f32]) -> bool {
        let w = self.w;
        let mut lowest: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
        for &i in comp {
            let e = lowest.entry(i % w).or_insert(0);
            *e = (*e).max(i / w);
        }
        let covered = lowest.iter().filter(|(x, y)| ((**y + 1)..self.h).any(|r| sky[r * w + **x] > 0.0)).count();
        covered * 10 >= lowest.len() * 7
    }

    /// A straight lower edge along the tilt is a real horizon (sea, plain): nothing below it is sky.
    fn limit_to_horizon(&self, sky: &mut [f32], tilt: f64) {
        let (w, h) = (self.w, self.h);
        // the lowest sky depth per column, for columns where the sky reaches down from the top
        let mut bottoms: Vec<f32> = Vec::new();
        for x in 0..w {
            if sky[x] > 0.0 {
                let mut y = 0;
                while y + 1 < h && sky[(y + 1) * w + x] > 0.0 {
                    y += 1;
                }
                bottoms.push(self.depth(y * w + x, tilt));
            }
        }
        if bottoms.len() <= w * 8 / 10 {
            return;
        }
        bottoms.sort_by(f32::total_cmp);
        let median = bottoms[bottoms.len() / 2];
        let spread = bottoms[bottoms.len() * 9 / 10] - bottoms[bottoms.len() / 10];
        // straight within 3 % of the height across 80 % of the columns
        if spread >= 0.03 {
            return;
        }
        let limit = median + 0.02;
        for i in 0..sky.len() {
            if sky[i] > 0.0 && self.depth(i, tilt) > limit {
                sky[i] = 0.0;
            }
        }
    }

    /// Morphological opening (3×3 erosion, then dilation): removes specks and thin bridges to false regions.
    fn open(&self, sky: &mut [f32]) {
        let (w, h) = (self.w, self.h);
        let eroded: Vec<bool> = (0..sky.len()).map(|i| sky[i] > 0.0 && neighbors(i, w, h).all(|j| sky[j] > 0.0)).collect();
        for i in 0..sky.len() {
            sky[i] = if sky[i] > 0.0 && (eroded[i] || neighbors(i, w, h).any(|j| eroded[j])) { 1.0 } else { 0.0 };
        }
    }

    /// Keeps the largest region and those over 2 % of the frame; returns the largest region's share of what was
    /// selected (the fragmentation measure).
    fn keep_main_region(&self, sky: &mut [f32]) -> f32 {
        let parts = components(self.w, self.h, |i| sky[i] > 0.0);
        let total: usize = parts.iter().map(Vec::len).sum();
        let Some(largest) = parts.iter().map(Vec::len).max() else { return 0.0 };
        if total == 0 {
            return 0.0;
        }
        for part in &parts {
            if part.len() < largest && part.len() < sky.len() / 50 {
                for &i in part {
                    sky[i] = 0.0;
                }
            }
        }
        largest as f32 / total as f32
    }
}

/// Detect the sky in a `w × h` grid of gamma-encoded RGB. `tilt` is the horizon's tilt in radians
/// (counter-clockwise); `threshold` 0..=1 moves the confidence cut (low selects more, with more false positives).
pub fn detect(rgb: &[[f32; 3]], w: usize, h: usize, tilt: f64, threshold: f32) -> Sky {
    if w < MIN_SIDE || h < MIN_SIDE || rgb.len() != w * h {
        return Sky::NotFound;
    }
    let tilt = if tilt.is_finite() { tilt.clamp(-0.8, 0.8) } else { 0.0 };
    let grid = Grid { w, h, rgb };
    let signals = grid.signals(tilt);
    let cut = 0.18 + 0.44 * if threshold.is_finite() { threshold.clamp(0.0, 1.0) } else { DEFAULT_THRESHOLD };
    let Some(mut sky) = grid.grow(&signals.confidence, cut) else { return Sky::NotFound };
    grid.expand_edges(&mut sky, &signals.color);
    grid.fill_holes(&mut sky, &signals.color);
    grid.limit_to_horizon(&mut sky, tilt);
    grid.open(&mut sky);
    let fraction = grid.keep_main_region(&mut sky);
    let coverage = sky.iter().filter(|v| **v > 0.0).count() as f32 / sky.len() as f32;
    if coverage < 0.05 {
        return if coverage < 0.005 { Sky::NotFound } else { Sky::Uncertain };
    }
    if fraction < 0.7 {
        return Sky::Uncertain;
    }
    Sky::Mask(sky)
}

/// `img` (linear, working space, scaled by `gain`) averaged into a `tw × th` grid and mapped to gamma-encoded sRGB.
fn display_grid(img: &Rgb32f, gain: f32, tw: usize, th: usize) -> Vec<[f32; 3]> {
    let (w, h) = (img.width, img.height);
    let to_srgb = lightcraft_color::REC2020.to_space(&lightcraft_color::SRGB);
    let mut out = Vec::with_capacity(tw * th);
    for ty in 0..th {
        let (y0, y1) = (ty * h / th, ((ty + 1) * h / th).max(ty * h / th + 1).min(h));
        for tx in 0..tw {
            let (x0, x1) = (tx * w / tw, ((tx + 1) * w / tw).max(tx * w / tw + 1).min(w));
            let mut acc = [0.0f64; 3];
            let mut n = 0.0f64;
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = img.data.get(y * w + x).copied().unwrap_or([0.0; 3]);
                    for c in 0..3 {
                        let v = p[c] * gain;
                        acc[c] += if v.is_finite() { f64::from(v.max(0.0)) } else { 0.0 };
                    }
                    n += 1.0;
                }
            }
            let mean = acc.map(|a| (a / n.max(1.0)) as f32);
            // a rough display mapping, as the colour picker uses
            let mapped = mean.map(|v| v / (1.0 + v));
            let srgb = to_srgb.apply_f32(mapped);
            out.push(srgb.map(|v| lightcraft_color::transfer::linear_to_srgb(v.clamp(0.0, 1.0))));
        }
    }
    out
}

/// Bilinear enlargement of one plane from `sw × sh` to `tw × th`.
fn resize_plane(src: &[f32], sw: usize, sh: usize, tw: usize, th: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(tw * th);
    let at = |x: usize, y: usize| src.get(y.min(sh - 1) * sw + x.min(sw - 1)).copied().unwrap_or(0.0);
    for y in 0..th {
        let fy = ((y as f32 + 0.5) * sh as f32 / th as f32 - 0.5).clamp(0.0, sh as f32 - 1.0);
        let (y0, ty) = (fy.floor() as usize, fy.fract());
        for x in 0..tw {
            let fx = ((x as f32 + 0.5) * sw as f32 / tw as f32 - 0.5).clamp(0.0, sw as f32 - 1.0);
            let (x0, tx) = (fx.floor() as usize, fx.fract());
            let top = at(x0, y0) + (at(x0 + 1, y0) - at(x0, y0)) * tx;
            let bottom = at(x0, y0 + 1) + (at(x0 + 1, y0 + 1) - at(x0, y0 + 1)) * tx;
            out.push(top + (bottom - top) * ty);
        }
    }
    out
}

/// Guided filter of `p` with guide `guide` (both `w × h`), window radius `r`, regulariser `eps`.
fn guided_filter(guide: &[f32], p: &[f32], w: usize, h: usize, r: usize, eps: f32) -> Vec<f32> {
    let mean_i = box_mean(guide, w, h, r);
    let mean_p = box_mean(p, w, h, r);
    let ii: Vec<f32> = guide.iter().map(|v| v * v).collect();
    let ip: Vec<f32> = guide.iter().zip(p).map(|(a, b)| a * b).collect();
    let (corr_i, corr_ip) = (box_mean(&ii, w, h, r), box_mean(&ip, w, h, r));
    let mut a = vec![0.0f32; w * h];
    let mut b = vec![0.0f32; w * h];
    for k in 0..w * h {
        let var = corr_i[k] - mean_i[k] * mean_i[k];
        let cov = corr_ip[k] - mean_i[k] * mean_p[k];
        a[k] = cov / (var + eps);
        b[k] = mean_p[k] - a[k] * mean_i[k];
    }
    let (mean_a, mean_b) = (box_mean(&a, w, h, r), box_mean(&b, w, h, r));
    (0..w * h).map(|k| mean_a[k] * guide[k] + mean_b[k]).collect()
}

/// The sky coverage (0..=1) of `img` (linear working space times `gain`) at its own `w × h`; all zero when no sky
/// is found with confidence. Detected on a 256 px grid, so previews and exports agree.
pub fn alpha(img: &Rgb32f, gain: f32, threshold: f32) -> Vec<f32> {
    let (w, h) = (img.width, img.height);
    let zeros = || vec![0.0f32; w * h];
    if w == 0 || h == 0 || img.data.len() != w * h {
        return zeros();
    }
    let scale = |long: usize| (long as f32 / w.max(h) as f32).min(1.0);
    let (gw, gh) = (((w as f32 * scale(GRID_LONG_SIDE)).round() as usize).max(1), ((h as f32 * scale(GRID_LONG_SIDE)).round() as usize).max(1));
    let grid = display_grid(img, gain, gw, gh);
    let Sky::Mask(coarse) = detect(&grid, gw, gh, 0.0, threshold) else { return zeros() };
    // edges snapped to the image: guided filter on the luma at up to 1024 px, then enlarged to `w × h`
    let (rw, rh) = (((w as f32 * scale(REFINE_LONG_SIDE)).round() as usize).max(1), ((h as f32 * scale(REFINE_LONG_SIDE)).round() as usize).max(1));
    let guide: Vec<f32> = display_grid(img, gain, rw, rh).into_iter().map(luma).collect();
    let up = resize_plane(&coarse, gw, gh, rw, rh);
    let radius = ((8.0 * rw.max(rh) as f32 / REFINE_LONG_SIDE as f32).round() as usize).max(1);
    let refined = guided_filter(&guide, &up, rw, rh, radius, 0.0004);
    // firm up the transition the filter leaves (0.25 → 0, 0.75 → 1)
    let firm: Vec<f32> = refined.into_iter().map(|v| (2.0 * v - 0.5).clamp(0.0, 1.0)).collect();
    if (rw, rh) == (w, h) { firm } else { resize_plane(&firm, rw, rh, w, h) }
}

/// Long edge of the whole-frame proxy the sky is found on.
const MATTE_LONG_SIDE: usize = 768;

/// The sky of the whole source as a matte (0..255, over the source at proxy resolution), found once on a small,
/// white-balanced, exposure-normalised copy so a windowed render and a whole one agree. All zero when there is no
/// confident sky. The cache keeps the last result: slider drags re-plan on the same source.
pub fn source_matte(src: &Rgb32f, info: &crate::SourceInfo, s: &lightcraft_develop::DevelopSettings) -> lightcraft_raster::Image<u8> {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    use std::sync::Mutex;
    static LAST: Mutex<Option<(u64, lightcraft_raster::Image<u8>)>> = Mutex::new(None);
    let (w, h) = (src.width, src.height);
    let step = (src.data.len() / 997).max(1);
    let sample: Vec<u32> = src.data.iter().step_by(step).flat_map(|p| p.map(f32::to_bits)).collect();
    let (wb_t, wb_tint) = crate::local::effective_wb(info, s);
    let key = BuildHasherDefault::<DefaultHasher>::default().hash_one((
        w,
        h,
        sample,
        [wb_t, wb_tint, info.as_shot_temp, info.as_shot_tint].map(f64::to_bits),
    ));
    if let Some((_, m)) = LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref().filter(|(k, _)| *k == key) {
        return m.clone();
    }
    let matte = compute_matte(src, info, s);
    *LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((key, matte.clone()));
    matte
}

fn compute_matte(src: &Rgb32f, info: &crate::SourceInfo, s: &lightcraft_develop::DevelopSettings) -> lightcraft_raster::Image<u8> {
    let (w, h) = (src.width, src.height);
    let none = || lightcraft_raster::Image::<u8>::from_fn(1, 1, |_, _| 0);
    if w == 0 || h == 0 || src.data.len() != w * h {
        return none();
    }
    let k = (MATTE_LONG_SIDE as f64 / w.max(h) as f64).min(1.0);
    let (pw, ph) = (((w as f64 * k).round() as usize).max(1), ((h as f64 * k).round() as usize).max(1));
    // box-average down to the proxy
    let mut proxy = Rgb32f::from_fn(pw, ph, |tx, ty| {
        let (x0, x1) = (tx * w / pw, ((tx + 1) * w / pw).max(tx * w / pw + 1).min(w));
        let (y0, y1) = (ty * h / ph, ((ty + 1) * h / ph).max(ty * h / ph + 1).min(h));
        let mut acc = [0.0f64; 3];
        let mut n = 0.0f64;
        for y in y0..y1 {
            for x in x0..x1 {
                let p = src.data.get(y * w + x).copied().unwrap_or([0.0; 3]);
                for c in 0..3 {
                    acc[c] += if p[c].is_finite() { f64::from(p[c].max(0.0)) } else { 0.0 };
                }
                n += 1.0;
            }
        }
        acc.map(|a| (a / n.max(1.0)) as f32)
    });
    crate::local::white_balance(&mut proxy, info, s);
    // normalise the exposure: the geometric mean luminance to a mid-grey a developed picture would have
    let logs: f64 = proxy.data.iter().map(|p| f64::from(luma(*p).max(1e-5).ln())).sum::<f64>() / proxy.data.len().max(1) as f64;
    let gain = (0.18 / logs.exp()).clamp(0.25, 16.0) as f32;
    let a = alpha(&proxy, gain, DEFAULT_THRESHOLD);
    lightcraft_raster::Image::<u8>::from_fn(pw, ph, |x, y| (a.get(y * pw + x).copied().unwrap_or(0.0).clamp(0.0, 1.0) * 255.0).round() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 128;
    const H: usize = 96;

    /// A deterministic pseudo-random value in 0..1 for texture.
    fn noise(x: usize, y: usize) -> f32 {
        let mut v = (x as u32).wrapping_mul(374_761_393).wrapping_add((y as u32).wrapping_mul(668_265_263));
        v = (v ^ (v >> 13)).wrapping_mul(1_274_126_177);
        ((v ^ (v >> 16)) & 0xffff) as f32 / 65535.0
    }

    fn scene(f: impl Fn(usize, usize) -> [f32; 3]) -> Vec<[f32; 3]> {
        (0..W * H).map(|i| f(i % W, i / W)).collect()
    }

    /// Foliage: green with strong texture.
    fn foliage(x: usize, y: usize) -> [f32; 3] {
        let n = noise(x, y);
        [0.15 + 0.2 * n, 0.35 + 0.3 * noise(y, x), 0.1 + 0.1 * n]
    }

    fn blue_sky(_x: usize, y: usize) -> [f32; 3] {
        let t = y as f32 / H as f32;
        [0.35 + 0.2 * t, 0.55 + 0.2 * t, 0.9 - 0.1 * t]
    }

    fn coverage(mask: &[f32], rows: std::ops::Range<usize>) -> f32 {
        let (mut sum, mut n) = (0.0, 0.0);
        for y in rows {
            for x in 0..W {
                sum += mask[y * W + x];
                n += 1.0;
            }
        }
        sum / n
    }

    fn mask_of(rgb: &[[f32; 3]]) -> Vec<f32> {
        match detect(rgb, W, H, 0.0, DEFAULT_THRESHOLD) {
            Sky::Mask(m) => m,
            other => panic!("expected a sky mask, got {other:?}"),
        }
    }

    #[test]
    fn blue_sky_over_foliage_is_selected_and_the_foliage_is_not() {
        let rgb = scene(|x, y| if y < 40 { blue_sky(x, y) } else { foliage(x, y) });
        let m = mask_of(&rgb);
        assert!(coverage(&m, 2..36) > 0.95, "sky {}", coverage(&m, 2..36));
        assert!(coverage(&m, 48..H) < 0.02, "ground {}", coverage(&m, 48..H));
    }

    #[test]
    fn a_bright_grey_overcast_sky_is_selected_but_green_hills_are_not() {
        let rgb = scene(|x, y| {
            if y < 45 {
                [0.8, 0.82, 0.85]
            } else {
                // smooth, defocused, green-cast hills: no texture but the wrong hue
                let _ = x;
                [0.55, 0.7, 0.5]
            }
        });
        let m = mask_of(&rgb);
        assert!(coverage(&m, 2..40) > 0.95);
        assert!(coverage(&m, 52..H) < 0.02, "hills {}", coverage(&m, 52..H));
    }

    #[test]
    fn a_sunset_is_selected_but_a_saturated_red_wall_is_not() {
        let rgb = scene(|x, y| {
            if y < 35 {
                [0.95, 0.6 + 0.1 * y as f32 / 35.0, 0.45]
            } else if y < 50 {
                [0.8, 0.1, 0.08] // a saturated red barrier
            } else {
                foliage(x, y)
            }
        });
        let m = mask_of(&rgb);
        assert!(coverage(&m, 2..30) > 0.9, "sunset {}", coverage(&m, 2..30));
        assert!(coverage(&m, 52..H) < 0.02);
        assert!(coverage(&m, 38..48) < 0.1, "red band {}", coverage(&m, 38..48));
    }

    #[test]
    fn a_cloud_inside_the_sky_is_filled() {
        let rgb = scene(|x, y| {
            let cloud = ((x as f32 - 64.0).powi(2) / 400.0 + (y as f32 - 18.0).powi(2) / 60.0) < 1.0;
            if y >= 50 {
                foliage(x, y)
            } else if cloud {
                [0.93, 0.93, 0.95]
            } else {
                blue_sky(x, y)
            }
        });
        let m = mask_of(&rgb);
        assert!(m[18 * W + 64] > 0.5, "the cloud is part of the sky");
        assert!(coverage(&m, 56..H) < 0.02);
    }

    #[test]
    fn a_frame_without_sky_gives_no_mask() {
        let rgb = scene(foliage);
        assert_eq!(detect(&rgb, W, H, 0.0, DEFAULT_THRESHOLD), Sky::NotFound);
        // blue sky only at the bottom (water): not sky
        let rgb = scene(|x, y| if y < 60 { foliage(x, y) } else { blue_sky(x, y) });
        assert!(!matches!(detect(&rgb, W, H, 0.0, DEFAULT_THRESHOLD), Sky::Mask(_)));
    }

    #[test]
    fn a_straight_sea_horizon_is_a_hard_limit() {
        // a calm blue sea just below a blue sky: the same colour continues below the horizon
        let rgb = scene(|x, y| {
            if y < 40 {
                blue_sky(x, y)
            } else {
                let _ = x;
                [0.3, 0.5, 0.85]
            }
        });
        let m = mask_of(&rgb);
        assert!(coverage(&m, 2..36) > 0.9);
        assert!(coverage(&m, 46..H) < 0.05, "sea {}", coverage(&m, 46..H));
    }

    #[test]
    fn degenerate_inputs_do_not_panic() {
        assert_eq!(detect(&[], 0, 0, 0.0, 0.5), Sky::NotFound);
        assert_eq!(detect(&[[0.5; 3]; 4], 2, 2, 0.0, 0.5), Sky::NotFound);
        assert_eq!(detect(&[[0.5; 3]; 10], 8, 8, 0.0, 0.5), Sky::NotFound, "wrong length");
        let nan = vec![[f32::NAN, f32::INFINITY, -1.0]; W * H];
        let _ = detect(&nan, W, H, f64::NAN, f32::NAN);
        let _ = detect(&vec![[0.4, 0.6, 0.9]; W * H], W, H, 9.0, 7.0);
        assert!(alpha(&Rgb32f::new(0, 0), 1.0, 0.5).is_empty());
        assert_eq!(alpha(&Rgb32f::new(3, 2), 1.0, 0.5), vec![0.0; 6]);
    }

    #[test]
    fn alpha_covers_the_sky_at_any_size_and_agrees_between_sizes() {
        // linear working-space picture: blue sky over foliage
        let make = |w: usize, h: usize| {
            Rgb32f::from_fn(w, h, |x, y| {
                let (fx, fy) = (x * W / w, y * H / h);
                let c = if fy < 40 { blue_sky(fx, fy) } else { foliage(fx, fy) };
                // linear Rec.2020 values whose display mapping (`v / (1 + v)`, then sRGB) gives `c` back
                let lin = c.map(|v| lightcraft_color::transfer::srgb_to_linear(v.clamp(0.0, 1.0)).min(0.98));
                let lin = lin.map(|s| s / (1.0 - s));
                lightcraft_color::SRGB.to_space(&lightcraft_color::REC2020).apply_f32(lin)
            })
        };
        let (small, big) = (make(256, 192), make(1000, 750));
        let (a, b) = (alpha(&small, 1.0, DEFAULT_THRESHOLD), alpha(&big, 1.0, DEFAULT_THRESHOLD));
        let mean = |v: &[f32], w: usize, rows: std::ops::Range<usize>| {
            let s: f32 = rows.clone().map(|y| v[y * w..(y + 1) * w].iter().sum::<f32>()).sum();
            s / (rows.len() * w) as f32
        };
        assert!(mean(&a, 256, 5..70) > 0.9 && mean(&a, 256, 110..192) < 0.05, "small {} {}", mean(&a, 256, 5..70), mean(&a, 256, 110..192));
        assert!(mean(&b, 1000, 20..270) > 0.9 && mean(&b, 1000, 420..750) < 0.05, "big");
        assert!(a.iter().chain(&b).all(|v| (0.0..=1.0).contains(v)));
    }
}
