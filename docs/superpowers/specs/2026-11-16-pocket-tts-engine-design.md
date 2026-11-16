# A second engine: Pocket TTS, with voices cloned from a clip

Date: 2026-11-16
Status: approved, not yet implemented

## Why

Kokoro gets information across but sounds flat: there is no way to make it
joke, tease or sound pleased with itself, and the user wants to banter with
their agent without it sounding like a robot. A side-by-side listening test
(`.tts-bench/`, not committed) put five open models through the same lines on
this machine's CPU. Only Pocket TTS (Kyutai, 100M parameters) ran faster than
realtime: about 0.35-0.6x, against 9-55x for Qwen3-TTS, Orpheus, Chatterbox
and Dia. Pocket TTS has no emotion controls, but it clones a voice from a short
reference clip, and a clone copies the clip's delivery along with its
timbre, so an expressive clip is the way to an expressive voice.

So the user wants Pocket TTS in sayd **next to** Kokoro, switchable, starting
with a clone of `am_fenrir`, the Kokoro voice they use today.

## Goals

- A user can switch between Kokoro and Pocket TTS in the settings window and
  in `config.toml`, without restarting the daemon. Kokoro stays the default
  engine, so an existing install sounds exactly as before.
- With Pocket TTS selected, the voice is a reference `.wav` file. Picking a
  different clip changes the voice; adding a file to the voices directory adds
  a voice.
- sayd ships one reference clip, `fenrir` (Kokoro `am_fenrir`), as the
  default Pocket voice. The user tried an expressive Qwen3-TTS "Aiden" clip as
  a reference and found the joking delivery sounded weird when copied, so it
  is not shipped. Other voices are a `.wav` dropped into the voices directory.
- Pocket TTS runs in-process in Rust on ONNX Runtime, the same `ort` the
  Kokoro crate already loads. No Python and no new process.
- On this machine's CPU, int8 Pocket TTS synthesizes at least as fast as
  realtime for a typical sentence, so speech never stalls between chunks.
- The **Download voices** flow can fetch the Pocket TTS model, with the same
  "what it costs, from which host" sentence before anything is fetched.

## Non-goals

- Streaming first audio before a chunk is complete. The engine hands sayd a
  whole chunk today; Pocket TTS can stream frame by frame later, but that is a
  separate change to the engine's audio path.
- Kyutai's eight predefined voices (alba, marius, ...). Their prompt states
  are separate files from a gated repository. Cloning from a clip covers the
  need.
- Languages other than English, GPU execution, fine-tuning.
- Emotion controls. Pocket TTS has none beyond sampling temperature; delivery
  comes from the reference clip.

## Anti-goals (failure even if it technically works)

- Python, torch or a sidecar process at runtime.
- Any network access outside the explicit download button. The privacy
  promise in the README stays true word for word.
- A change in how Kokoro behaves for anyone who does not opt in.
- Shipping or fetching weights from a source whose licence does not allow
  redistribution.

## Approaches

1. **Native Rust on ONNX Runtime (chosen).** A new crate, `sayd-pocket`, next
   to `sayd-kokoro`, driving a community ONNX export of the model with the
   generation loop ported from the reference implementations. It keeps sayd a
   single self-contained Rust daemon and leaves the Nix packaging unchanged.
   It is the most work: the autoregressive loop, flow sampling, state handling
   and voice encoding all need porting and testing.
2. **Python sidecar.** Rejected. sayd would start the official `pocket-tts`
   in a Python process and call it over localhost. Quickest, but it adds about
   1 GB of Python and torch and a second process to supervise.
3. **Link PocketTTS.cpp over FFI.** Rejected. It reuses a working loop, but
   puts a third-party C++ build inside a Rust/Nix project. Porting its loop to
   Rust costs little more.

The user picked 1.

## The model

