# Pocket TTS Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **2026-11-17 note:** some steps below changed while they were built. Each
> change is a ruling (R1 to R21) in the SDD ledger,
> `.superpowers/sdd/2026-11-16-pocket-tts-engine/progress.md`, which is kept
> locally and not committed. The ones a reader of this plan would trip over:
> Task 4's `frames_after_eos` is the package's guess + 2, not "not the wrapper's
> guess + 2" (R6); the Fenrir clip lives at `crates/sayd-pocket/assets/fenrir.wav`
> as `FENRIR_WAV`, not under `crates/sayd/assets/pocket/` (R8); there is no
> `SetVoice` routing, because sayd has no `SetVoice` (R11); "installed" means
> every int8 file is present (R16); and a missing Pocket model is refused at
> submission through `Synthesizer::unavailable` rather than accepted and failed
> (R18). The design doc's own update note lists what was built.

**Goal:** Add Pocket TTS as a second, switchable speech engine in sayd. It runs in Rust on ONNX Runtime and speaks with a voice cloned from a reference clip, Fenrir by default.

**Architecture:** A new crate `sayd-pocket` drives the five ONNX graphs of the `KevinAHM/pocket-tts-onnx` export. It reproduces the official `pocket-tts` 4.0.1 package's text and audio handling. The `sayd` binary wraps it as `PocketSynthesizer` and puts both engines behind one `SwitchingSynthesizer` that implements `sayd_core::synth::Synthesizer`. That way `sayd-core`'s `Engine` keeps owning a single `Box<dyn Synthesizer>`. The switch takes effect at the next `unload`, which the engine already defers until no utterance is in flight.

**Tech Stack:** Rust 2021 (MSRV 1.87); `ort =2.0.0-rc.13` (`load-dynamic`), shared with `sayd-kokoro`; GTK4/libadwaita settings; Python only as a fixture generator, never at runtime.

**Spec:** `docs/superpowers/specs/2026-11-16-pocket-tts-engine-design.md`

## Global Constraints

- Default engine stays `"kokoro"`. An existing config must produce byte-identical Kokoro behaviour.
- No Python, torch or extra process at runtime. No network outside the explicit download button.
- Reuse the workspace's `ort = "=2.0.0-rc.13"` with `default-features = false, features = ["load-dynamic"]`. No second ONNX Runtime.
- Model source: `https://huggingface.co/KevinAHM/pocket-tts-onnx/resolve/58a6d00cf13d239b6748cb0769f35c580a8f606c/onnx/english_2026-04/<file>`, pinned to that commit. Licence CC-BY-4.0, named in the README.
- Behaviour to match is the official PyTorch `pocket-tts` 4.0.1 package, at `.tts-bench/venv-pocket/lib/python3.12/site-packages/pocket_tts/`. The ONNX wrapper `.tts-bench/ref-pocket-onnx/pocket_tts_onnx.py` is the reference only for *how to drive the graphs*.
- Defaults: `temperature = 0.3`, `lsd_steps = 1`, `precision = "int8"`, EOS threshold `-4.0`, `max_token_per_chunk = 50`, 1920 samples per frame, 24 000 Hz.
- ORT session options for Pocket: `intra_op_num_threads = min(cfg.threads, 4)`, `inter_op_num_threads = 1`.
- Only one shipped Pocket voice: `fenrir` (Kokoro `am_fenrir`, Apache-2.0).
- Tokenizer and file parsers are pure Rust. No C++ bindings.
- Before every commit: `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` inside `nix develop`. Both must pass. `sayd` binary tests that need Wayland/D-Bus run through `scripts/headless.sh`.
- Commit on the current branch (`master`). No new branches or worktrees.

## Review Focus

