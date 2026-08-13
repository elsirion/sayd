//! English G2P for Kokoro TTS, ported from misaki (Apache-2.0).
//!
//! No POS tagger: POS-dependent lexicon entries resolve to their DEFAULT
//! variant. See `data/PROVENANCE.md`.

pub mod lexicon;
pub use lexicon::{Entry, Lexicon};

pub mod stress;

pub mod stem;

pub mod numbers;

pub mod special_case;

pub mod tokenize;

type Fallback = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Ties the lexicon, stemmer, number normalizer and tokenizer into one call.
///
/// No POS tagger and no capitalization-based stress: out-of-lexicon words
/// only get phonemized via the `-s`/`-ed`/`-ing` stemmer or the caller's
/// `fallback` closure (typically an espeak FFI binding, which this crate
/// deliberately does not depend on).
pub struct G2p {
    lex: Lexicon,
    fallback: Option<Fallback>,
}

impl G2p {
    /// See `Lexicon::new`: only US lexicons are vendored, so `british: true`
    /// does NOT give British pronunciations for lexicon/stemmer hits -- it
    /// only selects the stemmer's British-vs-American suffix phonemes. Any
    /// word the lexicon knows comes out American either way. Callers that
    /// need real British output must not rely on this flag; route British
    /// voices around this crate entirely (e.g. a whole-text espeak `en-gb`
    /// call), as `rust/src/main.rs` does.
    pub fn new(british: bool) -> Self {
        G2p { lex: Lexicon::new(british), fallback: None }
    }

    /// `f` is called for words the lexicon and stemmer both miss. The binary
    /// passes its espeak FFI here, keeping this crate free of C dependencies.
    /// See `new`'s doc comment: `british` does not affect lexicon/stemmer
    /// output beyond a few suffix phonemes -- only `f`, if the caller wires
    /// it to something `british`-aware, actually produces British phonemes.
    pub fn with_fallback(british: bool, f: Fallback) -> Self {
        G2p { lex: Lexicon::new(british), fallback: Some(f) }
    }

    /// `future_vowel` mirrors misaki's `TokenContext.future_vowel`: `None`
    /// means no later token in this call has (yet) established a value --
    /// i.e. `w` is the last resolvable token, or everything after it so far
    /// has been empty/dropped or reset by sentence punctuation; `Some(true)`
    /// means the next token's resolved phonemes start with a vowel,
    /// `Some(false)` a consonant. See `special_case.rs` for how it's
    /// threaded and consumed.
    fn word(&self, w: &str, future_vowel: Option<bool>) -> Option<String> {
        // en.py's get_word calls get_special_case first, before any lexicon
        // lookup or case-folding (en.py:331-332).
        if let Some(ps) = special_case::get_special_case(w, future_vowel) {
            return Some(ps.to_string());
        }
        if let Some(ps) = self.lex.raw(w).and_then(|e| e.resolve(future_vowel)) {
            return Some(ps.to_string());
        }
        let lower = w.to_lowercase();
        if lower != w {
            if let Some(ps) = self.lex.raw(&lower).and_then(|e| e.resolve(future_vowel)) {
                return Some(ps.to_string());
            }
        }
        // en.py's get_word calls stem_ing with `0.5 if stress is None else
        // stress`, but stem_s/stem_ed keep the caller's stress (always None
        // here, since this crate does no capitalization-based stress).
        // stem_lookup applies one stress value to all three suffixes, but
        // their guards ('s'/'d'/"ing" as the final character) are mutually
        // exclusive, so at most one of them can ever fire for a given word --
        // it's safe to pick the stress for the whole call from the word's
        // suffix alone.
        let stem_stress = if lower.ends_with("ing") { Some(0.5) } else { None };
        if let Some((ps, _)) = stem::stem_lookup(&self.lex, w, stem_stress, future_vowel) {
            return Some(ps);
        }
        self.fallback.as_ref().and_then(|f| f(w))
    }

