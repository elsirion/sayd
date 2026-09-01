//! The real `Synthesizer`: misaki-en/espeak G2P plus a Kokoro ONNX session.
//!
//! The session is created lazily and dropped on `unload`, which is what makes
//! the idle-unload policy actually return the ~1.27 GB it holds.

use std::path::{Path, PathBuf};

use sayd_core::config::Config;
use sayd_core::synth::Synthesizer;
use sayd_g2p::{Dialect, Phonemizer};
use sayd_kokoro::audio::time_stretch;
use sayd_kokoro::Kokoro;

pub struct KokoroSynthesizer {
    models_dir: PathBuf,
    model_file: String,
    threads: usize,
    /// `"model"` | `"stretch"`; see `Config::speed_mode`'s doc comment for
    /// the measured trade-off. Stored verbatim rather than parsed into an
    /// enum: anything other than `"stretch"` takes the `"model"` path, the
    /// same forward-compatible "unrecognised falls back to today's
    /// behaviour" shape `model_file_for` uses for `model`, so a config
    /// written by a future `sayd` with a third mode still runs here instead
    /// of refusing to load.
    speed_mode: String,
    phonemizer: Phonemizer,
    session: Option<Kokoro>,
    /// Voices loaded into the live session.
    loaded_voices: Vec<String>,
}

/// `pub(crate)` so `settings::model`'s tests can pin
/// `settings::model::FALLBACK_MODEL` against the file this actually loads
/// for an unrecognised string -- the whole premise of normalising an unknown
/// `model` to fp32 rather than to something else.
pub(crate) fn model_file_for(model: &str) -> &'static str {
    match model {
        "fp16" => "model_fp16.onnx",
        "q8" => "model_quantized.onnx",
        _ => "model.onnx",
    }
}

/// British voice packs are the `bf_`/`bm_` prefixes. misaki-en only vendors
/// US lexicons, so routing those through it would make every British voice
/// speak rhotic American; British text must take the whole-text espeak
/// `en-gb` path instead (see `g2p`'s module docs).
fn dialect_for(voice: &str) -> Dialect {
    if voice.starts_with("bf_") || voice.starts_with("bm_") {
        Dialect::British
    } else {
        Dialect::American
    }
}

impl KokoroSynthesizer {
    pub fn new(models_dir: &Path, cfg: &Config) -> Result<Self, String> {
        Ok(KokoroSynthesizer {
            models_dir: models_dir.to_path_buf(),
            model_file: model_file_for(&cfg.model).to_string(),
            threads: cfg.threads,
            speed_mode: cfg.speed_mode.clone(),
            phonemizer: Phonemizer::new(),
            session: None,
            loaded_voices: Vec::new(),
        })
    }

    fn ensure_session(&mut self) -> Result<&mut Kokoro, String> {
        if self.session.is_none() {
            let k = Kokoro::new(&self.models_dir, &self.model_file, self.threads)
                .map_err(|e| e.to_string())?;
            self.session = Some(k);
            self.loaded_voices.clear();
        }
        self.session
            .as_mut()
            .ok_or_else(|| "session missing".to_string())
    }
}

impl Synthesizer for KokoroSynthesizer {
    fn phonemize(&mut self, text: &str, voice: &str) -> String {
        self.phonemizer.phonemize(text, dialect_for(voice))
    }

    fn fits(&mut self, phonemes: &str) -> bool {
        match self.ensure_session() {
            Ok(k) => k.tokenize(phonemes).len() < sayd_kokoro::MAX_TOKENS,
            // If the model will not load, do not also block chunking; the
            // failure surfaces from `synth`.
            Err(_) => phonemes.chars().count() <= sayd_kokoro::MAX_TOKENS,
        }
    }

