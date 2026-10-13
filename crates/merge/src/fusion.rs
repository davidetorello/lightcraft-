//! Exposure fusion: a bracketed set blended into one display-ready image, with no radiance map and no tone mapping.
//!
//! Each frame gets a per-pixel weight from three quality measures (contrast, saturation and well-exposedness),
//! the weights are normalised across the frames, and the frames are blended per pyramid level (a Laplacian
//! pyramid of every frame weighted by a Gaussian pyramid of its weights), so that seams between frames stay
//! invisible. Unlike [`crate::hdr`] the result is not scene-linear: inputs and output are display-referred
//! (gamma-encoded) values in `0..=1`.
//!
//! Reference: T. Mertens, J. Kautz, F. Van Reeth, "Exposure Fusion", Pacific Graphics 2007. Pyramids follow
//! Burt & Adelson, "The Laplacian Pyramid as a Compact Image Code", 1983. Own implementation of the published
//! method; no code from other projects was consulted.
#![forbid(unsafe_code)]

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use lightcraft_raster::Rgb32f;

use crate::{MergeError, Progress, Result, check, star};

/// Smallest side, in pixels, of the coarsest pyramid level.
const MIN_TOP_SIDE: usize = 4;
/// Most pyramid levels (a 100 MP frame needs about 12 to reach [`MIN_TOP_SIDE`]).
const MAX_LEVELS: usize = 14;
/// Added to the weights so that a pixel no frame likes still blends (to the plain average).
const WEIGHT_FLOOR: f32 = 1e-12;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FusionOptions {
    /// Exponent of the contrast measure (edges, texture); 0 switches it off.
    pub contrast: f64,
    /// Exponent of the saturation measure (colour); 0 switches it off.
    pub saturation: f64,
    /// Exponent of the well-exposedness measure (close to mid-grey); 0 switches it off.
    pub exposure: f64,
    /// Width of the well-exposedness bell around 0.5; 0.2 is the paper's value.
    pub sigma: f64,
    /// Align the frames to the first one before blending.
    pub auto_align: bool,
}

impl Default for FusionOptions {
    fn default() -> Self {
        Self { contrast: 1.0, saturation: 1.0, exposure: 1.0, sigma: 0.2, auto_align: true }
    }
}

pub struct FusionResult {
    pub image: Rgb32f,
    /// Number of frames blended.
    pub frames_used: usize,
}

/// A float image with three interleaved channels.
struct Plane3 {
    w: usize,
    h: usize,
    px: Vec<[f32; 3]>,
}

/// A one-channel float image.
struct Plane1 {
    w: usize,
    h: usize,
    px: Vec<f32>,
}

fn finite01(v: f32) -> f32 {
    if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 }
}

/// The frame with every sample made finite and clamped to `0..=1`.
fn sanitise(img: &Rgb32f) -> Plane3 {
    Plane3 { w: img.width, h: img.height, px: img.data.iter().map(|p| [finite01(p[0]), finite01(p[1]), finite01(p[2])]).collect() }
}

/// The index of the sample at (`x`, `y`) with the coordinates clamped to the image.
fn at(w: usize, h: usize, x: isize, y: isize) -> usize {
    let xc = x.clamp(0, w as isize - 1) as usize;
    let yc = y.clamp(0, h as isize - 1) as usize;
    yc * w + xc
}

