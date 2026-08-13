#!/usr/bin/env bash
# Download Kokoro-82M ONNX weights and voice packs into ./models.
set -euo pipefail
cd "$(dirname "$0")/.."

BASE=https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main
VOICES=(af af_alloy af_aoede af_bella af_heart af_jessica af_kore af_nicole
        af_nova af_river af_sarah af_sky am_adam am_echo am_eric am_fenrir
        am_liam am_michael am_onyx am_puck am_santa bf_alice bf_emma
        bf_isabella bf_lily bm_daniel bm_fable bm_george bm_lewis)
MODELS=(model.onnx model_fp16.onnx model_quantized.onnx)

mkdir -p models/voices
for f in config.json tokenizer.json; do
  [[ -s models/$f ]] || curl -sSL -o "models/$f" "$BASE/$f"
done
for v in "${VOICES[@]}"; do
  [[ -s models/voices/$v.bin ]] || curl -sSL -o "models/voices/$v.bin" "$BASE/voices/$v.bin"
done
for m in "${MODELS[@]}"; do
  [[ -s models/$m ]] || { echo "fetching $m"; curl -sSL -o "models/$m" "$BASE/onnx/$m"; }
done
echo "models ready:"
ls -la models models/voices