    fn synth(&mut self, phonemes: &str, voice: &str, speed: f32) -> Result<Vec<f32>, String> {
        // `ensure_session` only needs to run for its side effect here (make
        // sure `self.session` is populated); its returned `&mut Kokoro`
        // borrow is not kept alive across the `loaded_voices` mutation below.
        self.ensure_session()?;

        if !self.loaded_voices.contains(&voice.to_string()) {
            let k = self
                .session
                .as_mut()
                .ok_or_else(|| "session missing".to_string())?;
            k.load_voice(voice).map_err(|e| e.to_string())?;
            // The previous borrow of `k` ends at the statement above, so
            // mutating `loaded_voices` here does not conflict with it.
            self.loaded_voices.push(voice.to_string());
        }

        let k = self
            .session
            .as_mut()
            .ok_or_else(|| "session missing".to_string())?;

        if self.speed_mode == "stretch" {
            // Synthesize at the model's own native tempo, then stretch the
            // result to the requested factor -- see `Config::speed_mode`'s
            // doc comment for why this exists. `time_stretch` itself already
            // returns its input unchanged within 1e-6 of `factor == 1.0`, so
            // this costs nothing extra at the default speed beyond the
            // wasted `speed` argument to `k.synth`.
            let audio = k.synth(phonemes, voice, 1.0).map_err(|e| e.to_string())?;
            Ok(time_stretch(&audio, speed))
        } else {
            k.synth(phonemes, voice, speed).map_err(|e| e.to_string())
        }
    }

    fn sample_rate(&self) -> u32 {
        sayd_kokoro::SAMPLE_RATE
    }

    fn unload(&mut self) {
        self.session = None;
        self.loaded_voices.clear();
    }

    fn is_loaded(&self) -> bool {
        self.session.is_some()
    }

    /// A cheap filesystem check -- no session, no model load -- for whether
    /// `voice` has an installed voice pack. Voice packs live at
    /// `<models_dir>/voices/<voice>.bin` (see `load_voice`'s call site in
    /// `sayd_kokoro::Kokoro`, which this mirrors); existence of that file is
    /// exactly what `load_voice` itself needs to succeed.
    fn voice_exists(&self, voice: &str) -> bool {
        self.models_dir
            .join("voices")
            .join(format!("{voice}.bin"))
            .is_file()
    }

    fn reconfigure(&mut self, cfg: &Config) -> bool {
        let file = model_file_for(&cfg.model).to_string();
        let changed = file != self.model_file || cfg.threads != self.threads;
        self.model_file = file;
        self.threads = cfg.threads;
        // Deliberately not part of `changed`: `speed_mode` only decides which
        // branch `synth` takes on the *next* call, and does not touch the
        // loaded ORT session at all -- dropping the ~1.27 GB session because
        // someone toggled this would be a bug, the same one a model or
        // thread-count change is right to cause and this is not.
        self.speed_mode = cfg.speed_mode.clone();
        changed
    }
}

#[cfg(all(test, feature = "models"))]
mod models_tests {
    use super::*;
    use std::path::Path;

    fn models_dir() -> std::path::PathBuf {
        sayd_kokoro::default_models_dir()
    }

    /// The real end-to-end proof available without an audio device: text in,
    /// samples out, through the actual ONNX session and G2P frontend.
    #[test]
    fn synth_produces_plausible_length_audio_from_real_text() {
        let cfg = Config::default();
        let mut s = KokoroSynthesizer::new(&models_dir(), &cfg).expect("synthesizer constructs");

        let text = "Hello there. This is sayd speaking from the engine.";
        let phonemes = s.phonemize(text, "af_heart");
        assert!(
            !phonemes.is_empty(),
            "expected non-empty phonemes for real text"
        );

        let audio = s.synth(&phonemes, "af_heart", 1.0).expect("synth succeeds");
        assert!(!audio.is_empty(), "expected non-empty audio");

        // Sanity bound on length: this sentence should produce somewhere
        // between roughly half a second and 20 seconds of audio at 24 kHz.
        // A wildly wrong value (e.g. one frame, or silence-length garbage)
        // would fall well outside this window.
        let seconds = audio.len() as f64 / sayd_kokoro::SAMPLE_RATE as f64;
        assert!(
            (0.5..20.0).contains(&seconds),
            "synthesized audio duration {seconds}s is not plausible for this text"
        );
    }