1. **A queued utterance spans an engine switch.** Switching engines while one utterance plays and two are queued finishes the current one on the old engine and speaks both queued ones on the new engine, with no sticky error. Pinned in Task 6, `switch_waits_for_unload_and_queued_kokoro_voice_is_substituted`.
2. **The reference clip is not 24 kHz mono, or ends on speech.** A 44.1 kHz stereo clip with no trailing silence clones without a click or burst at chunk starts. Pinned in Task 5, `clip_at_44k1_stereo_is_resampled_and_end_on_pause_applied`.
3. **A clip is deleted or replaced while the daemon runs.** Replacing `fenrir.wav` with another clip re-encodes it (the cache is keyed by content hash, not by name). Deleting it makes `voice_exists` false without a panic. Pinned in Task 5, `replaced_clip_is_reencoded_and_deleted_clip_is_reported_missing`.
4. **Long multi-sentence text** (a 1 500-character article) is split at sentence boundaries, never mid-word, and the total pause time stays close to the official package's. Pinned in Task 4, `pause_total_matches_package_within_half_a_second`, and Task 3, `long_text_splits_only_at_sentence_or_word_boundaries`.
5. **The Pocket bundle is missing while `engine = "pocket"`.** The daemon starts, warns once that the model is missing, and rejects submissions with a message that names the settings download, rather than crashing or wedging. Pinned in Task 6, `pocket_selected_without_bundle_warns_and_rejects_cleanly`.

---

### Task 1: Config fields, snapshot, and clearing a sticky error on engine switch

**Files:**
- Modify: `crates/sayd-core/src/config.rs` (`Config` at ~632, `Default` at ~672)
- Modify: `crates/sayd-core/src/engine.rs` (`Snapshot` at ~167, config apply at ~390)

**Interfaces:**
- Produces: `Config.engine: String` (`"kokoro"` default). `Config.pocket: Box<PocketConfig>`. `pub struct PocketConfig { pub voice: String /* "fenrir" */, pub precision: String /* "int8" */, pub temperature: f32 /* 0.3 */ }` with `#[serde(default)]`. `pub fn Config::pocket_active(&self) -> bool` (true only for exactly `"pocket"`; unknown values mean Kokoro). `Snapshot.engine: String`, holding `"kokoro"` or `"pocket"` as resolved by `pocket_active`.

- [ ] **Step 1: Write the failing tests** in `config.rs`'s test module:
  - `engine_defaults_to_kokoro_and_pocket_table_has_spec_defaults`: `Config::default()` has `engine == "kokoro"`, `pocket.voice == "fenrir"`, `pocket.precision == "int8"`, `pocket.temperature == 0.3`, and `!pocket_active()`.
  - `pocket_table_round_trips`: `Config::load_str("engine = \"pocket\"\n[pocket]\nvoice = \"x\"\ntemperature = 0.5\n")` gives `pocket_active()`, `pocket.voice == "x"`, `temperature == 0.5`, `precision == "int8"`, and no error.
  - `unknown_engine_means_kokoro`: `engine = "espeak"` loads without error and `!pocket_active()`.
  - In `engine.rs`'s test module, `switching_engine_clears_a_sticky_synth_error`: use `FailingSynth` to put the engine into `ErrorKind::Synth`. Then `ApplyConfig` a config differing only in `engine`. The snapshot error is cleared and `snapshot().engine == "pocket"`. Applying a config that changes only `speed` does *not* clear the error.
- [ ] **Step 2:** `cargo test -p sayd-core engine_defaults pocket_table unknown_engine switching_engine` fails to compile.
- [ ] **Step 3:** Add the fields, following the boxed `reword` precedent and its doc-comment style. In the apply path, if `old.pocket_active() != new.pocket_active()`, clear the error exactly as an explicit dismiss does. Fill `Snapshot.engine`.
- [ ] **Step 4:** `cargo test -p sayd-core` passes. Every existing test still passes.
- [ ] **Step 5:** Run fmt and clippy, then commit: `feat(config): an engine setting and a [pocket] table, inert until chosen`.

### Task 2: `sayd-pocket` crate: bundle metadata, `.npy`, and sessions

**Files:**
- Create: `crates/sayd-pocket/{Cargo.toml,README.md,LICENSE}`, `src/lib.rs`, `src/bundle.rs`, `src/npy.rs`
- Modify: root `Cargo.toml` (`[workspace.dependencies] sayd-pocket`); `package.nix` (`cargoTestFlags` gains `-p sayd-pocket`)
- Test fixture: `crates/sayd-pocket/tests/fixtures/bundle.json` (copy of `.tts-bench/ref-pocket-onnx/bundle.json`)