/// Per-pixel quality of a frame: `contrast^wc · saturation^ws · exposedness^we`.
fn weights(img: &Plane3, opts: &FusionOptions) -> Plane1 {
    let (w, h) = (img.w, img.h);
    let grey: Vec<f32> = img.px.iter().map(|p| 0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2]).collect();
    let sigma = if opts.sigma.is_finite() && opts.sigma > 1e-3 { opts.sigma as f32 } else { 0.2 };
    let two_sigma_sq = 2.0 * sigma * sigma;
    let (wc, ws, we) = (opts.contrast.max(0.0) as f32, opts.saturation.max(0.0) as f32, opts.exposure.max(0.0) as f32);
    let px: Vec<f32> = (0..w * h)
        .into_par_iter()
        .map(|i| {
            let (x, y) = ((i % w) as isize, (i / w) as isize);
            let g = |dx: isize, dy: isize| grey.get(at(w, h, x + dx, y + dy)).copied().unwrap_or(0.0);
            // |Laplacian| of the grey image (4-neighbour kernel)
            let contrast = (g(-1, 0) + g(1, 0) + g(0, -1) + g(0, 1) - 4.0 * g(0, 0)).abs();
            let p = img.px.get(i).copied().unwrap_or([0.0; 3]);
            let mean = (p[0] + p[1] + p[2]) / 3.0;
            let saturation = (((p[0] - mean).powi(2) + (p[1] - mean).powi(2) + (p[2] - mean).powi(2)) / 3.0).sqrt();
            let exposedness: f32 = p.iter().map(|&v| (-(v - 0.5).powi(2) / two_sigma_sq).exp()).product();
            // a measure with exponent 0 is 1; a zero measure with a positive exponent is the floor
            let m = |v: f32, e: f32| if e == 0.0 { 1.0 } else { v.max(1e-6).powf(e) };
            m(contrast, wc) * m(saturation, ws) * m(exposedness, we) + WEIGHT_FLOOR
        })
        .collect();
    Plane1 { w, h, px }
}

/// Blur with the 5-tap binomial kernel and keep every second sample.
fn reduce<const N: usize>(src: &[[f32; N]], w: usize, h: usize) -> (Vec<[f32; N]>, usize, usize) {
    const K: [f32; 5] = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];
    let (w2, h2) = (w.div_ceil(2), h.div_ceil(2));
    // horizontal pass at the kept columns, all rows
    let tmp: Vec<[f32; N]> = (0..w2 * h)
        .into_par_iter()
        .map(|i| {
            let (xo, y) = ((i % w2) as isize * 2, (i / w2) as isize);
            let mut acc = [0.0f32; N];
            for (t, k) in K.iter().enumerate() {
                let s = src.get(at(w, h, xo + t as isize - 2, y)).copied().unwrap_or([0.0; N]);
                for c in 0..N {
                    acc[c] += k * s[c];
                }
            }
            acc
        })
        .collect();
    // vertical pass at the kept rows
    let out: Vec<[f32; N]> = (0..w2 * h2)
        .into_par_iter()
        .map(|i| {
            let (x, yo) = ((i % w2) as isize, (i / w2) as isize * 2);
            let mut acc = [0.0f32; N];
            for (t, k) in K.iter().enumerate() {
                let s = tmp.get(at(w2, h, x, yo + t as isize - 2)).copied().unwrap_or([0.0; N]);
                for c in 0..N {
                    acc[c] += k * s[c];
                }
            }
            acc
        })
        .collect();
    (out, w2, h2)
}

/// Bilinear enlargement of `src` (`sw × sh`) to `tw × th`.
fn expand<const N: usize>(src: &[[f32; N]], sw: usize, sh: usize, tw: usize, th: usize) -> Vec<[f32; N]> {
    (0..tw * th)
        .into_par_iter()
        .map(|i| {
            let (x, y) = (i % tw, i / tw);
            let fx = ((x as f32 + 0.5) * sw as f32 / tw as f32 - 0.5).clamp(0.0, sw as f32 - 1.0);
            let fy = ((y as f32 + 0.5) * sh as f32 / th as f32 - 0.5).clamp(0.0, sh as f32 - 1.0);
            let (x0, y0) = (fx.floor() as isize, fy.floor() as isize);
            let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
            let s = |dx: isize, dy: isize| src.get(at(sw, sh, x0 + dx, y0 + dy)).copied().unwrap_or([0.0; N]);
            let (a, b, c, d) = (s(0, 0), s(1, 0), s(0, 1), s(1, 1));
            let mut out = [0.0f32; N];
            for ch in 0..N {
                let top = a[ch] + (b[ch] - a[ch]) * tx;
                let bot = c[ch] + (d[ch] - c[ch]) * tx;
                out[ch] = top + (bot - top) * ty;
            }
            out
        })
        .collect()
}