    /// M21: `voice_exists` must agree with what `load_voice`/`synth` would
    /// actually accept, checked against the real `models/voices` directory
    /// rather than a fake one -- a known voice pack exists there and an
    /// obviously-bogus name does not.
    #[test]
    fn voice_exists_matches_the_real_voices_directory() {
        let cfg = Config::default();
        let s = KokoroSynthesizer::new(&models_dir(), &cfg).expect("synthesizer constructs");
        assert!(
            s.voice_exists("af_heart"),
            "af_heart.bin ships in models/voices"
        );
        assert!(!s.voice_exists("totally_bogus_name"));
    }

    /// Regression guard for Correction 1: American and British voices must
    /// take different phonemization paths. "tomato" has a well-known
    /// transatlantic pronunciation split, so their phoneme strings must
    /// differ -- if they don't, British packs have silently fallen back to
    /// misaki-en's US-only lexicons again.
    #[test]
    fn american_and_british_voices_produce_different_phonemes() {
        let cfg = Config::default();
        let mut s = KokoroSynthesizer::new(&models_dir(), &cfg).expect("synthesizer constructs");

        let us = s.phonemize("tomato", "af_heart");
        let gb = s.phonemize("tomato", "bf_emma");
        assert_ne!(
            us, gb,
            "British voice bf_emma must not collapse into the American phonemization"
        );
    }

    /// Only `model` and `threads` may invalidate a loaded session -- this
    /// needs no model load, just the field bookkeeping `reconfigure` does
    /// before any session is ever created.
    #[test]
    fn reconfigure_reports_a_reload_only_when_the_model_or_thread_count_moves() {
        let cfg = Config::default();
        let mut s = KokoroSynthesizer::new(&models_dir(), &cfg).expect("synthesizer constructs");

        let same = Config {
            voice: "am_fenrir".into(),
            ..Config::default()
        };
        assert!(!s.reconfigure(&same), "a voice change needs no reload");

        let stretch = Config {
            speed_mode: "stretch".into(),
            ..Config::default()
        };
        assert!(
            !s.reconfigure(&stretch),
            "speed_mode picks a synth-time branch, not a session rebuild"
        );

        let q8 = Config {
            model: "q8".into(),
            ..Config::default()
        };
        assert!(s.reconfigure(&q8), "a model change needs a reload");

        let mut threads = q8.clone();
        threads.threads = 4;
        assert!(
            s.reconfigure(&threads),
            "a thread-count change needs a reload"
        );
        assert!(
            !s.reconfigure(&threads),
            "reapplying the same config does not"
        );
    }

    /// Segments `audio` the way the leading-word investigation did: 10 ms RMS
    /// windows, a voiced threshold at 6% of the utterance's own peak window
    /// RMS, gaps under 40 ms merged into one segment. Returns each segment's
    /// mean level in dB relative to the utterance's peak.
    fn segment_levels_db(audio: &[f32]) -> Vec<f32> {
        let win = sayd_kokoro::SAMPLE_RATE as usize / 100; // 10 ms
        let rms: Vec<f32> = audio
            .chunks(win)
            .map(|c| (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).sqrt())
            .collect();
        let peak = rms.iter().cloned().fold(0.0f32, f32::max);
        let threshold = peak * 0.06;
        let voiced: Vec<bool> = rms.iter().map(|&r| r > threshold).collect();

        const MERGE_GAP_WINDOWS: usize = 4; // 40 ms at a 10 ms window
        let mut segments: Vec<(usize, usize)> = Vec::new();
        let mut i = 0;
        while i < voiced.len() {
            if !voiced[i] {
                i += 1;
                continue;
            }
            let start = i;
            let mut end = i + 1;
            loop {
                let gap_end = (end + MERGE_GAP_WINDOWS).min(voiced.len());
                match voiced[end..gap_end].iter().position(|&v| v) {
                    Some(offset) => end = end + offset + 1,
                    None => break,
                }
            }
            segments.push((start, end));
            i = end;
        }

        segments
            .into_iter()
            .map(|(a, b)| {
                let mean = rms[a..b].iter().sum::<f32>() / (b - a) as f32;
                20.0 * (mean / peak).log10()
            })
            .collect()
    }

