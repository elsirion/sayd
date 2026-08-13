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
}

/// Test double. Emits silence at roughly the rate real speech occupies, so
/// duration-dependent engine behaviour is exercised without a model.
pub struct StubSynthesizer {
    token_budget: usize,
    loaded: bool,
    pub synth_calls: usize,
    pub unload_calls: usize,
}

impl StubSynthesizer {
    pub fn new() -> Self {
        Self::with_token_budget(509)
    }

    pub fn with_token_budget(token_budget: usize) -> Self {
        StubSynthesizer {
            token_budget,
            loaded: false,
            synth_calls: 0,
            unload_calls: 0,
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
}
