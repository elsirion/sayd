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

## Testing without an audio device

`sayd` normally opens the default audio output device (`cpal`) on startup and
exits if that fails. On a machine with no device -- no `/dev/snd`, PulseAudio
refusing to start, CI -- that means the D-Bus interface never comes up
either.

Setting `SAYD_NO_AUDIO=1` makes the daemon substitute a sink that accepts and
discards every sample instead of opening a device, so `sh.sayd.Sayd1` can
still be introspected, called, and polled over the session bus. This is a
testing aid, not a supported way to run `sayd` -- there is no audio output in
this mode, and utterances there finish instantly since nothing paces
playback.

## Licence

MIT, except the vendored misaki lexicons in
[`sayd-misaki-en`](https://crates.io/crates/sayd-misaki-en), which are
Apache-2.0.