    pub fn phonemize(&self, text: &str) -> String {
        let normalized = numbers::normalize(text);
        let toks = tokenize::tokenize(&normalized);

        // en.py resolves tokens right-to-left (en.py:686) so that by the
        // time a word is phonemized, `ctx.future_vowel` already reflects
        // whatever comes after it -- needed by the `a`/`to`/`the`/`in`/`am`
        // special cases and the lexicon's `'None'`-tag entries (`this`,
        // `by`, ...; see special_case.rs and Entry::resolve). This crate's
        // main loop is left-to-right for assembly, so pieces are resolved in
        // a first reverse pass into a parallel array, then assembled forward
        // in a second pass using the existing spacing logic unchanged.
        let mut pieces: Vec<String> = vec![String::new(); toks.len()];
        let mut future_vowel: Option<bool> = None;
        for i in (0..toks.len()).rev() {
            let tok = &toks[i];
            let piece = if tok.is_punct {
                // only punctuation Kokoro has tokens for survives
                if tokenize::PUNCTS.contains(&tok.text) { tok.text.clone() } else { String::new() }
            } else {
                self.word(&tok.text, future_vowel).unwrap_or_default()
            };
            // Classifies on the raw piece, before the ɾ/ʔ substitution below
            // -- misaki does the same (that substitution is a final pass
            // over already-resolved tokens, en.py:733-736), and 'ɾ' (unlike
            // its 'T' replacement) is itself in CONSONANTS.
            future_vowel = special_case::next_future_vowel(&piece, future_vowel);
            pieces[i] = piece;
        }

        let mut out = String::new();
        // A token that contributes no output (punctuation Kokoro has no
        // token for, or a word neither the lexicon/stemmer nor the fallback
        // can resolve) must not glue its neighbors together -- it still
        // marked a boundary in the source text even without whitespace
        // around it (e.g. "and/or", "cat/dog"). `pending_space` defers
        // emitting a separator until the next token that actually produces
        // output, so any run of space/dropped-token boundaries collapses to
        // exactly one space, and a boundary at the very start or end of the
        // output never leaves an orphaned space.
        let mut pending_space = false;
        for (tok, piece) in toks.iter().zip(pieces.iter()) {
            if !piece.is_empty() {
                if pending_space && !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(piece);
                pending_space = false;
            }
            if tok.space_after || piece.is_empty() {
                pending_space = true;
            }
        }
        // misaki's G2P.__call__ applies this substitution as its final step,
        // after all tokens' phonemes are joined (en.py:736).
        out.replace('ɾ', "T").replace('ʔ', "t")
    }
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    #[test]
    fn crate_builds() {
        assert_eq!(super::version(), "0.1.0");
    }
}

#[cfg(test)]
mod g2p_tests {
    use super::G2p;

    #[test]
    fn known_words_come_from_the_lexicon() {
        let g = G2p::new(false);
        assert_eq!(g.phonemize("hello world"), "həlˈO wˈɜɹld");
    }

    #[test]
    fn punctuation_is_preserved() {
        let g = G2p::new(false);
        let out = g.phonemize("Hello, world.");
        assert!(out.contains(','), "got {out}");
        assert!(out.ends_with('.'), "got {out}");
    }

    #[test]
    fn numbers_are_normalized_before_lookup() {
        let g = G2p::new(false);
        let out = g.phonemize("$5");
        assert!(out.contains("fˈIv"), "got {out}");
        assert!(out.contains("dˈɑləɹz"), "got {out}");
    }

    #[test]
    fn unknown_words_hit_the_fallback() {
        let g = G2p::with_fallback(false, Box::new(|_w| Some("XX".to_string())));
        assert_eq!(g.phonemize("zzzqqqnotaword"), "XX");
    }

    #[test]
    fn tau_and_glottal_stop_are_substituted_as_the_final_step() {
        // Amendment 1: G2P.__call__ ends with
        // .replace('ɾ', 'T').replace('ʔ', 't'). The gold lexicon stores
        // "meeting" as "mˈiɾɪŋ"; reference misaki (verified against Python:
        // g("meeting")[0]) emits "mˈiTɪŋ".
        let g = G2p::new(false);
        let raw = g.lex.raw("meeting").and_then(|e| e.resolve(None)).unwrap().to_string();
        assert!(raw.contains('ɾ'), "expected lexicon entry to contain ɾ, got {raw}");
        assert_eq!(g.phonemize("meeting"), "mˈiTɪŋ");
    }

    #[test]
    fn unknown_non_stemmable_no_orphaned_space() {
        // Regression test: unknown, non-stemmable word without fallback
        // should not emit an orphaned trailing space. The word produces no
        // output, but the adjacent known words should be correctly spaced.
        let g = G2p::new(false);
        let out = g.phonemize("hello zzzqqq world");
        // No double space should appear
        assert!(!out.contains("  "), "expected no double space, got: {out}");
        // Both words should produce output; split and verify we get exactly 2
        // phoneme groups with single space between them
        let parts: Vec<&str> = out.split_whitespace().collect();
        assert_eq!(parts.len(), 2, "expected 2 phoneme groups from known words, got: {out}");
    }