/// The pyramid levels needed for a `w × h` image: down to [`MIN_TOP_SIDE`] on the shorter side.
fn level_count(w: usize, h: usize) -> usize {
    let mut n = 1;
    let mut side = w.min(h);
    while side.div_ceil(2) >= MIN_TOP_SIDE && n < MAX_LEVELS {
        side = side.div_ceil(2);
        n += 1;
    }
    n
}

/// Gaussian pyramid: `levels` images, each with its size, from full resolution down.
fn gaussian<const N: usize>(base: Vec<[f32; N]>, w: usize, h: usize, levels: usize) -> Vec<(Vec<[f32; N]>, usize, usize)> {
    let mut pyr = vec![(base, w, h)];
    for _ in 1..levels {
        let next = match pyr.last() {
            Some((p, pw, ph)) => reduce(p, *pw, *ph),
            None => break,
        };
        pyr.push(next);
    }
    pyr
}

/// Laplacian pyramid from a Gaussian one: each level minus the enlarged next level; the top level stays as it is.
fn laplacian(g: Vec<(Vec<[f32; 3]>, usize, usize)>) -> Vec<(Vec<[f32; 3]>, usize, usize)> {
    let mut out = Vec::with_capacity(g.len());
    for i in 0..g.len() {
        let Some((cur, w, h)) = g.get(i) else { break };
        match g.get(i + 1) {
            Some((next, nw, nh)) => {
                let up = expand(next, *nw, *nh, *w, *h);
                let diff: Vec<[f32; 3]> = cur.iter().zip(&up).map(|(a, b)| [a[0] - b[0], a[1] - b[1], a[2] - b[2]]).collect();
                out.push((diff, *w, *h));
            }
            None => out.push((cur.clone(), *w, *h)),
        }
    }
    out
}

