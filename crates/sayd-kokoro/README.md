# sayd-kokoro

[Kokoro-82M](https://huggingface.co/hexgrad/Kokoro-82M) inference via ONNX
Runtime: phonemes in, 24 kHz mono `f32` samples out.

```rust
let mut k = sayd_kokoro::Kokoro::new(models_dir, "model.onnx", 8)?;
k.load_voice("af_heart")?;
let samples = k.synth(phonemes, "af_heart", 1.0)?;
```

Also provides `audio::time_stretch`, a WSOLA implementation that changes tempo
without changing pitch.

## Requirements

Uses `ort` with `load-dynamic`, so ONNX Runtime is loaded at runtime rather than
linked. Point `ORT_DYLIB_PATH` at your `libonnxruntime.so`.

Model weights, `tokenizer.json` and voice packs are not included; fetch them
from the Kokoro ONNX release. Voice packs are exactly 510 rows x 256 `f32` and
are validated at load, and the model accepts at most 509 phoneme tokens per
call.

The `models` feature enables tests that need real weights in `./models`; it is
off by default so a plain `cargo test` needs no downloads.

Part of [sayd](https://crates.io/crates/sayd), a local speech daemon for Wayland.
