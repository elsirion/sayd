#!/usr/bin/env bash
# Download Kokoro-82M ONNX weights and voice packs into ./models.
#
# The settings window does this too: with no voice packs installed, its Voice
# group offers a "Download voices" button (crates/sayd/src/settings/download.rs).
# That is the path an ordinary user takes; this script stays because it fetches
# all three model variants -- the window deliberately fetches only the fp32
# `model.onnx` the default config loads, not the 255 MB of fp16 and quantized
# weights nobody has asked for -- and because populating a source tree from a
# shell should not require a display.
#
# The two share no code and must not diverge: BASE and VOICES below are the
# same base URL and the same 29 names as `settings::download`, and the test
# `the_voice_list_matches_the_shell_script` reads this file to prove the voice
# lists still agree. Adding a voice to one means adding it to the other.
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
