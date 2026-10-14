//! Subject, Background and People masks from two small segmentation models (`lightcraft-salient`,
//! cargo feature `salient`; the desktop app enables it).
//!
//! The models are part of the build (see `docs/ai-masks.md`), so nothing is downloaded. The model
//! sees the photo as developed, uncropped and without masks at 1024 px, and its result is stored
//! with the mask as plain data ([`SegMask`] logits over the whole photo): renders, exports and the
//! web build never need the model, and the mask survives a move to a build without it. Without the
//! models (a development build) the heuristics stay in charge.
//!
//! In the desktop app ([`crate::segment::Segmenter::background`]) the mask is added at once with the
//! heuristic standing in, a thread finds the subject (a row in the activity stack) and
//! [`Session::segment_poll`] swaps the result in; CLI, MCP and tests wait for it.

use lightcraft_catalog::PhotoId;
use lightcraft_develop::{MaskShape, SegMask};

use crate::Session;
use crate::segment::Polled;

/// Long edge of the image the models see.
pub const EDGE: usize = 1024;
/// Side of the grid a selection is stored on (the models' own outputs are 320 and 1024 px).
pub const SEG_SIDE: usize = 384;

/// Whether this build can find subjects and people with a model.
pub fn ready() -> bool {
    #[cfg(feature = "salient")]
    return lightcraft_salient::available();
    #[cfg(not(feature = "salient"))]
    false
}

/// The `class` of a [`MaskShape::Salient`] a shape becomes when a model finds it (`None`: the shape
/// is not one a model handles; a People mask for particular parts stays a heuristic).
pub fn class_of(shape: &MaskShape) -> Option<&'static str> {
    match shape {
        MaskShape::Subject => Some("subject"),
        MaskShape::Background => Some("background"),
        MaskShape::People { parts, .. } if parts.is_empty() => Some("people"),
        _ => None,
    }
}

/// The static name of a class (`subject`, `background`, `people`; anything else is `subject`).
pub fn class_of_name(class: &str) -> &'static str {
    match class {
        "background" => "background",
        "people" => "people",
        _ => "subject",
    }
}

/// Bilinear resample of a `from × from` grid of values to `to × to`.
#[cfg_attr(not(feature = "salient"), allow(dead_code))]
pub(crate) fn resample(grid: &[f32], from: usize, to: usize) -> Vec<f32> {
    if from == 0 || to == 0 || grid.len() != from * from {
        return vec![0.0; to * to];
    }
    let scale = from as f32 / to as f32;
    let mut out = Vec::with_capacity(to * to);
    for y in 0..to {
        let fy = ((y as f32 + 0.5) * scale - 0.5).clamp(0.0, (from - 1) as f32);
        let (y0, wy) = (fy.floor() as usize, fy.fract());
        let y1 = (y0 + 1).min(from - 1);
        for x in 0..to {
            let fx = ((x as f32 + 0.5) * scale - 0.5).clamp(0.0, (from - 1) as f32);
            let (x0, wx) = (fx.floor() as usize, fx.fract());
            let x1 = (x0 + 1).min(from - 1);
            let at = |xx: usize, yy: usize| grid.get(yy * from + xx).copied().unwrap_or(0.0);
            let top = at(x0, y0) * (1.0 - wx) + at(x1, y0) * wx;
            let bottom = at(x0, y1) * (1.0 - wx) + at(x1, y1) * wx;
            out.push(top * (1.0 - wy) + bottom * wy);
        }
    }
    out
}

/// A probability grid (`side × side`, 0..1) as a stored selection: its logits, clamped to ±4.6 (1 %).
#[cfg_attr(not(feature = "salient"), allow(dead_code))]
pub(crate) fn to_seg(prob: &[f32], side: usize) -> SegMask {
    let logits: Vec<f32> = prob
        .iter()
        .map(|p| {
            let p = if p.is_finite() { p.clamp(0.01, 0.99) } else { 0.01 };
            (p / (1.0 - p)).ln()
        })
        .collect();
    SegMask::from_logits(side, &logits)
}

/// The union of `grids` (all `side × side`), cell by cell.
#[cfg_attr(not(feature = "salient"), allow(dead_code))]
pub(crate) fn union(grids: &[Vec<f32>]) -> Vec<f32> {
    let n = grids.iter().map(Vec::len).max().unwrap_or(0);
    (0..n).map(|i| grids.iter().filter_map(|g| g.get(i)).copied().fold(0.0, f32::max)).collect()
}

/// One finished selection, from a thread to [`Session::segment_poll`].
#[cfg(feature = "salient")]
pub(crate) struct Outcome {
    photo: PhotoId,
    mask: u32,
    comp: usize,
    class: String,
    result: Result<SegMask, String>,
}

/// The channel the threads answer on.
pub struct Salient {
    #[cfg(feature = "salient")]
    results: (std::sync::mpsc::Sender<Outcome>, std::sync::mpsc::Receiver<Outcome>),
}

impl Default for Salient {
    fn default() -> Self {
        Salient {
            #[cfg(feature = "salient")]
            results: std::sync::mpsc::channel(),
        }
    }
}

#[cfg(feature = "salient")]
mod run {
    use std::sync::{Arc, Mutex, PoisonError};

    use lightcraft_salient::{Class, Model};

    use super::*;

