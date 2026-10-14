//! Salient-object and person segmentation with two small ONNX models, run in pure Rust.
//!
//! * [`Class::General`]: ISNet (DIS, Apache-2.0), the most salient object, 1024 px input.
//! * [`Class::Person`]: U²-Net trained on humans (Apache-2.0), whole people clothes included, 320 px
//!   input.
//!
//! Both are quantized to 8 bits (about 45 MB each; the masks match the full-precision ones to 99 %).
//! They are embedded at build time from `LIGHTCRAFT_MODELS_DIR` (see `build.rs`): without them
//! [`embedded`] is `None` and callers keep their heuristics. A [`Matte`] covers the whole photo, stretched to
//! the model's square input, and is kept with the mask (as its logits) so nothing needs the model afterwards.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod models {
    include!(concat!(env!("OUT_DIR"), "/models.rs"));
}

/// Which model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    /// The most salient object (any kind).
    General,
    /// Whole people.
    Person,
}

impl Class {
    /// The side of the model's square input.
    pub fn side(self) -> usize {
        match self {
            Class::General => 1024,
            Class::Person => 320,
        }
    }

    fn mean(self) -> [f32; 3] {
        [0.485, 0.456, 0.406]
    }

    fn std(self) -> [f32; 3] {
        match self {
            Class::General => [1.0, 1.0, 1.0],
            Class::Person => [0.229, 0.224, 0.225],
        }
    }
}

/// The embedded model for `class`, if this build has it.
pub fn embedded(class: Class) -> Option<&'static [u8]> {
    match class {
        Class::General => models::GENERAL,
        Class::Person => models::PERSON,
    }
}

/// Whether this build embeds both models.
pub fn available() -> bool {
    cfg!(not(target_arch = "wasm32")) && embedded(Class::General).is_some() && embedded(Class::Person).is_some()
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the {0:?} segmentation model is not part of this build")]
    NotEmbedded(Class),
    #[error("segmentation is not available on this platform")]
    Unsupported,
    #[error("the image is empty or its size does not match its pixels")]
    BadImage,
    #[error("segmentation model: {0}")]
    Model(String),
}

/// A probability that each pixel belongs to the object, over a `side × side` grid covering the whole
/// photo (stretched), 0..1 after normalizing the model's output to its own range.
#[derive(Clone, Debug, PartialEq)]
pub struct Matte {
    pub side: usize,
    pub prob: Vec<f32>,
}

/// Bilinear resize of 8-bit RGB (`w × h`) to `side × side`, normalized per channel: `(v/255 − mean) / std`,
/// planar (CHW). `None` for an empty image or one whose pixel count does not match.
pub fn preprocess(rgb: &[u8], w: usize, h: usize, side: usize, mean: [f32; 3], std: [f32; 3]) -> Option<Vec<f32>> {
    if w == 0 || h == 0 || side == 0 || rgb.len() != w.checked_mul(h)?.checked_mul(3)? {
        return None;
    }
    let plane = side.checked_mul(side)?;
    let mut out = vec![0.0f32; plane.checked_mul(3)?];
    let (sx, sy) = (w as f32 / side as f32, h as f32 / side as f32);
    for y in 0..side {
        let fy = ((y as f32 + 0.5) * sy - 0.5).clamp(0.0, (h - 1) as f32);
        let (y0, wy) = (fy.floor() as usize, fy.fract());
        let y1 = (y0 + 1).min(h - 1);
        for x in 0..side {
            let fx = ((x as f32 + 0.5) * sx - 0.5).clamp(0.0, (w - 1) as f32);
            let (x0, wx) = (fx.floor() as usize, fx.fract());
            let x1 = (x0 + 1).min(w - 1);
            for c in 0..3 {
                let at = |xx: usize, yy: usize| rgb.get((yy * w + xx) * 3 + c).copied().unwrap_or(0) as f32;
                let top = at(x0, y0) * (1.0 - wx) + at(x1, y0) * wx;
                let bottom = at(x0, y1) * (1.0 - wx) + at(x1, y1) * wx;
                let v = (top * (1.0 - wy) + bottom * wy) / 255.0;
                if let Some(o) = out.get_mut(c * plane + y * side + x) {
                    *o = (v - mean[c]) / std[c];
                }
            }
        }
    }
    Some(out)
}

/// Stretch the model's output to 0..1 (the models' own sigmoid outputs rarely span it).
pub fn normalize(out: &mut [f32]) {
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for v in out.iter().filter(|v| v.is_finite()) {
        lo = lo.min(*v);
        hi = hi.max(*v);
    }
    let range = hi - lo;
    for v in out.iter_mut() {
        *v = if v.is_finite() && range > 1e-6 { ((*v - lo) / range).clamp(0.0, 1.0) } else { 0.0 };
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod run {
    use super::*;
    use tract_onnx::prelude::*;

    type Plan = std::sync::Arc<TypedRunnableModel>;

    /// A loaded model (loading optimizes the graph: a second or two).
    pub struct Model {
        class: Class,
        plan: Plan,
        executor: tract_linalg::multithread::Executor,
    }

    impl Model {
        /// Load the embedded model for `class`.
        pub fn embedded(class: Class) -> Result<Model, Error> {
            Model::from_bytes(class, embedded(class).ok_or(Error::NotEmbedded(class))?)
        }

        /// Load an ONNX model's bytes for `class`.
        pub fn from_bytes(class: Class, bytes: &[u8]) -> Result<Model, Error> {
            let side = class.side();
            let err = |e: TractError| Error::Model(e.to_string());
            let plan = tract_onnx::onnx()
                .model_for_read(&mut &bytes[..])
                .map_err(err)?
                .with_input_fact(0, f32::fact([1, 3, side, side]).into())
                .map_err(err)?
                .into_optimized()
                .map_err(err)?
                .into_runnable()
                .map_err(err)?;
            let threads = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, 8);
            Ok(Model { class, plan, executor: tract_linalg::multithread::Executor::multithread(threads) })
        }

        pub fn class(&self) -> Class {
            self.class
        }

        /// Segment 8-bit RGB (`w × h`).
        pub fn matte(&self, rgb: &[u8], w: usize, h: usize) -> Result<Matte, Error> {
            let side = self.class.side();
            let input = preprocess(rgb, w, h, side, self.class.mean(), self.class.std()).ok_or(Error::BadImage)?;
            let err = |e: TractError| Error::Model(e.to_string());
            let tensor = Tensor::from_shape(&[1, 3, side, side], &input).map_err(err)?;
            let out =
                tract_linalg::multithread::multithread_tract_scope(self.executor.clone(), || self.plan.run(tvec!(tensor.into()))).map_err(err)?;
            let first = out.first().ok_or_else(|| Error::Model("the model gave no output".into()))?;
            let view = first.to_plain_array_view::<f32>().map_err(err)?;
            let mut prob: Vec<f32> = view.iter().copied().collect();
            if prob.len() != side * side {
                return Err(Error::Model(format!("unexpected output of {} values", prob.len())));
            }
            normalize(&mut prob);
            Ok(Matte { side, prob })
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use run::Model;

#[cfg(test)]
mod tests;
