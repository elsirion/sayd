//! The real `Synthesizer`: misaki-en/espeak G2P plus a Kokoro ONNX session.
//!
//! The session is created lazily and dropped on `unload`, which is what makes
//! the idle-unload policy actually return the ~1.27 GB it holds.

use std::path::{Path, PathBuf};

use sayd_core::config::Config;
use sayd_core::synth::Synthesizer;
use sayd_g2p::{Dialect, Phonemizer};
use sayd_kokoro::Kokoro;

pub struct KokoroSynthesizer {
    models_dir: PathBuf,
    model_file: String,
    threads: usize,
    phonemizer: Phonemizer,
    session: Option<Kokoro>,
    /// Voices loaded into the live session.
    loaded_voices: Vec<String>,
}

fn model_file_for(model: &str) -> &'static str {
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
        k.synth(phonemes, voice, speed).map_err(|e| e.to_string())
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
}

#[cfg(all(test, feature = "models"))]
mod models_tests {
    use super::*;
    use std::path::Path;

    fn models_dir() -> &'static Path {
        Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../models"))
    }

    /// The real end-to-end proof available without an audio device: text in,
    /// samples out, through the actual ONNX session and G2P frontend.
    #[test]
    fn synth_produces_plausible_length_audio_from_real_text() {
        let cfg = Config::default();
        let mut s = KokoroSynthesizer::new(models_dir(), &cfg).expect("synthesizer constructs");

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
        let s = KokoroSynthesizer::new(models_dir(), &cfg).expect("synthesizer constructs");
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
        let mut s = KokoroSynthesizer::new(models_dir(), &cfg).expect("synthesizer constructs");

        let us = s.phonemize("tomato", "af_heart");
        let gb = s.phonemize("tomato", "bf_emma");
        assert_ne!(
            us, gb,
            "British voice bf_emma must not collapse into the American phonemization"
        );
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

    fn models_dir() -> &'static Path {
        Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../models"))
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
        let synth = KokoroSynthesizer::new(models_dir(), &cfg).expect("synthesizer constructs");
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