    /// Verifies the fix the whole feature exists for: at `speed = 1.3`,
    /// `af_heart`, Kokoro's own `speed` input (`speed_mode = "model"`)
    /// renders the pangram's leading "The" up to 10 dB quieter than the
    /// following "quick" (see `Config::speed_mode`'s doc comment).
    /// `speed_mode = "stretch"` synthesizes at 1.0 -- where "The" is not
    /// suppressed -- and stretches the result instead, so it must not
    /// reproduce the drop.
    ///
    /// Both modes are driven through the *same* `KokoroSynthesizer`,
    /// `reconfigure`d in between: besides being the realistic path (the
    /// window flips this live), it is also what proves `reconfigure` really
    /// does leave `speed_mode` free of a session reload -- if it silently
    /// reloaded, this test would still pass, but much more slowly.
    #[test]
    fn stretch_mode_keeps_the_leading_word_as_loud_as_model_mode_drops_it() {
        let cfg = Config::default();
        let mut s = KokoroSynthesizer::new(&models_dir(), &cfg).expect("synthesizer constructs");
        let text = "The quick brown fox jumps over the lazy dog.";
        let phonemes = s.phonemize(text, "af_heart");

        let model_audio = s
            .synth(&phonemes, "af_heart", 1.3)
            .expect("model-mode synth");

        let stretch_cfg = Config {
            speed_mode: "stretch".into(),
            ..Config::default()
        };
        assert!(
            !s.reconfigure(&stretch_cfg),
            "speed_mode must not force a session reload"
        );
        let stretch_audio = s
            .synth(&phonemes, "af_heart", 1.3)
            .expect("stretch-mode synth");

        let model_segs = segment_levels_db(&model_audio);
        let stretch_segs = segment_levels_db(&stretch_audio);
        assert!(
            model_segs.len() >= 2 && stretch_segs.len() >= 2,
            "expected at least 'The' and 'quick' as separate segments in both \
             modes: model={model_segs:?} stretch={stretch_segs:?}"
        );

        // "The" relative to "quick" -- strongly negative means "The" all but
        // vanished next to its neighbour, near zero means the two are
        // comparable. This is the number the task's measurement table
        // reports as roughly -10 dB for model mode at 1.3.
        let model_gap = model_segs[0] - model_segs[1];
        let stretch_gap = stretch_segs[0] - stretch_segs[1];
        eprintln!(
            "leading word ('The') relative to the next ('quick') at speed 1.3: \
             model_mode={model_gap:.1} dB, stretch_mode={stretch_gap:.1} dB \
             (model levels {model_segs:?}, stretch levels {stretch_segs:?})"
        );

        // Measured on this build/model at 5.2 dB (model -9.0 dB, stretch
        // -3.8 dB); the bound below leaves headroom for a different ORT
        // build or thread count to shift the exact figures without turning
        // this into a flaky test, while still failing if the fix regresses
        // to a difference too small to matter.
        assert!(
            stretch_gap > model_gap + 3.0,
            "stretch mode should leave 'The' meaningfully louder relative to \
             'quick' than model mode does: model={model_gap:.1} dB \
             stretch={stretch_gap:.1} dB"
        );
    }

