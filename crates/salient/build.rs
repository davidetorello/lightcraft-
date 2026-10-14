//! Optional build input: with `LIGHTCRAFT_MODELS_DIR=<folder>`, embed the segmentation models found there
//! (`salient-general-int8.onnx`, `salient-person-int8.onnx`; see docs/ai-masks.md) as `GENERAL` and `PERSON`.
//! Unset or missing, both are `None`: everything builds and runs, and the masks fall back to the
//! heuristics. Release builds set `LIGHTCRAFT_MODELS_REQUIRED=1` so a build never ships without them.
//! This script only reads a local directory (no network).
use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=LIGHTCRAFT_MODELS_DIR");
    println!("cargo::rerun-if-env-changed=LIGHTCRAFT_MODELS_REQUIRED");
    let required = std::env::var_os("LIGHTCRAFT_MODELS_REQUIRED").is_some();
    let dir = std::env::var_os("LIGHTCRAFT_MODELS_DIR").map(PathBuf::from);
    let mut src = String::new();
    let mut missing = Vec::new();
    for (name, file) in [("GENERAL", "salient-general-int8.onnx"), ("PERSON", "salient-person-int8.onnx")] {
        let path = dir.as_ref().map(|d| d.join(file)).filter(|p| p.is_file()).and_then(|p| p.canonicalize().ok());
        match path {
            Some(p) => {
                println!("cargo::rerun-if-changed={}", p.display());
                src.push_str(&format!("pub static {name}: Option<&[u8]> = Some(include_bytes!({:?}));\n", p.display().to_string()));
            }
            None => {
                missing.push(file);
                src.push_str(&format!("pub static {name}: Option<&[u8]> = None;\n"));
            }
        }
    }
    if !missing.is_empty() {
        let why = format!("segmentation models not embedded ({}): LIGHTCRAFT_MODELS_DIR is unset or lacks them", missing.join(", "));
        if required {
            println!("cargo::error={why}");
        } else {
            println!("cargo::warning={why}; Subject, Background and People use the heuristics");
        }
    }
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default()).join("models.rs");
    if let Err(e) = std::fs::write(&out, src) {
        println!("cargo::error=cannot write {}: {e}", out.display());
    }
}
