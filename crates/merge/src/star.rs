//! Star Stack and Star Trails merge.
//!
//! - **Star Stack** ([`merge_star_stack`]): sigma-clipped mean across aligned frames, which
//!   averages out thermal noise and tracking errors while preserving star colour and detail.
//! - **Star Trails** ([`merge_star_trails`]): per-pixel maximum, which accumulates light-trail
//!   arcs across all frames.
//!
//! References:
//! - Sigma clipping: Chauvenet (1863); see also Zackay et al. 2016 (optimal coaddition).
//! - Star trails: standard astrophotography technique — no specific paper.
#![forbid(unsafe_code)]

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use lightcraft_raster::Rgb32f;

use crate::{MergeError, Progress, Result, align, check, features};

// ── Options ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StarStackOptions {
    /// Sigma for iterative sigma-clipping; 2.5 is a good default.
    pub sigma: f64,
    /// Number of sigma-clipping iterations.
    pub iterations: u32,
    /// Whether to auto-align frames before stacking.
    pub auto_align: bool,
}

impl Default for StarStackOptions {
    fn default() -> Self {
        Self { sigma: 2.5, iterations: 3, auto_align: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StarTrailsOptions {
    /// Whether to auto-align frames before accumulating.
    pub auto_align: bool,
}

impl Default for StarTrailsOptions {
    fn default() -> Self {
        Self { auto_align: true }
    }
}

// ── Result ────────────────────────────────────────────────────────────────────

pub struct StarResult {
    pub image: Rgb32f,
    /// Number of frames that contributed to the output (after alignment).
    pub frames_used: usize,
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Align all frames to the first one.  Returns the aligned images in input order; frames that
/// could not be aligned are kept as-is (identity homography).
pub(crate) fn align_frames(imgs: &[Rgb32f], do_align: bool, progress: &Progress, p0: f32, p1: f32) -> Vec<Rgb32f> {
    if !do_align || imgs.len() < 2 {
        return imgs.to_vec();
    }
    let n = imgs.len();
    // Build work images + features for every frame.
    let work: Vec<(_, f64)> = imgs.iter().map(|img| align::work_image(img, 1.0_f32, align::WORK_EDGE)).collect();
    let feats: Vec<_> = work.iter().map(|(plane, _)| features::detect(plane, 1500)).collect();
    let ref_work = &work[0];
    let ref_feats = &feats[0];
    let (ref_w, ref_h) = (imgs[0].width, imgs[0].height);

    let mut out: Vec<Rgb32f> = Vec::with_capacity(n);
    out.push(imgs[0].clone());

    for i in 1..n {
        let _ = progress(p0 + (p1 - p0) * i as f32 / n as f32, "Aligning frames");
        let al = align::align_pair(ref_feats, &feats[i], ref_work.1, ref_w, ref_h, i as u64);
        let (warped, _valid) = align::warp(&imgs[i], &al.h);
        out.push(warped);
    }
    out
}

/// Iterative sigma-clipped mean of `values`.
///
/// Each value is judged against the mean and deviation of the *other* values (leave-one-out): an outlier that is
/// part of its own yardstick inflates the deviation and escapes (with 5 frames no value can ever lie beyond 1.8
/// standard deviations of a set that contains it), which left hot pixels and satellite trails in short stacks.
fn clipped_mean(values: &[f32], sigma: f64, iters: u32) -> f32 {
    // non-finite samples (a damaged frame) take no part
    let finite: Vec<f64> = values.iter().filter(|x| x.is_finite()).map(|&x| f64::from(x)).collect();
    if finite.is_empty() {
        return 0.0;
    }
    let mut v = finite.clone();
    for _ in 0..iters {
        let n = v.len();
        if n < 3 {
            break;
        }
        let sum: f64 = v.iter().sum();
        let sum_sq: f64 = v.iter().map(|x| x * x).sum();
        let others = (n - 1) as f64;
        let before = n;
        v.retain(|&x| {
            let mean = (sum - x) / others;
            let var = ((sum_sq - x * x) / others - mean * mean).max(0.0);
            // identical others (no spread at all): anything visibly different is an outlier
            (x - mean).abs() <= (sigma * var.sqrt()).max(1e-6)
        });
        if v.len() == before {
            break;
        }
    }
    if v.is_empty() {
        // all clipped: fall back to the raw mean
        return (finite.iter().sum::<f64>() / finite.len() as f64) as f32;
    }
    (v.iter().sum::<f64>() / v.len() as f64) as f32
}

// ── Public functions ──────────────────────────────────────────────────────────

/// Merge a set of astrophotography frames using sigma-clipped stacking.
///
/// Aligns frames to the first one (if `opts.auto_align`), then computes an iterative
/// sigma-clipped mean per pixel per channel.
pub fn merge_star_stack(imgs: Vec<Rgb32f>, opts: &StarStackOptions, progress: &Progress) -> Result<StarResult> {
    check(progress, 0.0, "Star Stack")?;
    if imgs.is_empty() {
        return Err(MergeError::TooFew(0, 2));
    }
    let (w, h) = (imgs[0].width, imgs[0].height);
    let n = imgs.len();

    let aligned = align_frames(&imgs, opts.auto_align, progress, 0.0, 0.5);
    check(progress, 0.5, "Stacking")?;

    let sigma = opts.sigma;
    let iters = opts.iterations;
    let npix = w * h;

    let data: Vec<[f32; 3]> = (0..npix)
        .into_par_iter()
        .map(|pidx| {
            let px = pidx % w;
            let py = pidx / w;
            let mut bufs: [Vec<f32>; 3] = [Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n)];
            for img in &aligned {
                let c = img.get(px, py);
                bufs[0].push(c[0]);
                bufs[1].push(c[1]);
                bufs[2].push(c[2]);
            }
            [clipped_mean(&bufs[0], sigma, iters), clipped_mean(&bufs[1], sigma, iters), clipped_mean(&bufs[2], sigma, iters)]
        })
        .collect();

    let mut output = Rgb32f::new(w, h);
    for (pidx, px_val) in data.into_iter().enumerate() {
        let px = pidx % w;
        let py = pidx / w;
        output.set(px, py, px_val);
    }

    check(progress, 1.0, "Done")?;
    Ok(StarResult { image: output, frames_used: n })
}

/// Accumulate star trails: per-pixel maximum across (optionally aligned) frames.
pub fn merge_star_trails(imgs: Vec<Rgb32f>, opts: &StarTrailsOptions, progress: &Progress) -> Result<StarResult> {
    check(progress, 0.0, "Star Trails")?;
    if imgs.is_empty() {
        return Err(MergeError::TooFew(0, 2));
    }
    let (w, h) = (imgs[0].width, imgs[0].height);
    let n = imgs.len();

    let aligned = align_frames(&imgs, opts.auto_align, progress, 0.0, 0.4);
    check(progress, 0.4, "Accumulating trails")?;

    let npix = w * h;
    let data: Vec<[f32; 3]> = (0..npix)
        .into_par_iter()
        .map(|pidx| {
            let px = pidx % w;
            let py = pidx / w;
            let mut r = 0.0f32;
            let mut g = 0.0f32;
            let mut b = 0.0f32;
            for img in &aligned {
                let c = img.get(px, py);
                r = r.max(c[0]);
                g = g.max(c[1]);
                b = b.max(c[2]);
            }
            [r, g, b]
        })
        .collect();

    let mut output = Rgb32f::new(w, h);
    for (pidx, px_val) in data.into_iter().enumerate() {
        let px = pidx % w;
        let py = pidx / w;
        output.set(px, py, px_val);
    }

    check(progress, 1.0, "Done")?;
    Ok(StarResult { image: output, frames_used: n })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcraft_raster::Rgb32f;

    fn solid(w: usize, h: usize, val: [f32; 3]) -> Rgb32f {
        let mut img = Rgb32f::new(w, h);
        for y in 0..h {
            for x in 0..w {
                img.set(x, y, val);
            }
        }
        img
    }

    #[test]
    fn stack_mean_no_noise() {
        let imgs = vec![solid(4, 4, [0.5, 0.4, 0.3]); 5];
        let opts = StarStackOptions { sigma: 2.5, iterations: 3, auto_align: false };
        let r = merge_star_stack(imgs, &opts, &crate::no_progress).unwrap();
        let c = r.image.get(2, 2);
        assert!((c[0] - 0.5).abs() < 1e-4, "R: {}", c[0]);
        assert!((c[1] - 0.4).abs() < 1e-4, "G: {}", c[1]);
    }

    #[test]
    fn trails_max() {
        let mut imgs = vec![solid(4, 4, [0.1, 0.1, 0.1]); 4];
        imgs[2] = solid(4, 4, [0.8, 0.9, 0.7]);
        let opts = StarTrailsOptions { auto_align: false };
        let r = merge_star_trails(imgs, &opts, &crate::no_progress).unwrap();
        let c = r.image.get(1, 1);
        assert!((c[0] - 0.8).abs() < 1e-4, "R: {}", c[0]);
        assert!((c[1] - 0.9).abs() < 1e-4, "G: {}", c[1]);
    }

    #[test]
    fn too_few_returns_error() {
        let opts = StarStackOptions::default();
        assert!(merge_star_stack(vec![], &opts, &crate::no_progress).is_err());
    }

    #[test]
    fn clipped_mean_keeps_clean_noise_and_handles_tiny_sets() {
        // ordinary noise around 0.5 is not clipped away: the result stays at the mean
        let noisy = [0.48f32, 0.52, 0.5, 0.49, 0.51, 0.5, 0.47, 0.53];
        assert!((super::clipped_mean(&noisy, 2.5, 3) - 0.5).abs() < 0.01);
        // one and two values are averaged as they are, an empty set is zero
        assert_eq!(super::clipped_mean(&[0.7], 2.5, 3), 0.7);
        assert!((super::clipped_mean(&[0.2, 0.8], 2.5, 3) - 0.5).abs() < 1e-6);
        assert_eq!(super::clipped_mean(&[], 2.5, 3), 0.0);
        // a spike among identical values, and non-finite input, never produce NaN
        assert!((super::clipped_mean(&[0.3, 0.3, 0.3, 0.3, 0.9], 2.5, 3) - 0.3).abs() < 1e-6);
        assert!((super::clipped_mean(&[0.3, f32::NAN, 0.3, f32::INFINITY], 2.5, 3) - 0.3).abs() < 1e-6);
        assert_eq!(super::clipped_mean(&[f32::NAN, f32::NEG_INFINITY], 2.5, 3), 0.0);
    }

    #[test]
    fn clipped_mean_removes_outlier() {
        let vals = vec![1.0f32, 1.0, 1.0, 1.0, 10.0]; // 10.0 is a spike
        let mean = super::clipped_mean(&vals, 2.5, 3);
        assert!(mean < 2.0, "mean should be near 1.0, got {mean}");
    }
}
