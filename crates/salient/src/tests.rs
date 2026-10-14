use super::*;

const ID_MEAN: [f32; 3] = [0.0; 3];
const ID_STD: [f32; 3] = [1.0; 3];

#[test]
fn preprocess_rejects_empty_and_mismatched_images() {
    assert!(preprocess(&[], 0, 0, 8, ID_MEAN, ID_STD).is_none());
    assert!(preprocess(&[0; 12], 2, 2, 8, ID_MEAN, ID_STD).is_some(), "3 bytes per pixel: 2×2 needs exactly 12");
    assert!(preprocess(&[0; 13], 2, 2, 8, ID_MEAN, ID_STD).is_none());
    assert!(preprocess(&[0; 11], 2, 2, 8, ID_MEAN, ID_STD).is_none());
    assert!(preprocess(&[0; 12], 2, 2, 0, ID_MEAN, ID_STD).is_none());
    // sizes whose product overflows are refused rather than wrapping
    assert!(preprocess(&[0; 12], usize::MAX, 2, 8, ID_MEAN, ID_STD).is_none());
}

#[test]
fn preprocess_normalizes_per_channel_in_planar_order() {
    // 1×1 pixel (255, 0, 51): any output size repeats it
    let out = preprocess(&[255, 0, 51], 1, 1, 2, [0.5, 0.0, 0.1], [0.5, 1.0, 0.1]).expect("valid");
    assert_eq!(out.len(), 12);
    assert!(out[..4].iter().all(|v| (v - 1.0).abs() < 1e-5), "red plane: (1 − .5)/.5");
    assert!(out[4..8].iter().all(|v| v.abs() < 1e-5), "green plane");
    assert!(out[8..].iter().all(|v| (v - 1.0).abs() < 1e-5), "blue plane: (.2 − .1)/.1");
}

#[test]
fn preprocess_keeps_the_layout_of_the_photo() {
    // left half black, right half white, 4×2 → 4×4: the left columns stay dark
    let mut rgb = Vec::new();
    for _ in 0..2 {
        for x in 0..4 {
            rgb.extend_from_slice(if x < 2 { &[0, 0, 0] } else { &[255, 255, 255] });
        }
    }
    let out = preprocess(&rgb, 4, 2, 4, ID_MEAN, ID_STD).expect("valid");
    assert!(out[0] < 0.05 && out[3] > 0.95, "{out:?}");
}

#[test]
fn normalize_spans_zero_to_one_and_survives_flat_and_broken_output() {
    let mut v = [0.2, 0.4, 0.6];
    normalize(&mut v);
    assert!(v[0].abs() < 1e-6 && (v[2] - 1.0).abs() < 1e-6 && (v[1] - 0.5).abs() < 1e-6);
    let mut flat = [0.3; 4];
    normalize(&mut flat);
    assert_eq!(flat, [0.0; 4]);
    let mut broken = [f32::NAN, 1.0, f32::INFINITY, 3.0];
    normalize(&mut broken);
    assert_eq!(broken[0], 0.0);
    assert_eq!(broken[2], 0.0);
    assert!(broken.iter().all(|v| (0.0..=1.0).contains(v)));
}

#[test]
fn no_models_means_unavailable_not_an_error_at_build_time() {
    // `available` is false in a build without LIGHTCRAFT_MODELS_DIR, and a request for the model says why
    if embedded(Class::General).is_none() {
        assert!(!available());
        #[cfg(not(target_arch = "wasm32"))]
        assert!(matches!(Model::embedded(Class::General), Err(Error::NotEmbedded(Class::General))));
    }
}

/// With the models (`LIGHTCRAFT_MODELS_DIR` at run time), a bright disc on a dark background is found.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn the_models_find_a_bright_object_on_a_dark_background() {
    let Some(dir) = std::env::var_os("LIGHTCRAFT_MODELS_DIR").map(std::path::PathBuf::from) else { return };
    let (w, h) = (96usize, 64usize);
    let mut rgb = vec![20u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let (dx, dy) = (x as f32 - 48.0, y as f32 - 32.0);
            if (dx * dx + dy * dy).sqrt() < 18.0 {
                let i = (y * w + x) * 3;
                rgb[i..i + 3].copy_from_slice(&[230, 200, 170]);
            }
        }
    }
    for (class, file) in [(Class::General, "salient-general-int8.onnx"), (Class::Person, "salient-person-int8.onnx")] {
        let Ok(bytes) = std::fs::read(dir.join(file)) else { continue };
        let model = Model::from_bytes(class, &bytes).expect("model loads");
        let matte = model.matte(&rgb, w, h).expect("segments");
        assert_eq!(matte.prob.len(), matte.side * matte.side);
        assert!(matte.prob.iter().all(|v| (0.0..=1.0).contains(v)));
        let s = matte.side;
        let centre = matte.prob[(s / 2) * s + s / 2];
        let corner = matte.prob[s + 1];
        // the person model may not call a disc a person; the general one must find it
        if class == Class::General {
            assert!(centre > corner + 0.3, "centre {centre} vs corner {corner}");
        }
    }
}
