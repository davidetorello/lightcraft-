use super::*;

#[test]
fn only_plain_subject_background_and_people_go_to_a_model() {
    assert_eq!(class_of(&MaskShape::Subject), Some("subject"));
    assert_eq!(class_of(&MaskShape::Background), Some("background"));
    assert_eq!(class_of(&MaskShape::People { person: 0, parts: vec![] }), Some("people"));
    // particular parts of a person are the heuristic's
    assert_eq!(class_of(&MaskShape::People { person: 0, parts: vec!["face".into()] }), None);
    assert_eq!(class_of(&MaskShape::Sky), None);
    assert_eq!(class_of_name("people"), "people");
    assert_eq!(class_of_name("anything"), "subject");
}

#[test]
fn resample_keeps_values_and_survives_bad_sizes() {
    let grid = [0.0, 1.0, 1.0, 0.0];
    let up = resample(&grid, 2, 4);
    assert_eq!(up.len(), 16);
    assert!(up.iter().all(|v| (0.0..=1.0).contains(v)));
    assert!((up[0] - 0.0).abs() < 1e-6 && (up[3] - 1.0).abs() < 1e-6);
    assert_eq!(resample(&grid, 2, 2), grid.to_vec());
    // a grid that does not match its side, or an empty one, gives zeros instead of panicking
    assert_eq!(resample(&grid, 3, 2), vec![0.0; 4]);
    assert_eq!(resample(&[], 0, 2), vec![0.0; 4]);
}

#[test]
fn a_probability_grid_becomes_a_stored_selection() {
    let side = 4;
    let mut prob = vec![0.0; side * side];
    prob[5] = 1.0;
    prob[6] = f32::NAN;
    let seg = to_seg(&prob, side);
    let json = serde_json::to_string(&seg).expect("serializes");
    let back: SegMask = serde_json::from_str(&json).expect("round trips");
    assert_eq!(back, seg);
}

#[test]
fn the_union_takes_the_larger_value_per_cell() {
    let u = union(&[vec![0.1, 0.9, 0.0], vec![0.5, 0.2, 0.0, 0.7]]);
    assert_eq!(u, vec![0.5, 0.9, 0.0, 0.7]);
    assert!(union(&[]).is_empty());
}

#[test]
fn a_salient_mask_round_trips_and_an_unanalyzed_one_stays_small() {
    let shape = MaskShape::Salient { class: "subject".into(), seg: None };
    let json = serde_json::to_value(&shape).expect("serializes");
    assert!(json.to_string().len() < 80, "{json}");
    let back: MaskShape = serde_json::from_value(json).expect("reads back");
    assert_eq!(back, shape);
}

#[cfg(not(feature = "salient"))]
#[test]
fn without_the_feature_there_is_no_model_and_a_request_says_so() {
    assert!(!ready());
}

/// With the models embedded, a new Subject / Background / People mask holds the model's selection
/// (and keeps it when the settings are saved and read back); builds without them keep the heuristics.
#[cfg(feature = "salient")]
#[test]
fn a_new_subject_mask_carries_the_models_selection() {
    use serde_json::json;
    let mut s = Session::with_demo();
    for kind in ["subject", "background", "people"] {
        let out = s.execute("mask.add", &json!({"kind": kind}));
        let out = out.expect("adds the mask");
        assert!(out.get("pending").is_none(), "tests wait for the model: {out}");
        let id = s.active().expect("a photo");
        let d = s.develop_of(id).expect("settings");
        let shape = &d.masks.last().expect("the mask").components[0].shape;
        match shape {
            MaskShape::Salient { class, seg } => {
                assert_eq!(class, kind);
                assert_eq!(seg.is_some(), ready(), "{kind}");
            }
            other => assert!(!ready(), "{kind} became {other:?} with the models embedded"),
        }
    }
}