    #[test]
    fn british_flag_does_not_change_lexicon_output() {
        // Honest-behaviour pin, not a desired behaviour: only US lexicons
        // are vendored (see Lexicon::new / build.rs), so `british: true`
        // currently affects nothing but stem.rs's suffix phoneme choice for
        // words reached through the stemmer -- every in-lexicon word is
        // American regardless of this flag. Reference misaki with
        // british=True gives "ðə wˈɔːtə bˈɒtᵊl" for this sentence; this
        // crate cannot produce that. If this test ever starts failing
        // because the two diverge, that's GB lexicon support landing, and
        // this test (and its doc comments) should be deleted, not "fixed".
        let us = G2p::new(false).phonemize("the water bottle");
        let gb = G2p::new(true).phonemize("the water bottle");
        assert_eq!(us, gb, "expected byte-identical output: US and GB \
            lexicon lookups are the same lexicon, got us={us:?} gb={gb:?}");
        // "the" -> "ðə" (not "ði"): "water" starts with a consonant. Before
        // the special-case port this was hardcoded to the wrong "ði" (the
        // reduced-vs-vowel distinction didn't exist yet); reference misaki
        // gives "ðə wˈɔTəɹ bˈɑTəl" for this sentence too, confirmed against
        // Python (see special-case-report.md).
        assert_eq!(us, "ðə wˈɔTəɹ bˈɑTəl");
    }

    // Final-review finding: a dropped token (punctuation Kokoro has no token
    // for, e.g. '/') that sits between two words with no surrounding
    // whitespace used to contribute neither text nor a separator, gluing the
    // words on either side into one unlooked-up token. Reference misaki
    // keeps them apart. This must hold at the same time as the
    // no-orphaned-space regressions above (dropping a token still must not
    // *introduce* a space where whitespace already produced exactly one).

    #[test]
    fn dropped_slash_does_not_glue_adjacent_words() {
        let g = G2p::new(false);
        assert_eq!(g.phonemize("and/or"), "ænd ɔɹ");
        assert_eq!(g.phonemize("cat/dog"), "kˈæt dˈɔɡ");
    }

    #[test]
    fn dropped_slash_in_a_fraction_does_not_glue_words() {
        let g = G2p::new(false);
        let out = g.phonemize("The 1/3 share");
        assert!(out.contains("wˈʌn θɹˈi"), "expected a space between \"one\" \
            and \"third\", got {out:?}");
    }

    #[test]
    fn ampersand_does_not_glue_adjacent_words() {
        // Same root cause, but in numbers::normalize's unpadded '&'/'+'/'@'
        // substitution: "yes&no" became the single token "yesandno", which
        // is absent from the lexicon and produced empty output.
        let g = G2p::new(false);
        assert_eq!(g.phonemize("yes&no"), "jˈɛs ænd nˈO");
    }

    #[test]
    fn unknown_punctuation_no_orphaned_space() {
        // Regression test: punctuation not in PUNCTS (e.g. %) should not emit
        // an orphaned trailing space when it is dropped. The adjacent known
        // words should be correctly spaced.
        // Note: % is not matched by normalize's percent regex if not preceded by
        // a digit, and % is not in PUNCTS, so a standalone % is dropped.
        let g = G2p::new(false);
        let out = g.phonemize("hello % world");
        // No double space should appear
        assert!(!out.contains("  "), "expected no double space, got: {out}");
        // Both words should produce output; split and verify we get exactly 2
        // phoneme groups with single space between them
        let parts: Vec<&str> = out.split_whitespace().collect();
        assert_eq!(parts.len(), 2, "expected 2 phoneme groups from known words, got: {out}");
    }

    // -- Special-case function words (special_case.rs / Entry::resolve's
    // 'None'-tag branch), exercised end-to-end through `phonemize`. Expected
    // values verified against reference Python misaki:
    //   nix-shell shell.nix --run 'python3 -c "
    //   from misaki import en, espeak
    //   g=en.G2P(trf=False, british=False,
    //            fallback=espeak.EspeakFallback(british=False))
    //   for t in [...]: print(repr(t), g(t)[0])
    //   "'
    // (see special-case-report.md for the exact transcript).

    #[test]
    fn a_is_the_determiner_reading_before_either_vowel_or_consonant() {
        let g = G2p::new(false);
        assert_eq!(g.phonemize("a test"), "ɐ tˈɛst");
        assert_eq!(g.phonemize("a apple"), "ɐ ˈæpᵊl");
    }

    #[test]
    fn the_reduces_to_ðə_before_a_consonant_and_ði_before_a_vowel() {
        let g = G2p::new(false);
        assert_eq!(g.phonemize("the cat"), "ðə kˈæt");
        assert_eq!(g.phonemize("the apple"), "ði ˈæpᵊl");
    }