/// Blend aligned, equally sized frames. See the module documentation; the frames are display-referred `0..=1`.
///
/// Needs at least two frames of one size. `progress` is called as the work advances and may cancel it.
pub fn merge_exposure_fusion(imgs: Vec<Rgb32f>, opts: &FusionOptions, progress: &Progress) -> Result<FusionResult> {
    check(progress, 0.0, "Exposure fusion")?;
    let n = imgs.len();
    if n < 2 {
        return Err(MergeError::TooFew(n, 2));
    }
    let (w, h) = imgs.first().map_or((0, 0), |i| (i.width, i.height));
    if w == 0 || h == 0 {
        return Err(MergeError::Mismatch("an empty photo cannot be fused".into()));
    }
    if imgs.iter().any(|i| i.width != w || i.height != h || i.data.len() != w * h) {
        return Err(MergeError::Mismatch("the photos must all have the same size".into()));
    }

    let aligned = star::align_frames(&imgs, opts.auto_align, progress, 0.0, 0.3);
    drop(imgs);
    check(progress, 0.3, "Weighing frames")?;
    let frames: Vec<Plane3> = aligned.iter().map(sanitise).collect();
    drop(aligned);

    // quality weights, normalised across the frames
    let mut ws: Vec<Plane1> = frames.iter().map(|f| weights(f, opts)).collect();
    let total: Vec<f32> = (0..w * h).into_par_iter().map(|i| ws.iter().map(|p| p.px.get(i).copied().unwrap_or(0.0)).sum::<f32>()).collect();
    for p in &mut ws {
        p.px.par_iter_mut().zip(total.par_iter()).for_each(|(v, t)| *v /= t.max(WEIGHT_FLOOR));
    }
    drop(total);

    // blend per pyramid level
    let levels = level_count(w, h);
    let mut blended: Vec<(Vec<[f32; 3]>, usize, usize)> = Vec::new();
    for (k, (frame, weight)) in frames.into_iter().zip(ws).enumerate() {
        check(progress, 0.3 + 0.6 * k as f32 / n as f32, "Blending")?;
        let lp = laplacian(gaussian(frame.px, frame.w, frame.h, levels));
        let gw = gaussian(weight.px.into_iter().map(|v| [v]).collect::<Vec<[f32; 1]>>(), weight.w, weight.h, levels);
        if blended.is_empty() {
            blended = lp.iter().map(|(p, pw, ph)| (vec![[0.0f32; 3]; p.len()], *pw, *ph)).collect();
        }
        for ((acc, _, _), ((layer, _, _), (wl, _, _))) in blended.iter_mut().zip(lp.iter().zip(gw.iter())) {
            acc.par_iter_mut().zip(layer.par_iter().zip(wl.par_iter())).for_each(|(a, (l, wt))| {
                a[0] += l[0] * wt[0];
                a[1] += l[1] * wt[0];
                a[2] += l[2] * wt[0];
            });
        }
    }
    check(progress, 0.9, "Collapsing")?;

    // collapse from the coarsest level up
    let mut cur: Option<(Vec<[f32; 3]>, usize, usize)> = None;
    for (layer, lw, lh) in blended.into_iter().rev() {
        cur = Some(match cur {
            None => (layer, lw, lh),
            Some((coarse, cw, ch)) => {
                let up = expand(&coarse, cw, ch, lw, lh);
                let sum: Vec<[f32; 3]> = layer.iter().zip(&up).map(|(a, b)| [a[0] + b[0], a[1] + b[1], a[2] + b[2]]).collect();
                (sum, lw, lh)
            }
        });
    }
    let Some((data, ow, oh)) = cur else { return Err(MergeError::Mismatch("nothing to fuse".into())) };
    let mut image = Rgb32f::new(ow, oh);
    for (dst, src) in image.data.iter_mut().zip(data) {
        *dst = [finite01(src[0]), finite01(src[1]), finite01(src[2])];
    }
    check(progress, 1.0, "Done")?;
    Ok(FusionResult { image, frames_used: n })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> FusionOptions {
        FusionOptions { auto_align: false, ..FusionOptions::default() }
    }

    fn image(w: usize, h: usize, f: impl Fn(usize, usize) -> [f32; 3]) -> Rgb32f {
        Rgb32f::from_fn(w, h, f)
    }

    /// A scene of linear brightness 0.02 (left) to 1.6 (right) with a little texture and colour.
    fn scene(x: usize, y: usize, w: usize) -> [f32; 3] {
        let l = 0.02 + 1.58 * x as f32 / (w - 1) as f32;
        let t = 1.0 + 0.08 * (((x / 3 + y / 3) % 2) as f32 - 0.5);
        [l * t, l * t * 0.9, l * t * 0.8]
    }

    #[test]
    fn identical_frames_come_back_unchanged() {
        let a = image(40, 30, |x, y| [x as f32 / 40.0, y as f32 / 30.0, 0.5]);
        let r = merge_exposure_fusion(vec![a.clone(), a.clone(), a.clone()], &opts(), &crate::no_progress).unwrap();
        assert_eq!((r.image.width, r.image.height, r.frames_used), (40, 30, 3));
        for (o, i) in r.image.data.iter().zip(&a.data) {
            for c in 0..3 {
                assert!((o[c] - i[c]).abs() < 2e-3, "{o:?} vs {i:?}");
            }
        }
    }

    #[test]
    fn each_region_follows_the_frame_that_exposes_it_well() {
        let w = 96;
        let dark = image(w, 48, |x, y| scene(x, y, w).map(|v| (v * 0.5).clamp(0.0, 1.0)));
        let bright = image(w, 48, |x, y| scene(x, y, w).map(|v| (v * 4.0).clamp(0.0, 1.0)));
        let r = merge_exposure_fusion(vec![dark.clone(), bright.clone()], &opts(), &crate::no_progress).unwrap();
        // deep shadows (left): the dark frame is nearly black, the bright one shows them
        let (d, b, f) = (dark.get(4, 24), bright.get(4, 24), r.image.get(4, 24));
        assert!(f[1] > d[1] + 0.5 * (b[1] - d[1]), "shadows follow the bright frame: dark {d:?}, bright {b:?}, fused {f:?}");
        // highlights (right): the bright frame is clipped to white, the dark one keeps them
        let (d, b, f) = (dark.get(w - 5, 24), bright.get(w - 5, 24), r.image.get(w - 5, 24));
        assert!(f[1] < b[1] - 0.5 * (b[1] - d[1]), "highlights follow the dark frame: dark {d:?}, bright {b:?}, fused {f:?}");
        assert!(r.image.data.iter().flatten().all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
    }

    #[test]
    fn odd_and_tiny_sizes_do_not_panic() {
        for (w, h) in [(1, 1), (2, 3), (5, 1), (37, 23), (64, 3)] {
            let a = image(w, h, |x, y| [(x % 3) as f32 / 3.0, (y % 2) as f32 / 2.0, 0.4]);
            let b = image(w, h, |x, y| [1.0 - (x % 3) as f32 / 3.0, 0.2, (y % 2) as f32]);
            let r = merge_exposure_fusion(vec![a, b], &opts(), &crate::no_progress).unwrap();
            assert_eq!((r.image.width, r.image.height), (w, h));
        }
    }

    #[test]
    fn bad_input_is_an_error_not_a_panic() {
        let a = image(8, 8, |_, _| [0.5; 3]);
        let big = image(9, 8, |_, _| [0.5; 3]);
        assert!(matches!(merge_exposure_fusion(vec![a.clone()], &opts(), &crate::no_progress), Err(MergeError::TooFew(1, 2))));
        assert!(matches!(merge_exposure_fusion(vec![a.clone(), big], &opts(), &crate::no_progress), Err(MergeError::Mismatch(_))));
        assert!(matches!(merge_exposure_fusion(vec![], &opts(), &crate::no_progress), Err(MergeError::TooFew(0, 2))));
        let empty = image(0, 0, |_, _| [0.0; 3]);
        assert!(merge_exposure_fusion(vec![empty.clone(), empty], &opts(), &crate::no_progress).is_err());
    }

    #[test]
    fn non_finite_samples_and_extreme_options_stay_finite() {
        let a = image(16, 16, |x, _| if x == 3 { [f32::NAN, f32::INFINITY, -5.0] } else { [0.3; 3] });
        let b = image(16, 16, |_, y| if y == 5 { [7.0, f32::NEG_INFINITY, 0.1] } else { [0.8; 3] });
        let wild = FusionOptions { contrast: -3.0, saturation: 1e9, exposure: f64::NAN, sigma: 0.0, auto_align: false };
        let r = merge_exposure_fusion(vec![a, b], &wild, &crate::no_progress).unwrap();
        assert!(r.image.data.iter().flatten().all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
    }

    #[test]
    fn cancelling_stops_the_merge() {
        let a = image(32, 32, |_, _| [0.4; 3]);
        let r = merge_exposure_fusion(vec![a.clone(), a], &opts(), &|_, _| false);
        assert!(matches!(r, Err(MergeError::Cancelled)));
    }

    #[test]
    fn pyramid_levels_reach_the_minimum_side() {
        assert_eq!(level_count(1, 1), 1);
        assert_eq!(level_count(8, 8), 2);
        assert!(level_count(6000, 4000) <= MAX_LEVELS);
        assert_eq!(level_count(6000, 4000), 11);
    }
}