    /// Utterances are synthesized chunk by chunk (`sayd_core::chunk::chunk`),
    /// so `speed_mode = "stretch"` stretches per chunk, independently, and
    /// the results are concatenated. `time_stretch`'s own fallback (see its
    /// doc comment in `sayd_kokoro::audio`) keeps the very first sample of
    /// each chunk from being forced to silence, but WSOLA re-anchors its
    /// alignment search from scratch on every call, so nothing guarantees
    /// chunk N's stretched tail lines up sample-for-sample with chunk N+1's
    /// stretched head -- this measures how big that residual seam is on
    /// real, two-sentence speech, rather than asserting a specific bound a
    /// different voice or sentence pair could falsify.
    #[test]
    fn per_chunk_stretch_seam_on_real_speech_is_measured_not_assumed() {
        let cfg = Config::default();
        let mut s = KokoroSynthesizer::new(&models_dir(), &cfg).expect("synthesizer constructs");
        let stretch_cfg = Config {
            speed_mode: "stretch".into(),
            ..Config::default()
        };
        assert!(
            !s.reconfigure(&stretch_cfg),
            "speed_mode must not force a session reload"
        );

        // Two sentences, synthesized (and so stretched) as two separate
        // chunks, exactly as `sayd_core::chunk::chunk` would split them with
        // a `target_chars` short enough to force the split -- the same
        // technique `engine.rs`'s own multi-chunk tests use.
        let chunks = sayd_core::chunk::chunk(
            "A package arrived for you this morning. The quick brown fox jumps over the lazy dog.",
            25,
        );
        assert!(
            chunks.len() >= 2,
            "expected the text to split into >= 2 chunks"
        );

        let mut audio = Vec::new();
        let mut boundaries = Vec::new();
        for c in &chunks {
            let phonemes = s.phonemize(&c.text, "af_heart");
            let out = s
                .synth(&phonemes, "af_heart", 1.3)
                .expect("stretch-mode synth");
            if !audio.is_empty() {
                boundaries.push(audio.len());
            }
            audio.extend_from_slice(&out);
        }

        let max_delta = |a: &[f32]| -> f32 {
            a.windows(2)
                .map(|w| (w[1] - w[0]).abs())
                .fold(0.0, f32::max)
        };
        // A generous window either side of each seam, and the same-sized
        // window well inside the surrounding chunk, so the two are measuring
        // like against like.
        let radius = sayd_kokoro::SAMPLE_RATE as usize / 200; // 5 ms
        for &b in &boundaries {
            let lo = b.saturating_sub(radius);
            let hi = (b + radius).min(audio.len());
            let seam = max_delta(&audio[lo..hi]);
            let interior_lo = lo.saturating_sub(4 * radius);
            let interior_hi = lo.saturating_sub(2 * radius).max(interior_lo + 1);
            let interior = max_delta(&audio[interior_lo..interior_hi]);
            eprintln!(
                "chunk boundary at sample {b}: seam max |delta|={seam:.4}, \
                 nearby interior max |delta|={interior:.4} (ratio {:.1}x)",
                if interior > 1e-6 {
                    seam / interior
                } else {
                    f32::INFINITY
                }
            );
        }
    }
}

/// I3: every `sayd-core` engine test asserts a sample *count*, never
/// content, and `StubSynthesizer` only ever emits `vec![0.0; n]` -- so an
/// engine that pushed all zeros, or the wrong buffer entirely, would pass
/// every one of them. `models_tests` above proves `KokoroSynthesizer`
/// itself produces real audio, but calls it directly, bypassing `Engine`
/// entirely. This module closes both gaps at once: a real `Engine`, wired to
/// the real `KokoroSynthesizer`, driven by nothing but `submit`/`tick`/
/// `snapshot` -- the same surface the binary and every `sayd-core` test use
/// -- checked for audio that is both non-silent and a plausible length.
///
/// `sayd-core` cannot depend on `kokoro`/`ort` (see the workspace
/// constraints), so this cannot live in `engine.rs`; it lives here instead,
/// next to the other `models`-gated tests, and reaches `Engine` purely
/// through `sayd-core`'s public API.
#[cfg(all(test, feature = "models"))]
mod engine_models_tests {
    use std::sync::{Arc, Mutex};

    use sayd_core::audio::{AudioSink, VecSink};
    use sayd_core::config::Config;
    use sayd_core::engine::{Engine, SayOpts, State};

    use super::*;

    fn models_dir() -> std::path::PathBuf {
        sayd_kokoro::default_models_dir()
    }

    /// `Engine` keeps its sink behind a private `Box<dyn AudioSink>`, so
    /// this test needs a handle it can drain (simulate playback) from
    /// outside after handing the sink away -- the same technique
    /// `sayd-core`'s own `engine.rs` tests use for the same reason
    /// (`SharedVecSink` there), reimplemented locally here because it
    /// exists purely to model playback in tests, not as part of the
    /// production `AudioSink` API `sayd-core` exports.
    struct DrainableSink(Arc<Mutex<VecSink>>);