    #[test]
    fn the_classifies_on_the_next_tokens_phoneme_not_its_spelling() {
        // The vowel/consonant classification `future_vowel` reduction relies
        // on is done on the *phoneme* the next token resolves to, not on
        // that token's spelling -- these three cases each pick a next word
        // where spelling and first phoneme disagree (or, for "a", is in
        // neither VOWELS nor CONSONANTS at all), so they'd fail if a future
        // change classified by spelling instead. Verified directly against
        // reference Python misaki (not just taken from a reviewer's table):
        //   g("the hour")[0]        -> "ði ˈWəɹ"
        //     ("hour" is spelled with a leading consonant letter, but its
        //     first phoneme "W" is a vowel -- VOWELS includes 'W' as the
        //     "aʊ" diphthong.)
        //   g("the university")[0]  -> "ðə jˌunəvˈɜɹsəTi"
        //     ("university" is spelled with a leading vowel letter, but its
        //     first phoneme "j" (the "y"-glide) is a consonant.)
        //   g("the a apple")[0]     -> "ði ɐ ˈæpᵊl"
        //     ("a" resolves to "ɐ" via this very special-case machinery;
        //     'ɐ' is in neither VOWELS nor CONSONANTS, so `next_future_vowel`
        //     is transparent to it and "the" classifies on "apple" beyond
        //     it, not on "a" -- an end-to-end exercise of the transparency
        //     that `next_future_vowel_is_transparent_when_empty` only
        //     covers with an empty piece. Unlike the two above, this case
        //     does NOT discriminate phoneme- from spelling-based
        //     classification -- "a" begins with a vowel letter too, so a
        //     spelling-based classifier would land on "ði" by coincidence.
        //     It is here for the transparency skip, not for that property.)
        let g = G2p::new(false);
        assert_eq!(g.phonemize("the hour"), "ði ˈWəɹ");
        assert_eq!(g.phonemize("the university"), "ðə jˌunəvˈɜɹsəTi");
        assert_eq!(g.phonemize("the a apple"), "ði ɐ ˈæpᵊl");
    }

    #[test]
    fn to_reduces_by_the_following_phoneme() {
        let g = G2p::new(false);
        assert_eq!(g.phonemize("to go"), "tə ɡˌO");
        assert_eq!(g.phonemize("to eat"), "tʊ ˈit");
    }

    #[test]
    fn sentence_punctuation_resets_future_vowel() {
        // "the"/"to" immediately followed by a comma must NOT peek past it
        // to the next word's vowel-ness -- misaki's future_vowel resets to
        // "unknown" at sentence-level punctuation, so both take their
        // None-branch reading ("ðə", "tu") even though "apple" (vowel)
        // follows the comma.
        let g = G2p::new(false);
        assert_eq!(g.phonemize("the, apple"), "ðə, ˈæpᵊl");
        assert_eq!(g.phonemize("to, apple"), "tu, ˈæpᵊl");
    }

    #[test]
    fn in_is_stressed_only_with_nothing_following() {
        let g = G2p::new(false);
        assert_eq!(g.phonemize("check in"), "ʧˈɛk ˈɪn");
        assert_eq!(g.phonemize("in time"), "ɪn tˈIm");
    }

    #[test]
    fn am_reduces_only_when_something_follows() {
        // "I" avoided deliberately: reference misaki has its own special
        // case downgrading standalone "I" to secondary stress (en.py:186-187,
        // needs a `PRP` tag), which this port doesn't implement (see
        // special_case.rs's doc comment) -- mixing it in here would make
        // this test assert on that unrelated, already-documented gap instead
        // of isolating "am".
        let g = G2p::new(false);
        assert_eq!(g.phonemize("am here"), "ɐm hˈɪɹ");
        assert_eq!(g.phonemize("here am"), "hˈɪɹ æm");
    }

    #[test]
    fn an_is_always_reduced() {
        let g = G2p::new(false);
        assert_eq!(g.phonemize("an apple"), "ɐn ˈæpᵊl");
    }

    #[test]
    fn none_tag_lexicon_entries_are_stressed_only_at_the_end() {
        // "this"/"by" aren't in special_case.rs at all -- they're ordinary
        // lexicon entries with a 'None'-tagged variant, picked up generically
        // by Entry::resolve. Exercises that path end-to-end, including
        // through the stemmer's suffix machinery is NOT needed here (neither
        // word takes a suffix); stem.rs's own future_vowel threading is
        // covered separately by its "gets" tests.
        let g = G2p::new(false);
        assert_eq!(g.phonemize("this"), "ðˈɪs");
        assert_eq!(g.phonemize("this is"), "ðɪs ɪz");
        assert_eq!(g.phonemize("by"), "bˈI");
        assert_eq!(g.phonemize("by the way"), "bI ðə wˈA");
    }
}