    /// A model, loaded on first use and kept (loading optimizes its graph: seconds).
    fn model(class: Class) -> Result<Arc<Model>, String> {
        static LOADED: Mutex<Vec<(Class, Arc<Model>)>> = Mutex::new(Vec::new());
        let mut loaded = LOADED.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((_, m)) = loaded.iter().find(|(c, _)| *c == class) {
            return Ok(m.clone());
        }
        let m = Arc::new(Model::embedded(class).map_err(|e| e.to_string())?);
        loaded.push((class, m.clone()));
        Ok(m)
    }

    /// The selection of `class` (`subject`, `background` or `people`) in 8-bit RGB (`w × h`): people
    /// are the person model's; a subject is the general model's together with the person model's
    /// (whose clothes the general one takes for background).
    pub(crate) fn compute(rgb: &[u8], w: usize, h: usize, class: &str) -> Result<SegMask, String> {
        let person = model(Class::Person)?.matte(rgb, w, h).map_err(|e| e.to_string())?;
        let person = resample(&person.prob, person.side, SEG_SIDE);
        let grid = if class == "people" {
            person
        } else {
            let general = model(Class::General)?.matte(rgb, w, h).map_err(|e| e.to_string())?;
            union(&[resample(&general.prob, general.side, SEG_SIDE), person])
        };
        Ok(to_seg(&grid, SEG_SIDE))
    }

    /// Render the photo as the model sees it and find `class` in it.
    pub(super) fn find(job: crate::media::RenderJob, class: &str) -> Result<SegMask, String> {
        let img = match job.run().rendered {
            Ok(r) => r.image,
            Err(e) => return Err(format!("couldn't render the photo to find its subject: {e}")),
        };
        let rgb: Vec<u8> = img.data.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
        compute(&rgb, img.width, img.height, class)
    }
}

impl Session {
    /// The render job of photo `id` as the model sees it (developed, uncropped, no masks).
    #[cfg(feature = "salient")]
    fn salient_job(&mut self, id: PhotoId) -> Result<crate::media::RenderJob, String> {
        let mut d = (*self.catalog.photo(id).ok_or("no such photo")?.develop).clone();
        d.masks.clear();
        self.preview_job(id, EDGE, EDGE, false, &d).ok_or_else(|| "no such photo".to_string())
    }

    /// Find `class` (`subject`, `background` or `people`) in photo `id`, waiting for the result.
    pub(crate) fn salient_seg(&mut self, id: PhotoId, class: &str) -> Result<SegMask, String> {
        #[cfg(feature = "salient")]
        {
            let job = self.salient_job(id)?;
            let _task = self.activity.start("analysis", label(class), crate::activity::Cancel::No);
            run::find(job, class)
        }
        #[cfg(not(feature = "salient"))]
        {
            let _ = (id, class);
            Err("AI masks are not available in this build".into())
        }
    }

    /// Find `class` in photo `id` on a thread; [`Session::segment_poll`] puts the result into
    /// component `comp` of mask `mask` (a [`MaskShape::Salient`] without a selection yet).
    pub(crate) fn salient_later(&mut self, id: PhotoId, mask: u32, comp: usize, class: &str) -> Result<(), String> {
        #[cfg(feature = "salient")]
        {
            let job = self.salient_job(id)?;
            let task = self.activity.start("analysis", label(class), crate::activity::Cancel::No);
            let tx = self.salient.results.0.clone();
            let class = class.to_string();
            std::thread::Builder::new()
                .name("salient".into())
                .spawn(move || {
                    let result = run::find(job, &class);
                    drop(task);
                    let _ = tx.send(Outcome { photo: id, mask, comp, class, result });
                })
                .map(|_| ())
                .map_err(|e| format!("couldn't start the analysis: {e}"))
        }
        #[cfg(not(feature = "salient"))]
        {
            let _ = (id, mask, comp, class);
            Err("AI masks are not available in this build".into())
        }
    }

    /// Apply the selections the threads have found.
    pub(crate) fn salient_poll(&mut self, polled: &mut Polled) {
        #[cfg(feature = "salient")]
        {
            let done: Vec<Outcome> = self.salient.results.1.try_iter().collect();
            for o in done {
                let found = match o.result {
                    Ok(seg) => seg,
                    Err(e) => {
                        polled.messages.push(e);
                        continue;
                    }
                };
                let Some(d) = self.develop_of(o.photo) else { continue };
                let mut d = (*d).clone();
                let Some(c) = d.masks.iter_mut().find(|m| m.id == o.mask).and_then(|m| m.components.get_mut(o.comp)) else { continue };
                // (the mask was changed or removed meanwhile: leave it)
                let MaskShape::Salient { class, seg } = &mut c.shape else { continue };
                if *class != o.class || seg.is_some() {
                    continue;
                }
                *seg = Some(found);
                match self.set_develop(o.photo, d, "AI Mask") {
                    Ok(()) => polled.changed = true,
                    Err(e) => polled.messages.push(e.to_string()),
                }
            }
        }
        #[cfg(not(feature = "salient"))]
        let _ = polled;
    }
}

#[cfg(feature = "salient")]
fn label(class: &str) -> &'static str {
    match class {
        "people" => "Finding people",
        "background" => "Finding the background",
        _ => "Finding the subject",
    }
}

#[cfg(test)]
mod tests;
