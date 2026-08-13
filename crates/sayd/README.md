# sayd

Local text-to-speech for Wayland. Select text, press a key, hear it — with
nothing leaving your machine.

Kokoro-82M runs locally through ONNX Runtime with a misaki-derived English
frontend. There is no cloud inference, no telemetry, and no network access at
runtime.

## Status

Early. The engine and audio path work: text in, chunk-streamed speech out,
starting about a second after the keypress regardless of how much was selected.
The D-Bus interface, control CLI, tray icon and settings window are in progress.

## Requirements

- Wayland compositor supporting `wlr-data-control` v2 for the primary selection
  (sway 1.9+)
- ONNX Runtime at `ORT_DYLIB_PATH`
- espeak-ng, with `ESPEAK_DATA_PATH` set
- Kokoro model weights and voice packs in `models/`

## Licence

MIT, except the vendored misaki lexicons in
[`sayd-misaki-en`](https://crates.io/crates/sayd-misaki-en), which are
Apache-2.0.