Source: [`KevinAHM/pocket-tts-onnx`](https://huggingface.co/KevinAHM/pocket-tts-onnx),
bundle `onnx/english_2026-04/`, licensed CC-BY-4.0 and ungated. `english_2026-04`
is the newest English checkpoint with voice-cloning weights; the 2026-10
checkpoint ships without them. Pin the download to a commit hash. (The Kokoro
download does not pin today: `BASE_URL` and `scripts/fetch-models.sh` use
`resolve/main`. Pocket TTS pins anyway, since a third-party export can change
under us.)

| File | Size | Role |
|---|---|---|
| `bundle.json` | 24 KB | metadata: sample rate (24000), frame rate (12.5), `samples_per_frame` (1920), `latent_dim` (32), `max_token_per_chunk` (50), and the state manifests for `flow_lm_main` and `mimi_decoder` (names, shapes, dtypes, initial fill) |
| `tokenizer.model` | 59 KB | SentencePiece tokenizer |
| `bos_before_voice.npy` | 4 KB | BOS embedding inserted before the voice prompt (`insert_bos_before_voice: true`) |
| `text_conditioner.onnx` | 16 MB | token ids → text embeddings (fp32 only) |
| `mimi_encoder.onnx` | 40 MB | reference audio → voice embeddings (fp32) |
| `flow_lm_main[_int8].onnx` | 303 / 76 MB | transformer: conditioning + per-frame step, with KV-cache state in and out; outputs the conditioning vector and an end-of-speech logit |
| `flow_lm_flow[_int8].onnx` | 39 / 10 MB | flow network: one Euler step from noise towards a latent frame |
| `mimi_decoder[_int8].onnx` | 41 / 23 MB | latent frames → 24 kHz audio, with streaming state |

int8 total is about 165 MB, fp32 about 440 MB.

### Spike: the export works, the reference wrapper's chunking does not

Before approval the export was run on this machine through its own Python
wrapper (`onnxruntime` 1.32, `.tts-bench/runners/pocket_onnx_run.py`), on the
same lines and the same two reference clips as the PyTorch package:

- **Speed:** int8 0.35-0.55x realtime, fp32 about 0.6x. The goal holds.
- **Words:** Whisper heard every scripted word in all 16 clips.
- **Pauses:** the wrapper's clips run 10-30% longer than PyTorch's, because
  it leaves 0.7-1.2 s gaps between sentences. The "joke" line has 4.4 s of
  pauses, against 1.4 s from the PyTorch package.

So **the behaviour to match is the official PyTorch `pocket-tts` package
(4.0.1)**, which is what the user listened to and approved. The ONNX graphs are
only the compute. Port the package's text and audio handling, not the
wrapper's:

- `models/text_chunking.py`: `prepare_text_prompt` (character replacement,
  a trailing comma or dash becomes a period, the frames-after-EOS guess) and
  `split_into_best_sentences` (sentence boundaries first, regrouped up to the
  token budget).
- `get_state_for_audio_prompt`'s `end_on_pause` on the reference clip: trim
  trailing silence, fade out the last 20 ms, append 80 ms of silence. The
  package's own comment says a clip that ends on speech causes a burst or a
  wrong first word at the start of every chunk.
- A 5 ms fade-in on the first audio of every chunk, which hides the click from
  a fresh Mimi decoder state (`tts_model.py`, around line 522).
- The package's `frames_after_eos` default (`model_recommended_frames_after_eos`
  or the guess), not the wrapper's guess + 2. The builder confirms which
  difference causes the long gaps by measuring, since this is the most likely
  one.

The package lives at
`.tts-bench/venv-pocket/lib/python3.12/site-packages/pocket_tts/`.

### Generation loop (per chunk)

