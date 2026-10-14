#!/usr/bin/env bash
# Build the two segmentation models Lumen embeds (see docs/ai-masks.md):
# download the upstream ONNX files (Apache-2.0), check their sha256, quantize them to 8 bits.
# Usage: tools/build-salient-models.sh <output-dir>   then   LIGHTCRAFT_MODELS_DIR=<output-dir> cargo build ...
# Needs python3 and network access (pip installs onnx and onnxruntime into a throwaway virtualenv).
set -euo pipefail
out="${1:?usage: build-salient-models.sh <output-dir>}"
mkdir -p "$out"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
base="https://github.com/danielgatis/rembg/releases/download/v0.0.0"
fetch() { # name file sha256
  if [ ! -f "$work/$2" ]; then curl -fsSL "$base/$1" -o "$work/$2"; fi
  echo "$3  $work/$2" | { sha256sum -c - 2>/dev/null || shasum -a 256 -c -; }
}
fetch isnet-general-use.onnx general.onnx 60920e99c45464f2ba57bee2ad08c919a52bbf852739e96947fbb4358c0d964a
fetch u2net_human_seg.onnx person.onnx 01eb6a29a5c4d8edb30b56adad9bb3a2a0535338e480724a213e0acfd2d1c73c
py3="$(command -v python3 || command -v python)" || { echo "error: python not found" >&2; exit 1; }
"$py3" -m venv "$work/venv"
py="$work/venv/bin/python"; [ -x "$py" ] || py="$work/venv/Scripts/python.exe"
"$py" -m pip install --quiet onnx onnxruntime
"$py" - "$work" "$out" <<'PY'
import sys
from onnxruntime.quantization import quantize_dynamic, QuantType
work, out = sys.argv[1:3]
for src, dst in (("general", "salient-general-int8.onnx"), ("person", "salient-person-int8.onnx")):
    quantize_dynamic(f"{work}/{src}.onnx", f"{out}/{dst}", weight_type=QuantType.QUInt8)
    print("wrote", f"{out}/{dst}")
PY