    impl AudioSink for DrainableSink {
        fn push(&mut self, samples: &[f32]) -> usize {
            self.0.lock().unwrap().push(samples)
        }
        fn pending(&self) -> usize {
            self.0.lock().unwrap().pending()
        }
        fn clear(&mut self) {
            self.0.lock().unwrap().clear()
        }
        fn set_paused(&mut self, paused: bool) {
            self.0.lock().unwrap().set_paused(paused)
        }
        fn is_paused(&self) -> bool {
            self.0.lock().unwrap().is_paused()
        }
        fn capacity(&self) -> usize {
            self.0.lock().unwrap().capacity()
        }
        fn total_written(&self) -> usize {
            self.0.lock().unwrap().total_written()
        }
    }

    /// End-to-end through the real `Engine`: submit real text, tick to
    /// completion, and check the sink actually received non-silent audio of
    /// a plausible duration -- not just a sample count.
    ///
    /// This also pins C1's drain gate, deliberately: a `VecSink` never
    /// empties on its own, so if `Engine` still declared `Idle` the instant
    /// synthesis finished (the bug C1 fixed) this test would never get to
    /// exercise the "drain, then Idle" half below at all. Driving that
    /// through the real synthesizer and a real (if manually driven) sink is
    /// exactly the "exercises both" the fix's report asks for.
    #[test]
    fn engine_produces_non_silent_audio_of_plausible_duration() {
        let cfg = Config::default();
        let synth = KokoroSynthesizer::new(&models_dir(), &cfg).expect("synthesizer constructs");
        let sink = Arc::new(Mutex::new(VecSink::new(24_000 * 30)));
        let mut e = Engine::new(cfg, Box::new(synth), Box::new(DrainableSink(sink.clone())));

        let text = "Hello there. This is sayd speaking from the engine.";
        e.submit(text.into(), SayOpts::default())
            .expect("well-formed text is accepted");

        // Tick the real engine to completion: nothing left queued, nothing
        // still in flight. The bound is generous because this drives real
        // ONNX inference, not a stub -- a short sentence like this finishes
        // in a handful of ticks in practice.
        let mut finished = false;
        for _ in 0..5000 {
            e.tick();
            let s = e.snapshot();
            if s.queue_len == 0 && s.current_id == 0 {
                finished = true;
                break;
            }
        }
        assert!(finished, "synthesis did not finish within the tick budget");

        // C1: audio is fully synthesized and sitting in the sink, but this
        // sink never drains on its own -- the engine must still report
        // Speaking, not Idle, until something actually plays it.
        let s = e.snapshot();
        assert_eq!(
            s.state,
            State::Speaking,
            "engine must stay Speaking while synthesized audio is still pending in the sink"
        );

        let written = sink.lock().unwrap().written.clone();
        assert!(
            !written.is_empty(),
            "expected some audio to have been written"
        );
        assert!(
            written.iter().any(|&x| x != 0.0),
            "expected non-silent audio from the real synthesizer, got all zeros"
        );

        // Same sanity window as `synth_produces_plausible_length_audio_from_
        // real_text` above, for the same sentence driven through the same
        // model -- a wildly wrong value (silence-length garbage, or a
        // single frame) would fall well outside it.
        let seconds = written.len() as f64 / sayd_kokoro::SAMPLE_RATE as f64;
        let nonzero = written.iter().filter(|&&x| x != 0.0).count();
        eprintln!(
            "engine_produces_non_silent_audio_of_plausible_duration: {} samples ({seconds:.3}s), \
             {nonzero} non-zero ({:.1}%)",
            written.len(),
            100.0 * nonzero as f64 / written.len() as f64
        );
        assert!(
            (0.5..20.0).contains(&seconds),
            "synthesized audio duration {seconds}s is not plausible for this sentence"
        );

        // The other half of C1: drain the sink (simulate playback finishing)
        // and confirm the engine settles to Idle.
        sink.lock().unwrap().drain(usize::MAX);
        e.tick();
        assert_eq!(
            e.snapshot().state,
            State::Idle,
            "engine must go Idle once the sink has actually drained"
        );
    }
}
