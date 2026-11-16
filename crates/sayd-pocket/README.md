# sayd-pocket

[Pocket TTS](https://huggingface.co/kyutai/pocket-tts) (Kyutai, 100M
parameters) inference via ONNX Runtime: text in, 24 kHz mono `f32` samples out.

This crate loads the ONNX bundle (`bundle.json`, `.npy` voice and BOS
tensors, five ONNX sessions) and, in later releases, runs the generation loop.

> **This crate needs ONNX Runtime and model weights that it does not ship.**
> It compiles without either; the failure arrives at runtime. The weights are
> licensed CC-BY-4.0 (Kyutai; ONNX export by KevinAHM) and are downloaded
> separately.
