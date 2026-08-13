//! The real `Synthesizer`: misaki-en/espeak G2P plus a Kokoro ONNX session.
//!
//! The session is created lazily and dropped on `unload`, which is what makes
//! the idle-unload policy actually return the ~1.27 GB it holds.

use std::path::{Path, PathBuf};

use g2p::{Dialect, Phonemizer};
use kokoro::Kokoro;
use sayd_core::config::Config;
use sayd_core::synth::Synthesizer;

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
        self.session.as_mut().ok_or_else(|| "session missing".to_string())
    }
}

impl Synthesizer for KokoroSynthesizer {
    fn phonemize(&mut self, text: &str, voice: &str) -> String {
        self.phonemizer.phonemize(text, dialect_for(voice))
    }

    fn fits(&mut self, phonemes: &str) -> bool {
        match self.ensure_session() {
            Ok(k) => k.tokenize(phonemes).len() < kokoro::MAX_TOKENS,
            // If the model will not load, do not also block chunking; the
            // failure surfaces from `synth`.
            Err(_) => phonemes.chars().count() <= kokoro::MAX_TOKENS,
        }
    }

    fn synth(&mut self, phonemes: &str, voice: &str, speed: f32) -> Result<Vec<f32>, String> {
        // `ensure_session` only needs to run for its side effect here (make
        // sure `self.session` is populated); its returned `&mut Kokoro`
        // borrow is not kept alive across the `loaded_voices` mutation below.
        self.ensure_session()?;

        if !self.loaded_voices.contains(&voice.to_string()) {
            let k = self.session.as_mut().ok_or_else(|| "session missing".to_string())?;
            k.load_voice(voice).map_err(|e| e.to_string())?;
            // The previous borrow of `k` ends at the statement above, so
            // mutating `loaded_voices` here does not conflict with it.
            self.loaded_voices.push(voice.to_string());
        }

        let k = self.session.as_mut().ok_or_else(|| "session missing".to_string())?;
        k.synth(phonemes, voice, speed).map_err(|e| e.to_string())
    }

    fn sample_rate(&self) -> u32 {
        kokoro::SAMPLE_RATE
    }

    fn unload(&mut self) {
        self.session = None;
        self.loaded_voices.clear();
    }

    fn is_loaded(&self) -> bool {
        self.session.is_some()
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
        assert!(!phonemes.is_empty(), "expected non-empty phonemes for real text");

        let audio = s.synth(&phonemes, "af_heart", 1.0).expect("synth succeeds");
        assert!(!audio.is_empty(), "expected non-empty audio");

        // Sanity bound on length: this sentence should produce somewhere
        // between roughly half a second and 20 seconds of audio at 24 kHz.
        // A wildly wrong value (e.g. one frame, or silence-length garbage)
        // would fall well outside this window.
        let seconds = audio.len() as f64 / kokoro::SAMPLE_RATE as f64;
        assert!(
            (0.5..20.0).contains(&seconds),
            "synthesized audio duration {seconds}s is not plausible for this text"
        );
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
