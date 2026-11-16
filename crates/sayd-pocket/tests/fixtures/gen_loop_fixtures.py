"""Regenerate the loop_* fixtures from the reference ONNX wrapper.

Run from the repo root, with the bench venv (isolated mode):

    source .tts-bench/env.sh
    HF_HUB_OFFLINE=1 .tts-bench/venv-pocket/bin/python -I \
        crates/sayd-pocket/tests/fixtures/gen_loop_fixtures.py \
        .tts-bench/ref-pocket-onnx \
        .tts-bench/ref-pocket-onnx/onnx/english_2026-04 \
        crates/sayd-pocket/assets/fenrir.wav \
        crates/sayd-pocket/tests/fixtures

Args: <dir holding pocket_tts_onnx.py> <bundle dir> <the crate's assets/fenrir.wav> <output dir>.
The venv holds pocket-tts 4.0.1 (for `end_on_pause` and the text preparation)
and onnxruntime 1.27.0 (sayd's nix shell has 1.27.1).

Writes, all from the fp32 graphs at temperature 0:
- loop_voice.npy: mimi_encoder's output for the first 3.12 s of the fenrir
  clip (it pauses there; loop_golden.rs cuts `FENRIR_WAV` the same way),
  after the package's `end_on_pause`, [frames, conditioning_dim].
- loop_latents_<i>.npy: the wrapper's per-chunk loop (`_run_flow_lm_chunk`)
  for each sentence, [frames, latent_dim], stopping on the package's
  `frames_after_eos` for that sentence.
- loop_cases.json: the sentences and their fixture names.
"""

import json
import sys
import wave
from pathlib import Path

import numpy as np
import torch

wrapper_dir, bundle_dir, clip_path, out_dir = (Path(a) for a in sys.argv[1:5])
sys.path.insert(0, str(wrapper_dir))

from pocket_tts.data.audio_utils import end_on_pause  # noqa: E402
from pocket_tts.models.text_chunking import prepare_text_prompt  # noqa: E402
from pocket_tts_onnx import PocketTTSOnnx  # noqa: E402

CLIP_SAMPLES = 74_880  # 3.12 s at 24 kHz
# The package's minimum before EOS counts (TTSModel._MIN_FRAMES_BEFORE_EOS);
# the wrapper has no such rule, so the fixtures must not depend on it.
MIN_FRAMES_BEFORE_EOS = 6
SENTENCES = [
    "Hello there, friend.",
    "The build finished, and every single test passed on the first try.",
]
# english_2026-04's text settings, as gen_text_fixtures.py uses them.
TEXT_SETTINGS = dict(
    pad_with_spaces_for_short_inputs=False,
    remove_semicolons=False,
    append_terminal_punctuation=True,
    capitalize_first_letter=True,
    replace_characters={"’": "'", "‘": "'"},
)

out_dir.mkdir(parents=True, exist_ok=True)

with wave.open(str(clip_path), "rb") as w:
    assert w.getframerate() == 24_000 and w.getnchannels() == 1 and w.getsampwidth() == 2
    pcm = np.frombuffer(w.readframes(CLIP_SAMPLES), dtype=np.int16)
clip = pcm.astype(np.float32) / 32768.0
clip = end_on_pause(torch.from_numpy(clip)[None], 24_000)[0].numpy()

tts = PocketTTSOnnx(models_dir=str(bundle_dir), precision="fp32", temperature=0.0)
assert tts.model_recommended_frames_after_eos is None
voice = tts.mimi_encoder.run(None, {"audio": clip.reshape(1, 1, -1)})[0]
np.save(out_dir / "loop_voice.npy", voice[0].astype(np.float32))
state = tts._condition_with_voice_embeddings(voice)

cases = []
for i, sentence in enumerate(SENTENCES):
    prepared, guess = prepare_text_prompt(sentence, **TEXT_SETTINGS)
    # The package adds 2 to the guess (tts_model.py, generate_audio_stream).
    frames_after_eos = guess + 2
    ids = np.array(tts.tokenizer.Encode(prepared), dtype=np.int64).reshape(1, -1)
    # One sentence within the 50-token budget: the package keeps it as one chunk.
    assert ids.shape[1] <= 50
    latents = np.concatenate(
        list(tts._run_flow_lm_chunk(state, ids, None, frames_after_eos)), axis=1
    )[0]
    eos_step = latents.shape[0] - frames_after_eos
    assert eos_step >= MIN_FRAMES_BEFORE_EOS, (sentence, eos_step)
    assert latents.shape[0] < tts._estimate_max_gen_len(ids.shape[1]), "no EOS"
    name = f"loop_latents_{i}.npy"
    np.save(out_dir / name, latents.astype(np.float32))
    cases.append(
        dict(
            text=sentence,
            prepared=prepared,
            token_ids=ids[0].tolist(),
            frames_after_eos=frames_after_eos,
            eos_step=eos_step,
            frames=latents.shape[0],
            latents=name,
        )
    )
    print(f"{sentence!r}: {latents.shape[0]} frames, EOS at {eos_step}")

(out_dir / "loop_cases.json").write_text(
    json.dumps(dict(voice_frames=voice.shape[1], sentences=cases), indent=1) + "\n"
)
print(f"voice: {voice.shape[1]} frames")
