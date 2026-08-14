//! The seam between the engine and actual speech synthesis.
//!
//! `sayd-core` never links ONNX. The binary supplies a real implementation;
//! tests supply `StubSynthesizer`, which returns silence of a predictable
//! length so the engine's timing, queueing and state transitions can be
//! asserted without a model or an audio device.

/// Sample rate every implementation must produce.
pub const SAMPLE_RATE: u32 = 24_000;

pub trait Synthesizer: Send {
    /// Grapheme-to-phoneme for one chunk of text.
    fn phonemize(&mut self, text: &str, voice: &str) -> String;

    /// Whether `phonemes` is within the model's per-call token budget.
    fn fits(&mut self, phonemes: &str) -> bool;

    /// Synthesize, returning mono f32 at `sample_rate()`.
    fn synth(&mut self, phonemes: &str, voice: &str, speed: f32) -> Result<Vec<f32>, String>;

    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    /// Drop the model, freeing its memory. The next `synth` must reload.
    fn unload(&mut self);

    fn is_loaded(&self) -> bool;

    /// Whether `voice` names something this synthesizer can actually
    /// synthesize with -- checked cheaply, without loading a model or
    /// session.
    ///
    /// `Engine::submit` (`sayd-core::engine`) calls this to reject an
    /// unknown voice synchronously, at submission time, instead of letting
    /// it reach `tick`'s synth path, where a failure becomes a sticky
    /// `ErrorKind::Synth` error that persists until an explicit dismiss and
    /// rejects every submission behind it -- including ones that never
    /// named a bad voice themselves. A quick, no-op-until-overridden default
    /// of `true` means test doubles that do not model a real voice list
    /// (most of them: their purpose is exercising unrelated state-machine
    /// behaviour) do not have to opt in just to keep compiling.
    fn voice_exists(&self, _voice: &str) -> bool {
        true
    }

    /// Take new settings that affect how audio is produced.
    ///
    /// Returns `true` if the change requires dropping any loaded model --
    /// the engine calls `unload` in that case, and the next `synth` reloads
    /// with the new settings. Returning `false` means the change needs no
    /// reload (or that this implementation has nothing to reconfigure).
    ///
    /// Only `model` and `threads` can require a reload. Voice, speed and
    /// every text-processing setting are read per-utterance by the engine
    /// and never reach here.
    fn reconfigure(&mut self, _cfg: &crate::config::Config) -> bool {
        false
    }
}

/// Test double. Emits silence at roughly the rate real speech occupies, so
/// duration-dependent engine behaviour is exercised without a model.
pub struct StubSynthesizer {
    token_budget: usize,
    loaded: bool,
    pub synth_calls: usize,
    pub unload_calls: usize,
    /// What `reconfigure` was last handed, so tests can assert the engine
    /// forwards a config change rather than only storing it. Seeded from
    /// `Config::default()`'s `(model, threads)` at construction, not `None`
    /// -- `StubSynthesizer` implicitly starts out "as if" constructed under
    /// the default config, mirroring how `KokoroSynthesizer::new` seeds its
    /// own `model_file`/`threads` fields from the `cfg` it is built with.
    /// Seeding this `None` instead would make the very first `reconfigure`
    /// call after construction always report `changed = true`, even when
    /// the config it is handed is the unchanged default -- indistinguishable
    /// from an actual model change to any caller.
    pub reconfigured_to: Option<(String, usize)>,
    pub reconfigure_calls: usize,
    /// `None` (the default, from `new`/`with_token_budget`) means every
    /// voice is treated as usable -- the vast majority of engine tests pass
    /// an arbitrary voice string while exercising behaviour that has
    /// nothing to do with voice validation, and making all of them
    /// enumerate a voice list just to keep working would be exactly the
    /// "every existing engine test starts rejecting submissions" failure
    /// mode a real restriction must avoid. `Some(set)` restricts
    /// `voice_exists` to that set, for tests that specifically exercise
    /// unknown-voice rejection; see `with_known_voices`.
    known_voices: Option<std::collections::HashSet<String>>,
}

impl StubSynthesizer {
    pub fn new() -> Self {
        Self::with_token_budget(509)
    }

    pub fn with_token_budget(token_budget: usize) -> Self {
        let d = crate::config::Config::default();
        StubSynthesizer {
            token_budget,
            loaded: false,
            synth_calls: 0,
            unload_calls: 0,
            reconfigured_to: Some((d.model, d.threads)),
            reconfigure_calls: 0,
            known_voices: None,
        }
    }