**Interfaces:**
- Produces:
  - `pub struct Bundle { pub sample_rate: u32, pub samples_per_frame: usize, pub frame_rate: f32, pub latent_dim: usize, pub conditioning_dim: usize, pub max_token_per_chunk: usize, pub insert_bos_before_voice: bool, pub flow_lm_state: Vec<StateEntry>, pub mimi_state: Vec<StateEntry> }`
  - `pub struct StateEntry { pub input_name: String, pub output_name: String, pub dtype: Dtype, pub shape: Vec<usize>, pub fill: Fill }`, with `pub enum Fill { Nan, Zeros, Empty }` and `pub enum Dtype { F32, I64, Bool }`
  - `pub fn Bundle::load(dir: &Path) -> Result<Bundle, Error>`
  - `pub fn npy::read_f32(path: &Path) -> Result<(Vec<usize>, Vec<f32>), Error>`
  - `pub enum Precision { Int8, Fp32 }`, `pub fn Precision::from_config(s: &str) -> Precision` (anything but `"fp32"` is int8)
  - `pub struct Sessions` (all five) with `pub fn Sessions::load(dir: &Path, p: Precision, threads: usize) -> Result<Sessions, Error>`. It picks `<stem>_int8.onnx` where the bundle has one (`flow_lm_main`, `flow_lm_flow`, `mimi_decoder`), and fp32 `text_conditioner.onnx`/`mimi_encoder.onnx` always.
  - `pub fn bundle_dir(models_dir: &Path) -> PathBuf` = `models_dir/pocket/english_2026-04`
  - `pub enum Error` with `Display`, following `sayd_kokoro::Error`.

- [ ] **Step 1: Write the failing tests.**
  - `bundle_fixture_parses_spec_values`: `sample_rate == 24000`, `samples_per_frame == 1920`, `latent_dim == 32`, `max_token_per_chunk == 50`, `insert_bos_before_voice`. The first flow-LM entry is `state_0` with shape `[2,1,1000,16,64]` and `Fill::Nan`.
  - `npy_reads_bos_fixture`: write a small `.npy` in the test (header `{'descr': '<f4', 'fortran_order': False, 'shape': (1, 3), }`); expect shape `[1,3]` and values round-trip. Also reject `'>f4'` and `fortran_order: True` with an error.
  - `sessions_load_when_bundle_present` (`#[ignore]` unless `SAYD_POCKET_BUNDLE` points at a bundle dir): all five sessions load for both precisions.
- [ ] **Step 2:** `cargo test -p sayd-pocket` fails (crate missing).
- [ ] **Step 3:** Implement. Hand-write the `.npy` v1/v2 header parser (little-endian f32 only). Parse `bundle.json` with `serde_json` (already a dependency of `sayd-kokoro`).
- [ ] **Step 4:** `cargo test -p sayd-pocket` passes. The ignored test passes with `SAYD_POCKET_BUNDLE=$PWD/.tts-bench/ref-pocket-onnx/onnx/english_2026-04 cargo test -p sayd-pocket -- --ignored`.
- [ ] **Step 5:** Run fmt and clippy, then commit: `feat(pocket): a crate that loads the Pocket TTS ONNX bundle`.

### Task 3: Tokenizer and the package's text preparation

**Files:**
- Create: `crates/sayd-pocket/src/text.rs`, `src/tokenizer.rs`
- Create: `crates/sayd-pocket/tests/fixtures/gen_text_fixtures.py`, which writes `text_cases.json`. Commit the generated JSON.

