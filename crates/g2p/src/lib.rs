//! English G2P for Kokoro, one type over two tiers.
//!
//! American text goes through misaki-en (vendored misaki lexicons, number and
//! currency normalization, `-s`/`-ed`/`-ing` stemming) with espeak-ng as the
//! per-word fallback for anything the lexicon and stemmer both miss.
//!
//! British text bypasses misaki-en entirely and takes a whole-text espeak
//! `en-gb` call. misaki-en only vendors US lexicons, so its `british` flag
//! does not produce British pronunciations. This must stay a whole-text call
//! rather than a per-word fallback: espeak phonemizes at clause level and
//! per-word calls produce audibly different output.

mod espeak;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Dialect {
    American,
    British,
}

pub struct Phonemizer {
    us: misaki_en::G2p,
}

impl Phonemizer {
    pub fn new() -> Self {
        Phonemizer {
            us: misaki_en::G2p::with_fallback(
                false,
                Box::new(|w| espeak::phonemize_word(w, false)),
            ),
        }
    }

    pub fn phonemize(&self, text: &str, dialect: Dialect) -> String {
        match dialect {
            Dialect::American => self.us.phonemize(text),
            Dialect::British => espeak::phonemize_en(text, true),
        }
    }
}

impl Default for Phonemizer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn american_lexicon_word_phonemizes() {
        let p = Phonemizer::new();
        let out = p.phonemize("hello", Dialect::American);
        assert!(!out.is_empty(), "expected phonemes for a lexicon word");
    }

    #[test]
    fn out_of_lexicon_word_falls_back_to_espeak() {
        let p = Phonemizer::new();
        // A nonsense word the lexicon and stemmer both miss.
        let out = p.phonemize("zorbleflux", Dialect::American);
        assert!(!out.is_empty(), "expected espeak fallback output, got empty");
    }

    #[test]
    fn british_takes_the_whole_text_espeak_path() {
        let p = Phonemizer::new();
        let out = p.phonemize("schedule", Dialect::British);
        assert!(!out.is_empty());
    }

    #[test]
    fn empty_input_gives_empty_output() {
        let p = Phonemizer::new();
        assert!(p.phonemize("", Dialect::American).trim().is_empty());
    }

    #[test]
    fn phonemizer_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Phonemizer>();
    }

    /// Regression guard: a predecessor project shipped all eight British
    /// voices speaking rhotic American because the British path fell through
    /// to misaki-en (which only vendors US lexicons). "tomato" has a
    /// well-known transatlantic pronunciation split, so American and British
    /// output for the same word must differ.
    #[test]
    fn american_and_british_dialects_produce_different_output() {
        let p = Phonemizer::new();
        let us = p.phonemize("tomato", Dialect::American);
        let gb = p.phonemize("tomato", Dialect::British);
        assert_ne!(us, gb, "British output must not collapse into American");
    }

    #[test]
    fn embedded_nul_does_not_panic() {
        let p = Phonemizer::new();
        let out = p.phonemize("hello\u{0000}world", Dialect::American);
        assert!(!out.is_empty(), "expected phonemes, got empty");
    }

    #[test]
    fn a_string_of_only_nuls_yields_empty_output_without_panicking() {
        let p = Phonemizer::new();
        assert!(p.phonemize("\u{0000}\u{0000}", Dialect::American).trim().is_empty());
    }

    #[test]
    fn embedded_nul_does_not_panic_on_the_british_path() {
        let p = Phonemizer::new();
        let out = p.phonemize("sched\u{0000}ule", Dialect::British);
        assert!(!out.is_empty());
    }
}