    /// A stub restricted to a specific voice list, for tests that exercise
    /// `Engine::submit`'s unknown-voice rejection. Every other stub
    /// constructor stays permissive (see `known_voices`'s doc comment).
    pub fn with_known_voices<I, S>(voices: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        StubSynthesizer {
            known_voices: Some(voices.into_iter().map(Into::into).collect()),
            ..Self::new()
        }
    }
}

impl Default for StubSynthesizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Synthesizer for StubSynthesizer {
    fn phonemize(&mut self, text: &str, _voice: &str) -> String {
        text.to_lowercase()
    }

    fn fits(&mut self, phonemes: &str) -> bool {
        phonemes.chars().count() <= self.token_budget
    }

    fn synth(&mut self, phonemes: &str, _voice: &str, speed: f32) -> Result<Vec<f32>, String> {
        self.loaded = true;
        self.synth_calls += 1;
        // ~80 ms of audio per phoneme character, divided by speed.
        let per_char = SAMPLE_RATE as f32 * 0.08;
        let n = ((phonemes.chars().count() as f32 * per_char) / speed.max(0.1)) as usize;
        Ok(vec![0.0; n])
    }

    fn unload(&mut self) {
        self.loaded = false;
        self.unload_calls += 1;
    }

    fn is_loaded(&self) -> bool {
        self.loaded
    }

    fn voice_exists(&self, voice: &str) -> bool {
        match &self.known_voices {
            Some(known) => known.contains(voice),
            None => true,
        }
    }

    fn reconfigure(&mut self, cfg: &crate::config::Config) -> bool {
        self.reconfigure_calls += 1;
        let changed = self.reconfigured_to.as_ref() != Some(&(cfg.model.clone(), cfg.threads));
        self.reconfigured_to = Some((cfg.model.clone(), cfg.threads));
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_produces_silence_proportional_to_length() {
        let mut s = StubSynthesizer::new();
        let short = s.synth("ab", "af_heart", 1.0).expect("synth");
        let long = s.synth("abcdefgh", "af_heart", 1.0).expect("synth");
        assert!(long.len() > short.len());
        assert!(short.iter().all(|x| *x == 0.0), "stub must emit silence");
    }

    #[test]
    fn stub_speed_shortens_output() {
        let mut s = StubSynthesizer::new();
        let normal = s.synth("abcdefgh", "af_heart", 1.0).expect("synth");
        let fast = s.synth("abcdefgh", "af_heart", 2.0).expect("synth");
        assert!(fast.len() < normal.len());
    }

    #[test]
    fn stub_counts_calls() {
        let mut s = StubSynthesizer::new();
        let _ = s.synth("a", "af_heart", 1.0);
        let _ = s.synth("b", "af_heart", 1.0);
        assert_eq!(s.synth_calls, 2);
    }

    #[test]
    fn stub_tracks_load_state() {
        let mut s = StubSynthesizer::new();
        assert!(!s.is_loaded(), "nothing is loaded before the first synth");
        let _ = s.synth("a", "af_heart", 1.0);
        assert!(s.is_loaded());
        s.unload();
        assert!(!s.is_loaded());
        assert_eq!(s.unload_calls, 1);
    }

    #[test]
    fn stub_fits_respects_the_configured_budget() {
        let mut s = StubSynthesizer::with_token_budget(4);
        assert!(s.fits("abcd"));
        assert!(!s.fits("abcde"));
    }

    #[test]
    fn stub_phonemize_is_identity_lowercased() {
        let mut s = StubSynthesizer::new();
        assert_eq!(s.phonemize("Hello There", "af_heart"), "hello there");
    }

    #[test]
    fn stub_voice_exists_is_permissive_by_default() {
        let s = StubSynthesizer::new();
        assert!(s.voice_exists("af_heart"));
        assert!(s.voice_exists("totally_bogus_name"));
    }

    #[test]
    fn stub_with_known_voices_restricts_voice_exists() {
        let s = StubSynthesizer::with_known_voices(["af_heart", "bf_emma"]);
        assert!(s.voice_exists("af_heart"));
        assert!(s.voice_exists("bf_emma"));
        assert!(!s.voice_exists("totally_bogus_name"));
    }
}
