"""Measure the PyTorch package's pauses on the "joke" line, for
`PACKAGE_MEAN_PAUSE_SECS` in loop_golden.rs.

Run from the repo root, with the bench venv (isolated mode):

    source .tts-bench/env.sh
    HF_HUB_OFFLINE=1 .tts-bench/venv-pocket/bin/python -I \
        crates/sayd-pocket/tests/fixtures/measure_package_pauses.py \
        .tts-bench/runners/pocket_english_2026-04_mirror.yaml \
        crates/sayd-pocket/assets/fenrir.wav

Args: <english_2026-04 config with cloning weights> <voice clip>. Needs
pocket-tts 4.0.1. It generates at the package's default temperature (0.3),
calling `torch.manual_seed(seed)` for seeds 0..10, and prints each seed's pause
total and the mean. A pause is a run of at least 250 ms of 20 ms frames below
-40 dBFS, which is `pause_total` in loop_golden.rs.
"""

import sys
from pathlib import Path

import numpy as np
import torch
from pocket_tts import TTSModel

JOKE = (
    "Good news and bad news. Good news: I found the bug! Bad news: it was in "
    "your commit called quick fix. From three weeks ago."
)
SEEDS = range(0, 10)


def pause_total(a, sr=24_000):
    frame = sr // 50
    n = len(a) // frame
    rms = np.sqrt(np.mean(a[: n * frame].reshape(n, frame) ** 2, axis=1))
    silent = 20 * np.log10(rms + 1e-12) < -40
    total = run = 0
    for s in list(silent) + [False]:
        if s:
            run += 1
        else:
            if run * 20 >= 250:
                total += run
            run = 0
    return total * 0.02


config, clip = sys.argv[1], Path(sys.argv[2])
model = TTSModel.load_model(config=config)
assert model.temp == 0.3
state = model.get_state_for_audio_prompt(clip)
totals = []
for seed in SEEDS:
    torch.manual_seed(seed)
    audio = model.generate_audio(state, JOKE).numpy().reshape(-1)
    totals.append(pause_total(audio))
    print(f"seed {seed}: {len(audio) / 24_000:.2f} s of audio, {totals[-1]:.2f} s of pauses", flush=True)
print("per seed:", ", ".join(f"{t:.2f}" for t in totals))
print(f"mean: {np.mean(totals):.3f}")
