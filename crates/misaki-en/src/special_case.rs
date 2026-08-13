//! Port of misaki's `Lexicon.get_special_case` (en.py:167-203), restricted to
//! the entries this crate can evaluate without a POS tagger, plus the
//! `future_vowel` machinery (`VOWELS`/`CONSONANTS`/`NON_QUOTE_PUNCTS` and
//! `G2P.token_context`, en.py:66-94 and en.py:645-650) that feeds it.
//!
//! misaki computes `future_vowel` -- "does the *next* token's resolved
//! phonemes start with a vowel, a consonant, or is there no next token at
//! all" -- by walking tokens right-to-left, so that by the time a word is
//! phonemized, whatever comes after it in the sentence is already known.
//! This crate phonemizes left-to-right, so `G2p::phonemize` does a first
//! pass over the token list in reverse (resolving each token's phonemes and
//! updating `future_vowel` via [`next_future_vowel`] as it goes) before
//! assembling the output in forward order.

/// en.py:94 `VOWELS`.
const VOWELS: &str = "AIOQWYaiuæɑɒɔəɛɜɪʊʌᵻ";
/// en.py:74 `CONSONANTS`.
const CONSONANTS: &str = "bdfhjklmnpstvwzðŋɡɹɾʃʒʤʧθ";
/// en.py:66-67: `PUNCTS` minus the quote characters (`"`, U+201C, U+201D).
const NON_QUOTE_PUNCTS: &str = ";:,.!?—…";

/// Port of `G2P.token_context` (en.py:645-650), restricted to `future_vowel`
/// -- `future_to` only feeds the `used`/VBD special case, which needs a POS
/// tagger and isn't ported (see [`get_special_case`]'s doc comment).
///
/// Scans `piece` (a token's already-resolved phonemes, *before* the final
/// `ɾ`->`T`/`ʔ`->`t` substitution -- see `G2p::phonemize`) left to right for
/// the first character that is a vowel, a consonant, or sentence-level
/// punctuation; stress marks and anything else are skipped over. Sentence
/// punctuation resets the state to "unknown" (`None`) -- misaki does this so
/// e.g. "a joke. For" doesn't treat "For" as if it were adjacent to "joke".
/// An empty `piece` (a dropped token, or one that hasn't resolved to
/// anything) leaves `prev` unchanged, mirroring misaki's `if ps else vowel`
/// guard: such a token is transparent to the context flowing through it.
pub fn next_future_vowel(piece: &str, prev: Option<bool>) -> Option<bool> {
    for c in piece.chars() {
        if NON_QUOTE_PUNCTS.contains(c) {
            return None;
        }
        if VOWELS.contains(c) {
            return Some(true);
        }
        if CONSONANTS.contains(c) {
            return Some(false);
        }
    }
    prev
}