Ported from `pocket_tts_onnx.py` (same repository, 719 lines) and cross-checked
against [PocketTTS.cpp](https://github.com/VolgaGerm/PocketTTS.cpp). The
builder must read the reference code for exact tensor names and shapes; this
is the outline:

1. **Voice (once per voice):** load the clip as 24 kHz mono f32, run
   `mimi_encoder`, prepend the BOS embedding, and prefill `flow_lm_main` with
   the voice embeddings starting from the manifest's initial state. The
   resulting state is the voice state. Clone it for every chunk; never mutate
   the cached one.
2. **Text:** tokenize the chunk; run `text_conditioner`; prefill `flow_lm_main`
   with the text embeddings on a copy of the voice state.
3. **Frames:** loop. Run `flow_lm_main` one step to get the conditioning vector
   and an EOS logit. Sample a latent frame by running `flow_lm_flow` for
   `lsd_steps` Euler steps from Gaussian noise with std `sqrt(temperature)`
   (no noise at temperature 0). Feed the frame back as the next step's input.
   When the EOS logit first exceeds -4.0, record the step and stop a few frames
   after it (`frames_after_eos`: 3+2 for chunks of four words or fewer,
   1+2 otherwise, per the reference). Cap the frame count with the reference's
   max-length estimate from the token count.
4. **Audio:** run `mimi_decoder` over the frames in chunks of about 12-15,
   carrying its state, and concatenate. 1920 samples per frame.

Session options, from the reference's measured 2x win:
`intra_op_num_threads = min(threads, 4)`, `inter_op_num_threads = 1`. The
existing `threads` setting is still the cap; the builder should measure
rather than trust either number on this machine (Kokoro's peak was 8).

Defaults: `temperature = 0.3` (Kyutai's config for this checkpoint says human
evaluations preferred 0.3), `lsd_steps = 1`, int8.

## How it fits sayd

### The seam

`sayd_core::synth::Synthesizer` already separates text processing from audio.
Pocket TTS implements it as `PocketSynthesizer` in `crates/sayd/src/`, beside
`kokoro_synth.rs`:

- `phonemize(text, voice)`: Pocket TTS reads text, not phonemes. Return the
  text after the bundle's normalisation (replace curly quotes with straight
  ones, as the 2026-04 config does).
- `fits(text)`: always `true` for text within the engine's character target.
  Pocket TTS splits inside `synth` with the ported `split_into_best_sentences`
  (sentence boundaries first, 50 tokens per piece), the way the package does.
  The shared chunker would instead halve two merged sentences mid-sentence:
  English runs about 3.3 characters per token, so the default 400-character
  target is about 120 tokens, and `refit` cuts on the middle word. Keeping the
  split inside `PocketSynthesizer` also leaves `sayd-core`'s chunker, and
  therefore Kokoro, untouched. *(Design call, from the builder check.)*
- `synth(text, voice, speed)`: run the loop above. Pocket TTS has no speed
  input, so a `speed` other than 1.0 goes through the existing WSOLA
  `time_stretch`, the same as Kokoro's `speed_mode = "stretch"`.
- `sample_rate()`: 24 000, already the trait's constant.
- `unload` / `is_loaded`: drop all five sessions and the cached voice states,
  so idle unload returns the memory.
- `voice_exists(voice)`: a file `<voice>.wav` exists in the Pocket voices
  directory.
- `reconfigure(cfg)`: return `true` when the Pocket precision or `threads`
  changes.

### Switching engines

The daemon owns one `Box<dyn Synthesizer>` today. Switching means building the
other synthesizer and replacing it, from the same place that already applies a
config change.

`Engine::submit` checks `voice_exists` and stores the voice name on each queued
utterance. With two engines, an utterance submitted without an explicit voice
must resolve its voice when it *plays* (`voice` for Kokoro, `pocket.voice` for
Pocket TTS), not when it was queued. Otherwise a queued `am_fenrir` reaches
Pocket TTS after a switch and becomes a sticky synth error. *(Design call,
from the builder check.)* An engine switch never reaches `reconfigure`; it replaces the
object. The builder decides exactly where that swap lives, and must keep the
current invariants: the swap happens between utterances, and a sticky synth
error from one engine must not survive into the other.

### Config

```toml
engine = "kokoro"        # "kokoro" (default) | "pocket"; unknown values fall back to kokoro

[pocket]
voice = "fenrir"         # a <name>.wav in <models>/pocket/voices/
precision = "int8"       # "int8" (default) | "fp32"
temperature = 0.3
```

The top-level `voice` stays the Kokoro voice, so switching engines back and
forth keeps both choices. `Config` gets an `engine: String` and a boxed
`pocket: PocketConfig`, following the `reword` precedent for nested tables.
Unknown values fall back the way `model_file_for` falls back today.

### Files on disk

```
<models>/                      # default_models_dir(), unchanged
  model.onnx, voices/*.bin     # Kokoro, unchanged
  pocket/
    english_2026-04/           # the bundle files above
    voices/
      fenrir.wav
```

Cache each voice state's *encoder output* under
`$XDG_CACHE_HOME/sayd/pocket/<sha256 of the wav>.f32`, so a voice is encoded
once per clip, not once per daemon start. The prefill is cheap and runs at load.

### The shipped voices

Commit `fenrir.wav` (about 430 KB, 24 kHz mono 16-bit) to the repo, and
install it into `pocket/voices/` when the model is downloaded, since that is
the moment the directory comes into existence. Its source: Kokoro-82M
`am_fenrir` (Apache-2.0), speed 1.0, reading this 9-second sentence:

> Hey there. I just finished reading through the whole project, and honestly, it's in pretty good shape. There are a couple of things I'd like to clean up, but nothing scary.

The clip in `.tts-bench/ref_fenrir.wav` (not committed) is the one the user
listened to; copy it, or regenerate it with sayd's own Kokoro, and record the
source and licence in the README.

### Settings window

- A new **Engine** row in the Voice group: Kokoro / Pocket TTS.
- The **Voice** dropdown lists Kokoro voice packs or Pocket voice clips,
  depending on the engine, and edits `voice` or `pocket.voice`.
- With Pocket TTS selected and no model on disk, show a **Download Pocket TTS**
  row that states its cost before fetching ("165 MB from huggingface.co: the
  Pocket TTS model and the Fenrir voice clip"), using the existing download
  machinery's `.part`-then-rename and cancel behaviour.
- A missing clip shows the same "no voice pack" marker the Kokoro dropdown
  shows for a missing pack.

### An explicit voice while Pocket TTS is active

Decided by the user: a request that names a voice (`say --voice …`, the D-Bus
`Say` option `voice`, `SetVoice`) while Pocket TTS is the engine
**uses a Pocket voice instead of failing**:

- If the name is a Pocket clip (`fenrir`, or any `<name>.wav` in the
  voices directory), use that clip.
- Otherwise speak with `pocket.voice` and log the substitution once per voice
  name, so a hook that passes `--voice am_fenrir` keeps working after a switch.
- `SetVoice` with a Kokoro pack name while Pocket TTS is active changes
  `voice` (the Kokoro choice) and leaves `pocket.voice` alone, so it takes
  effect on the next switch back. A Pocket clip name changes `pocket.voice`.
- The mirror case, a Pocket clip name while Kokoro is active, gets the same
  treatment the other way round: speak with `voice`, log once.

Rejected: refusing the request (breaks hooks after a switch), and speaking that
one request on Kokoro (keeps both engines loaded, about 1.3 GB more, and turns
one swappable synthesizer into two).

### D-Bus and CLI

Add `engine` to the status JSON and the D-Bus properties next to `voice`, so
`say status` and the tray show which engine is speaking. No new CLI flags.

## Constraints

- The `ort` version is pinned (`=2.0.0-rc.13`, `load-dynamic`). Reuse it; do
  not add a second ONNX Runtime.
- Tokenizer: prefer a pure-Rust SentencePiece reader, or the `tokenizers`
  crate with the `tokenizer.json` Kyutai publishes (ungated, in
  `kyutai/pocket-tts-without-voice-cloning`, `languages/english_2026-04/`),
  over C++ bindings. Prove the token ids match the Python tokenizer on a test
  corpus.
- `.npy` and `.wav` reading need only small hand-written parsers or existing
  light crates. Resample a clip that is not 24 kHz mono with the existing
  `resample.rs`.
- Run `just clippy` (or the repo's lint) before every commit, per the user's
  instructions.

## Testing

- **Golden test for the graph loop.** With temperature 0 the loop is
  deterministic. Generate latents and audio for a few single-chunk sentences
  with `pocket_tts_onnx.py` on the same fp32 ONNX files (`.tts-bench/venv-ponnx`
  has it set up), commit small fixtures, and assert the Rust latents match
  within a stated tolerance. This proves the graphs are driven correctly.
- **Unit tests for the ported package logic:** `prepare_text_prompt`,
  `split_into_best_sentences` (against outputs captured from the Python
  package), `end_on_pause`, and the fade-in.
- **Pause check:** for the multi-sentence "joke" line, the total silence (over
  250 ms at -40 dB) in the Rust output is within 0.5 s of the PyTorch package's
  for the same voice. This is the regression the spike found.
- **Tokenizer parity** on a corpus with punctuation, numbers and code words.
- **Speed:** an ignored-by-default test that prints the realtime factor for
  int8 and fp32 on a 20-word sentence. The goal is under 1.0 on this machine.
- **Engine switch:** with stub synthesizers, switching mid-queue finishes the
  current utterance on the old engine and speaks the next on the new one, and a
  sticky error on one engine is cleared by the switch.
- The existing real-model tests (`kokoro_synth.rs` style) get a Pocket twin
  that skips when the bundle is absent.

## Left to the builder

- Where the engine swap lives in the daemon, and how the settings model
  represents the engine-dependent voice list.
- int8 vs fp32 for `mimi_encoder` if int8 is ever added (the bundle ships it
  fp32 only today).
- Exact `threads` default for Pocket sessions, from measurement.
- Whether to overlap decoding with generation (the reference uses a decoder
  thread). Only if the speed goal is missed without it.