**Interfaces:**
- Produces:
  - `pub struct Tokenizer`, `pub fn Tokenizer::load(path: &Path) -> Result<Tokenizer, Error>` (reads `tokenizer.model`), `pub fn encode(&self, text: &str) -> Vec<i64>`
  - `pub fn prepare_text_prompt(text: &str) -> (String, usize /* frames_after_eos_guess */)`
  - `pub fn split_into_best_sentences(tok: &Tokenizer, text: &str, max_tokens: usize) -> Vec<String>`
  - Port all three from `pocket_tts/models/text_chunking.py` (`prepare_text_prompt`, `split_into_best_sentences` with the bundle's `pad_with_spaces_for_short_inputs = false` and `remove_semicolons = false`), plus the config's `replace_characters` (`’`/`‘` → `'`).

- [ ] **Step 1: Generate fixtures.** `gen_text_fixtures.py` runs under `.tts-bench/venv-pocket` (`-I`, with paths as arguments). For about 30 inputs it records token ids, `prepare_text_prompt` output and `split_into_best_sentences(…, 50)` output. Cover: the six `lines.json` lines; curly quotes; a trailing comma or dash; a number like `3.5`; `e.g.`; a code word `foo_bar()`; ellipses; a 1 500-character paragraph; an empty string; one word. The header comment says how to regenerate.
- [ ] **Step 2: Write the failing tests.** `tokenizer_matches_python_ids`, `prepare_text_prompt_matches_package` and `split_matches_package`, each looping over `text_cases.json` with exact equality. Plus `long_text_splits_only_at_sentence_or_word_boundaries`: for the 1 500-character case, every chunk encodes to at most 50 tokens, and joining the chunks with spaces recovers the input's words in order.
- [ ] **Step 3:** Run them; they fail.
- [ ] **Step 4:** Implement. Tokenizer: pick a pure-Rust SentencePiece reader that loads `tokenizer.model` (or write the unigram decoder on its protobuf). The parity test is the acceptance bar, whatever you choose.
- [ ] **Step 5:** `cargo test -p sayd-pocket` passes.
- [ ] **Step 6:** Run fmt and clippy, then commit: `feat(pocket): tokenizer and sentence splitting that match the Pocket TTS package`.

### Task 4: The generation loop, voice encoding, and the package's audio handling

**Files:**
- Create: `crates/sayd-pocket/src/model.rs`, `src/rng.rs`
- Create: `crates/sayd-pocket/tests/fixtures/gen_loop_fixtures.py`, which writes `loop_*.npy`. It runs `pocket_tts_onnx.py` at temperature 0 on the fp32 graphs, recording the voice embedding for `ref_fenrir.wav` and the latents for two single-chunk sentences. Commit the outputs (a few hundred KB at most).
- Create: `crates/sayd-pocket/tests/loop_golden.rs`

**Interfaces:**
- Consumes: `Bundle`, `Sessions`, `Precision`, `Tokenizer`, `prepare_text_prompt`, `split_into_best_sentences`, `npy::read_f32` (Tasks 2–3).
- Produces:
  - `pub struct PocketTts`, `pub fn PocketTts::load(bundle_dir: &Path, p: Precision, threads: usize) -> Result<PocketTts, Error>`
  - `pub struct VoiceEmbedding { pub frames: usize, pub data: Vec<f32> /* frames × conditioning_dim */ }`, plus `to_bytes(&self) -> Vec<u8>` and `from_bytes(&[u8]) -> Result<Self, Error>` for the cache
  - `pub fn PocketTts::encode_voice(&mut self, clip_24k_mono: &[f32]) -> Result<VoiceEmbedding, Error>`. Applies `end_on_pause` first.
  - `pub struct VoiceState` (opaque; holds the prefilled flow-LM state tensors) and `pub fn PocketTts::voice_state(&mut self, e: &VoiceEmbedding) -> Result<VoiceState, Error>`
  - `pub fn PocketTts::generate(&mut self, v: &VoiceState, text: &str, temperature: f32, seed: u64) -> Result<Vec<f32>, Error>`. Splits with `split_into_best_sentences`, runs each chunk from a clone of `v`, and concatenates.
  - `pub fn end_on_pause(clip: &[f32], sample_rate: u32) -> Vec<f32>`: trim trailing silence, fade out the last 20 ms, append 80 ms of zeros. Use the package's exact threshold from `get_state_for_audio_prompt`.
  - `rng.rs`: a seeded, dependency-free Gaussian source (`pub struct Gauss`, `pub fn new(seed: u64)`, `pub fn fill(&mut self, out: &mut [f32], std: f32)`).
- Loop details are in the design's "Generation loop" section. Use the package's `frames_after_eos` (`model_recommended_frames_after_eos` or the `prepare_text_prompt` guess), **not** the wrapper's guess + 2. Apply a 5 ms linear fade-in to the first decoded audio of every chunk, as `tts_model.py` does around line 522.

- [ ] **Step 1: Write the failing tests** (`loop_golden.rs`, ignored unless `SAYD_POCKET_BUNDLE` is set):
  - `voice_embedding_matches_reference`: max abs diff < `1e-4` against `loop_voice.npy`. The fixture script feeds the reference the *same* `end_on_pause`-processed clip, so both sides see identical input.
  - `latents_match_reference_at_temperature_zero`: fp32, temperature 0; frame count equal and max abs diff < `1e-3` per sentence.
  - `pause_total_matches_package_within_half_a_second`: int8, temperature 0.3, seed 1, the `joke` line, fenrir clip. Total silence (frames below −40 dBFS in runs ≥ 250 ms) is within 0.5 s of `out/pocket/fenrir_joke.wav`'s 1.41 s. Commit that number as a constant with a comment naming its source.
  - `realtime_factor_under_one` (prints the RTF): int8, a 20-word sentence; `gen_secs / audio_secs < 1.0`.
  - Unit tests (not ignored): `end_on_pause_trims_fades_and_pads` on a synthetic tone followed by silence; `fade_in_is_5ms_linear`; `gauss_is_deterministic_per_seed`.
- [ ] **Step 2:** Run them; they fail.
- [ ] **Step 3:** Implement `model.rs` and `rng.rs`. Read `pocket_tts_onnx.py` lines 176–530 for the tensor names and state threading, and `pocket_tts/models/tts_model.py` for `end_on_pause` and the fade.
- [ ] **Step 4:** All tests pass, including with `--ignored` and `SAYD_POCKET_BUNDLE` set. Paste the printed RTF into the commit message.
- [ ] **Step 5:** Run fmt and clippy, then commit: `feat(pocket): Pocket TTS inference in Rust, matching the reference at temperature 0`.

### Task 5: `PocketSynthesizer`: voices directory, clip loading, encoder cache

**Files:**
- Create: `crates/sayd/src/pocket_synth.rs`
- Modify: `crates/sayd/Cargo.toml` (adds `sayd-pocket.workspace = true`, plus a small pure-Rust WAV reader such as `hound`); `crates/sayd/src/main.rs` (`mod pocket_synth;`)

**Interfaces:**
- Consumes: `PocketTts`, `VoiceEmbedding`, `VoiceState`, `Precision`, `bundle_dir` (Tasks 2–4); `crate::resample` for clips that aren't 24 kHz; `sayd_kokoro::audio::time_stretch` for `speed != 1.0`.
- Produces:
  - `pub struct PocketSynthesizer`, `pub fn PocketSynthesizer::new(models_dir: &Path, cache_dir: &Path, cfg: &Config) -> PocketSynthesizer`
  - `pub fn voices_dir(models_dir: &Path) -> PathBuf` = `models_dir/pocket/voices`
  - `pub fn list_pocket_voices(models_dir: &Path) -> Vec<String>` (sorted `*.wav` stems)
  - `pub fn bundle_installed(models_dir: &Path) -> bool`
  - `pub fn default_cache_dir() -> PathBuf` = `$XDG_CACHE_HOME/sayd/pocket`, falling back to `~/.cache/sayd/pocket`
  - `impl Synthesizer`. `phonemize` returns the text with `’`/`‘` replaced. `fits` returns `true`, since splitting happens inside `PocketTts::generate`. `synth(text, voice, speed)` loads lazily, encodes the clip (cached as `<cache_dir>/<sha256 of clip bytes>.f32`), builds the `VoiceState` (memoised per voice name plus hash), generates at `cfg.pocket.temperature` with a seed taken from a counter, and time-stretches when `speed != 1.0`. `unload` drops everything. `voice_exists(v)` checks that `voices_dir/<v>.wav` exists. `reconfigure` stores `pocket.temperature` without a reload and returns `true` iff `pocket.precision` or `threads` changed.
- A `sha2` dependency is fine for the hash.

- [ ] **Step 1: Write the failing tests** (real-model ones skip when `SAYD_POCKET_BUNDLE` is unset; point `models_dir` at a temp dir with the bundle symlinked in):
  - `voice_exists_reflects_the_voices_directory`
  - `clip_at_44k1_stereo_is_resampled_and_end_on_pause_applied`: a 44.1 kHz stereo WAV that ends on speech synthesizes. Within the first 5 ms, the output's peak stays below its overall 99th-percentile level, so there's no burst.
  - `replaced_clip_is_reencoded_and_deleted_clip_is_reported_missing`: synthesize with `a.wav` and record the cache files. Overwrite `a.wav` with different audio and synthesize again: a new cache file appears. Delete it: `voice_exists("a")` is false, and `synth` returns `Err` naming the file.
  - `speed_other_than_one_time_stretches`: at `speed = 1.5` the output is about 1/1.5 the length (±10%).
- [ ] **Step 2:** Run them; they fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** The tests pass via `scripts/headless.sh` (or `cargo test -p sayd pocket_synth` where no display is needed).
- [ ] **Step 5:** Run fmt and clippy, then commit: `feat(pocket): a Synthesizer that speaks with a voice cloned from a clip`.

### Task 6: `SwitchingSynthesizer`, wiring, startup checks, D-Bus

**Files:**
- Create: `crates/sayd/src/switch_synth.rs`
- Modify: `crates/sayd/src/main.rs` (~843 constructs the synth; ~853 has the startup voice warning); `crates/sayd/src/dbus.rs` (an `Engine` property next to `voice` at ~402); `crates/sayd-cli/src/main.rs` (`status` prints the engine)

**Interfaces:**
- Consumes: `KokoroSynthesizer`, `PocketSynthesizer`, `list_pocket_voices`, `bundle_installed`, `Config::pocket_active`, `Snapshot.engine`.
- Produces:
  - `pub struct SwitchingSynthesizer { kokoro: Box<dyn Synthesizer>, pocket: Box<dyn Synthesizer>, active: ActiveEngine, pending: Option<ActiveEngine>, kokoro_voice: String, pocket_voice: String, warned: HashSet<String> }`. Its constructor `new(kokoro, pocket, cfg)` takes `Box<dyn Synthesizer>` so tests can pass stubs. `enum ActiveEngine { Kokoro, Pocket }` (not `Engine`, which is `sayd_core::engine::Engine`).
  - **Switching:** `reconfigure(cfg)` forwards to both inner synthesizers, updates `kokoro_voice`/`pocket_voice` from `cfg.voice`/`cfg.pocket.voice`, and records `pending` when `cfg.pocket_active()` differs from `active`. It returns `true` if a switch is pending or the active inner one asked for a reload. `unload()` unloads the active synthesizer, then makes `pending` active. The engine already defers `unload` until no utterance is mid-flight (`engine.rs` ~390), which gives the between-utterances guarantee.
  - **Voice resolution (user decision, option A):** with Pocket active, a voice that `pocket.voice_exists` accepts is used as is; anything else becomes `pocket_voice`. With Kokoro active, the same rule applies the other way round with `kokoro_voice`. The first substitution per name logs `eprintln!("note: voice '{v}' is not a {engine} voice; speaking with '{fallback}'")`. `voice_exists` returns true when the name is valid on the active engine *or* the fallback is, so `Engine::submit` never rejects a Kokoro name after a switch.
  - **`phonemize`** delegates to the active engine. Kokoro phonemes never reach Pocket, because the switch happens only at `unload`, between utterances.
  - **D-Bus:** a `#[zbus(property)] async fn engine(&self) -> String` that returns `snapshot().engine`. `say status` prints `engine:`, and `--json` includes `"engine"`.
  - **`SetVoice(name)`:** a Pocket clip name sets `pocket.voice`; anything else sets `voice`, as today.

- [ ] **Step 1: Write the failing tests** (stub synthesizers that record calls):
  - `switch_waits_for_unload_and_queued_kokoro_voice_is_substituted`: with Kokoro active, `reconfigure` to Pocket. Then `synth("…", "am_fenrir", 1.0)` still goes to Kokoro. After `unload()`, the same call goes to the Pocket stub with voice `"fenrir"`.
  - `voice_exists_accepts_names_valid_on_either_side_via_fallback`
  - `substitution_logs_once_per_name` (with the log sink injected as a `Vec<String>` for the test)
  - `pocket_selected_without_bundle_warns_and_rejects_cleanly`: start the daemon pieces with `engine = "pocket"` and an empty models dir. The startup check returns a warning that contains `Download Pocket TTS`. The first `synth` returns `Err` with that same hint, and the engine stays healthy for a later switch back to Kokoro.
  - In `dbus.rs`, `engine_property_reflects_config` (runs under `scripts/headless.sh`).
- [ ] **Step 2:** Run them; they fail.
- [ ] **Step 3:** Implement. Replace the direct `KokoroSynthesizer` construction in `main.rs` with `SwitchingSynthesizer`. Extend the startup warning to check `pocket.voice` and `bundle_installed` when Pocket is active.
- [ ] **Step 4:** `scripts/headless.sh` runs the `sayd` suite green, and `cargo test -p sayd-core -p sayd-pocket` passes.
- [ ] **Step 5:** Run fmt and clippy, then commit: `feat: switch between Kokoro and Pocket TTS without restarting`.

### Task 7: Download, the shipped Fenrir clip, settings rows, README

**Files:**
- Create: `crates/sayd/assets/pocket/fenrir.wav` (copy of `.tts-bench/ref_fenrir.wav`, 24 kHz mono, 16-bit PCM), `crates/sayd/assets/pocket/fenrir.txt` (its transcript, from the design)
- Modify: `crates/sayd/src/settings/download.rs` (`assets()` at ~200, `download()` at ~374); `settings/schema.rs` (`ROOT`, ~419); `settings/model.rs` (`voices()`/`rescan_voices()` at ~483); `settings/window.rs` (a new `pocket_download_row` next to `voice_download_row` at ~1222); `README.md`

**Interfaces:**
- Consumes: `bundle_dir`, `voices_dir`, `list_pocket_voices`, `bundle_installed` (Tasks 2, 5).
- Produces:
  - `download.rs`: `pub const POCKET_BASE_URL` (the pinned URL from Global Constraints); `pub fn pocket_assets() -> Vec<Asset>` (the int8 set plus `bundle.json`, `tokenizer.model`, `bos_before_voice.npy`, `text_conditioner.onnx`, `mimi_encoder.onnx`, with byte sizes from the HF tree listing); `pub fn pocket_offer_subtitle() -> String` = `"165 MB from huggingface.co: the Pocket TTS model and the Fenrir voice clip"`, computed from the sizes the way `offer_subtitle` is. `download()` becomes a wrapper over `pub fn download_assets(models_dir, assets: &[Asset], fetch, cancel, report) -> Outcome` with unchanged semantics. A successful Pocket download then writes the embedded `fenrir.wav` (`include_bytes!`) into `voices_dir` if it is absent.
  - `schema.rs`: in "Voice and speed", an **Engine** `Row::Choice` with `Options::Static(&[("kokoro", "Kokoro"), ("pocket", "Pocket TTS")])` and get/set on `engine`. A new group **"Pocket TTS"** (description: `"Used when Engine is Pocket TTS. The voice is copied from a short .wav clip in <models>/pocket/voices"`) holds a **Voice clip** `Row::Choice` (`Options::Discovered(|ui| ui.pocket_voices())`, unknown `"‘{v}’ — no clip in pocket/voices"`, get/set on `pocket.voice`), then `Row::Custom(window::pocket_download_row)`. The existing Kokoro **Voice** row is unchanged. *(Plan call: the schema has no per-row visibility, so a separate group replaces the design's engine-dependent dropdown.)*
  - `model.rs`: `pub fn pocket_voices(&self) -> Vec<String>`, `pub fn rescan_pocket_voices(&self) -> Vec<String>`, `pub fn download_pocket(&self, cancel) -> Receiver<download::Event>`, mirroring their Kokoro twins.
  - README: a "Pocket TTS" section covering what it is, the engine switch, the voices directory, adding your own clip, the licence (CC-BY-4.0, Kyutai; the export by KevinAHM; Fenrir clip from Kokoro `am_fenrir`, Apache-2.0), and that the download is the only network access. Add the Pocket-specific manual checks to the existing numbered manual test list.

- [ ] **Step 1: Write the failing tests:**
  - `pocket_assets_are_pinned_and_sized`: every URL starts with `POCKET_BASE_URL`, which contains `58a6d00cf13d239b6748cb0769f35c580a8f606c`; the expected sizes sum to the number the subtitle rounds; and no `flow_lm_main.onnx` (fp32) is in the int8 set.
  - `pocket_download_installs_fenrir_clip_once`: use the existing fake `Fetch` from `download.rs`'s tests. After success, `voices_dir/fenrir.wav` exists and equals the embedded bytes. A user-modified `fenrir.wav` isn't overwritten on a second run.
  - `schema_has_engine_row_and_pocket_group` (alongside the schema's existing tree-integrity tests)
  - `kokoro_download_unchanged`: `assets()` is still the same 32 entries in the same order.
- [ ] **Step 2:** Run them; they fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `scripts/headless.sh` is green. Manually: open settings, switch Engine to Pocket TTS, press Download Pocket TTS, and Test speaks in the Fenrir clone. Take the screenshot on a virtual sway output, per the screenshot memory.
- [ ] **Step 5:** Run fmt and clippy, then commit: `feat(settings): choose the engine, download Pocket TTS, pick a voice clip`.