/// Port of `Lexicon.get_special_case`, restricted to entries resolvable from
/// the word text and `future_vowel` alone -- no POS tagger. Returns the raw
/// phonemes for a special case (already stress-marked where applicable), or
/// `None` if `word` doesn't match one, in which case the caller falls
/// through to the normal lexicon/stemmer/fallback chain. Called with the
/// token's *original* casing, matching en.py's `get_word`, which checks
/// `get_special_case` before any lowercasing.
///
/// Cases skipped because they need a POS tagger (see en.py:167-203), and why
/// each is safe to skip or fall through:
///   - `ADD`-tagged `.`/`/` -- needs the `ADD` tag (URL/email tokens); this
///     crate's tokenizer never produces one anyway.
///   - `elif word in SYMBOLS` (en.py:170, `{'%': 'percent', '&': 'and', '+':
///     'plus', '@': 'at'}`) -- needs no tag, but is NOT ported here (a
///     separate scope decision). `&`/`+`/`@` are already handled upstream,
///     unconditionally, by `numbers::normalize` (see `numbers.rs`). A
///     standalone `%` (not part of a `numbers::normalize` percentage match)
///     is currently dropped rather than read as "percent" -- e.g. `the %
///     apple` -> reference `ðə pəɹsˈɛnt ˈæpᵊl`, this crate `ði ˈæpᵊl` (the
///     dropped token is also transparent to `future_vowel`, which flips
///     "the"'s reading too). Known gap, not fixed by this port.
///   - the dotted-abbreviation branch (`get_NNP` on e.g. "U.S.") -- doesn't
///     need a tag, but needs letter-by-letter spelling infrastructure this
///     port doesn't have. Not a function word; out of scope for this task.
///   - `am`/`Am`/`AM` and `AN` tagged NN* -- "am"/"AN" as a noun is
///     vanishingly rare; skipped with the same "dominant reading" reasoning
///     the task green-lit for `a`/DT below. `am`'s non-NN branches ARE
///     ported (see the match arm).
///   - `I` tagged `PRP` -- fits the same "dominant reading" argument (this
///     crate assumes it's a `PRP` pronoun, not e.g. a Roman numeral), but
///     it's NOT ported: it caused no measured mismatch in the token-level
///     corpus, and unlike `a`/`an`/`the`/`to`/`in`/`am` there's no golden-
///     corpus coverage to verify it against, so it's left out rather than
///     guessed at.
///   - `by`/`By`/`BY` tagged ADV -- needs the ADV/RB tag to pick the
///     adverbial "bˈI" reading (e.g. "drove by") over the dominant
///     preposition reading; skipped. (`by`'s *other* special case -- the
///     end-of-utterance stressed reading -- doesn't need a tag at all; it's
///     handled generically by `Entry::resolve`'s `'None'`-tag branch, not
///     here, since it comes from `lookup`, not `get_special_case`.)
///   - literal `vs`/`vs.` tagged `IN` -- needs a tag; not a top-frequency
///     function word, skipped.
///   - `used`/`Used`/`USED` tagged VBD/JJ with `ctx.future_to` -- needs a
///     tag. Falls through to the lexicon's DEFAULT variant (`jˈuzd`), which
///     is already correct far more often than not; no entry needed here.
///
/// `TO`/`THE`/`IN` (the all-caps spellings) ARE ported, unlike the cases
/// above: en.py gates each on a tag check (`tag in ('TO', 'IN')` for `TO`,
/// `tag == 'DT'` for `THE`, `tag != 'NNP'` for `IN`), but each of those
/// checks is itself a dominant-reading guard of exactly the kind the task
/// green-lit for `a`/`A` (`tag == 'DT'`) above: standalone all-caps
/// `TO`/`THE`/`IN` are that tag overwhelmingly more often than not, so
/// hardcoding the same reading their lowercase/Capitalized forms already
/// get is correct far more often than guessing otherwise. See the `to`/
/// `the`/`in` match arms below, which now include the all-caps spelling.
pub fn get_special_case(word: &str, future_vowel: Option<bool>) -> Option<&'static str> {
    match word {
        // en.py:174-175: `'ɐ' if tag == 'DT' else 'ˈA'`. The task green-lit
        // hard-coding the `DT` (determiner) reading as the unconditional
        // default: it dominates every other reading of standalone "a" (the
        // letter name, a grade, ...) so overwhelmingly in running text that
        // guessing DEFAULT-DT is correct far more often than guessing
        // DEFAULT-non-DT would be. Note this makes golds['a'] ('A', the
        // letter-name reading) unreachable, same as in the reference: this
        // branch matches on word text alone, before any lexicon lookup.
        "a" | "A" => Some("ɐ"),
        // en.py:182-185, minus the `AN`-tagged-NN* branch (see doc comment).
        "an" | "An" | "AN" => Some("ɐn"),
        // en.py:190-191: `word in ('to', 'To') or (word == 'TO' and tag in
        // ('TO', 'IN'))`, then `{None: golds['to'], False: 'tə', True:
        // 'tʊ'}`. `TO` is folded in under the dominant-reading assumption
        // (see the doc comment above): standalone "TO" is essentially
        // always tagged `TO` or `IN`, so it gets the same reading as `to`/
        // `To`. `golds['to']` is the plain lexicon entry ("tu"), used
        // verbatim only when there's no following token to reduce against.
        "to" | "To" | "TO" => Some(match future_vowel {
            None => "tu",
            Some(false) => "tə",
            Some(true) => "tʊ",
        }),
        // en.py:195-196: `word in ('the', 'The') or (word == 'THE' and tag
        // == 'DT')`, then `'ði' if ctx.future_vowel == True else 'ðə'`.
        // `THE` is folded in under the same dominant-reading assumption as
        // `a`/`A` (`DT`). Note `future_vowel is None` (last word) takes the
        // `ðə` branch too, not a third reading -- this is a strict `==
        // True`, not a truthiness check.
        //
        // `THE`'s dominant-reading risk is NOT identical to `a`/`A`'s: an
        // all-caps `THE` stranded with no grammatical continuation is tagged
        // `NNP` by the reference's tagger, which routes it through `get_NNP`
        // and spells it out letter by letter -- `g("That is not THE.")[0]`
        // -> "ðˈæt ɪz nˌɑt tˌiˌAʧˈi." ("T-H-E"), where this port says "ðə".
        // Measured: `TO`/`IN` do not share this (both match the reference in
        // the same position). Accepted gap, same class as particle "in"
        // below and the homograph limitation -- it needs a tagger.
        "the" | "The" | "THE" => Some(if future_vowel == Some(true) { "ði" } else { "ðə" }),
        // en.py:192-194 has two independent tag checks folded away here
        // under the dominant-reading assumption, both resting on "standalone
        // in/In/IN is (near-)always the `IN` (preposition) tag, not `NNP`
        // and not anything else":
        //   1. The branch-match gate, `word in ('in', 'In') or (word == 'IN'
        //      and tag != 'NNP')` -- `tag != 'NNP'` is always true under the
        //      assumption, so `IN` is folded into this arm alongside `in`/
        //      `In`, same as `THE`/`TO` above.
        //   2. `stress = PRIMARY_STRESS if ctx.future_vowel is None or tag
        //      != 'IN' else ''` -- `tag != 'IN'` is always false under the
        //      same assumption (this part of the reasoning already applied
        //      to `in`/`In` before this fix), so this collapses to:
        //      stressed when nothing follows, unstressed otherwise --
        //      regardless of whether what follows is a vowel or consonant.
        // Both assumptions are measurably wrong for phrasal-verb particle
        // "in" ("log in", "check in", "come in") when something follows it:
        // reference misaki tags that as RP/ADV, not `IN`, so it stays
        // stressed ("ˈɪn") even mid-sentence -- e.g. `g("log in now")[0]`
        // -> "lˈɔɡ ˈɪn nˈW", not the "ɪn" this port produces. No tagger
        // means no way to tell particle "in" from preposition "in"; this is
        // an accepted gap, same class as the homograph limitation, and it
        // applies equally to "IN" (e.g. `g("LOG IN")[0]` has the same shape).
        "in" | "In" | "IN" => Some(if future_vowel.is_none() { "ˈɪn" } else { "ɪn" }),
        // en.py:176-181, minus the NN*-tagged branch. Only the lowercase
        // spelling reduces (`Am`/`AM` fall through to golds['am'] both here
        // and in en.py, since `word != 'am'` there). Reduces only when a
        // following token is known to exist (`future_vowel.is_some()`) --
        // like `in`, this doesn't depend on whether that following phoneme
        // is actually a vowel, just on whether there is one. When nothing
        // follows, fall through to the ordinary lexicon entry (`golds['am']`
        // = "æm").
        "am" if future_vowel.is_some() => Some("ɐm"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{get_special_case, next_future_vowel};

    // Expected values verified against reference Python misaki
    // (misaki.en.G2P, trf=False, british=False), e.g.:
    //   from misaki import en, espeak
    //   g = en.G2P(trf=False, british=False,
    //              fallback=espeak.EspeakFallback(british=False))
    //   g("a test")[0]   # -> "ɐ tˈɛst"

    #[test]
    fn a_is_always_the_determiner_reading() {
        assert_eq!(get_special_case("a", None), Some("ɐ"));
        assert_eq!(get_special_case("A", Some(true)), Some("ɐ"));
    }

    #[test]
    fn an_is_always_reduced() {
        for w in ["an", "An", "AN"] {
            assert_eq!(get_special_case(w, None), Some("ɐn"));
            assert_eq!(get_special_case(w, Some(true)), Some("ɐn"));
        }
    }

    #[test]
    fn to_depends_on_the_next_phoneme() {
        // g("to eat")[0] -> "tʊ ˈit"
        assert_eq!(get_special_case("to", Some(true)), Some("tʊ"));
        // g("to go")[0] -> "tə ɡˈO"
        assert_eq!(get_special_case("To", Some(false)), Some("tə"));
        // g("nice to.")[0] (degenerate, but exercises the None arm) ->
        // "nˈIs tu."
        assert_eq!(get_special_case("to", None), Some("tu"));
    }

    #[test]
    fn the_is_ði_only_before_a_vowel() {
        // g("the apple")[0] -> "ði ˈæpəl"
        assert_eq!(get_special_case("the", Some(true)), Some("ði"));
        // g("the cat")[0] -> "ðə kˈæt"
        assert_eq!(get_special_case("The", Some(false)), Some("ðə"));
        // g("I like the.")[0] (degenerate, but exercises the None arm)
        assert_eq!(get_special_case("the", None), Some("ðə"));
    }

    #[test]
    fn in_is_stressed_only_at_the_end() {
        // g("check in")[0] -> "ʧˈɛk ˈɪn"
        assert_eq!(get_special_case("in", None), Some("ˈɪn"));
        // g("in time")[0] -> "ɪn tˈIm"
        assert_eq!(get_special_case("In", Some(false)), Some("ɪn"));
        assert_eq!(get_special_case("in", Some(true)), Some("ɪn"));
    }

    // Regression test for the bug the doc comment above `get_special_case`
    // describes: all-caps "THE"/"TO"/"IN" used to skip this function
    // entirely (falling through to `lex.raw` -> miss -> lowercase fallback
    // -> the wrong reduced/unstressed reading), the exact bug already fixed
    // for lowercase/Capitalized spellings. Values verified directly against
    // reference Python misaki (not just taken from a reviewer's table):
    //   g("THE cat")[0]    -> "ðə kˈæt"     (THE, consonant follows)
    //   g("THE apple")[0]  -> "ði ˈæpᵊl"    (THE, vowel follows)
    //   g("TO eat")[0]     -> "tʊ ˈit"      (TO, vowel follows)
    //   g("TO go")[0]      -> "tə ɡˌO"      (TO, consonant follows)
    //   g("he is IN")[0]   -> "hi ɪz ˈɪn"   (IN, nothing follows)
    //   g("CHECK IN")[0]   -> "ʧˈɛk ˈɪn"    (IN, nothing follows)
    //   g("IN time")[0]    -> "ɪn tˈIm"     (IN, consonant follows)
    #[test]
    fn all_caps_the_to_in_take_the_same_dominant_reading_as_lowercase() {
        assert_eq!(get_special_case("THE", Some(false)), Some("ðə"));
        assert_eq!(get_special_case("THE", Some(true)), Some("ði"));
        assert_eq!(get_special_case("TO", Some(true)), Some("tʊ"));
        assert_eq!(get_special_case("TO", Some(false)), Some("tə"));
        assert_eq!(get_special_case("IN", None), Some("ˈɪn"));
        assert_eq!(get_special_case("IN", Some(false)), Some("ɪn"));
    }

    #[test]
    fn am_reduces_only_when_something_follows() {
        // g("I am here")[0] -> "Y ɐm hˈɪɹ"
        assert_eq!(get_special_case("am", Some(false)), Some("ɐm"));
        assert_eq!(get_special_case("am", Some(true)), Some("ɐm"));
        // g("here I am")[0] -> "hˈɪɹ Y æm" (falls through to golds['am'])
        assert_eq!(get_special_case("am", None), None);
        // Capitalized/all-caps never reduce; they fall through too.
        assert_eq!(get_special_case("Am", Some(true)), None);
        assert_eq!(get_special_case("AM", Some(true)), None);
    }

    #[test]
    fn non_special_words_fall_through() {
        assert_eq!(get_special_case("cat", Some(true)), None);
        assert_eq!(get_special_case("this", None), None); // handled by Entry::resolve
    }

    #[test]
    fn next_future_vowel_scans_past_stress_marks() {
        assert_eq!(next_future_vowel("ˈæpəl", None), Some(true));
        assert_eq!(next_future_vowel("kˈæt", None), Some(false));
    }

    #[test]
    fn next_future_vowel_resets_on_sentence_punctuation() {
        assert_eq!(next_future_vowel(".", Some(true)), None);
        assert_eq!(next_future_vowel(",", Some(false)), None);
    }

    #[test]
    fn next_future_vowel_is_transparent_when_empty() {
        // A dropped/empty token doesn't overwrite the context flowing
        // through it from the right.
        assert_eq!(next_future_vowel("", Some(true)), Some(true));
        assert_eq!(next_future_vowel("", None), None);
    }
}
