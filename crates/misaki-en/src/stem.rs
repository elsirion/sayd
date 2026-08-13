//! Port of misaki's -s / -ed / -ing morphology.
//!
//! Mirrors en.py's `stem_s`/`_s`, `stem_ed`/`_ed`, `stem_ing`/`_ing`. Notably,
//! en.py's `stem_*` methods call `self.lookup(stem, ...)` -- which resolves
//! the BARE stem's phonemes and applies stress to *that* -- and only then
//! append the suffix's phonemes via `_s`/`_ed`/`_ing`. Stress is therefore
//! applied before the suffix is glued on, not after.

use crate::stress::apply_stress;
use crate::{Entry, Lexicon};

/// en.py:76 `US_TAUS` -- vowel-ish codas that trigger US t-flapping.
const US_TAUS: &str = "AIOWYiuæɑəɛɪɹʊʌ";

/// en.py `Lexicon.is_known`, collapsed to what this crate can express without
/// a POS tagger or golds/silvers-vs-proper-noun casing rules: "the lexicon
/// resolves this word". Presence-only, so which `future_vowel` variant would
/// be picked doesn't matter here -- `None` is passed arbitrarily.
fn is_known(lex: &Lexicon, w: &str) -> bool {
    lex.raw(w).and_then(|e: Entry| e.resolve(None)).is_some()
}

/// en.py `Lexicon.lookup`, restricted to the part `stem_*` actually uses:
/// resolve the stem's phonemes and apply stress to them (before any suffix
/// is appended). `future_vowel` must be threaded through from the caller
/// (not hardcoded): the stem itself can be a word with a `'None'`-tagged
/// entry (e.g. "get", stemmed from "gets") whose DEFAULT-vs-`'None'` choice
/// depends on it same as any other word -- see `Entry::resolve`.
fn lookup(
    lex: &Lexicon,
    stem: &str,
    stress: Option<f32>,
    future_vowel: Option<bool>,
) -> Option<String> {
    lex.raw(stem).and_then(|e: Entry| e.resolve(future_vowel)).map(|ps| apply_stress(ps, stress))
}

/// en.py `_s`.
fn suffix_s(stem: &str, british: bool) -> Option<String> {
    let c = stem.chars().last()?;
    Some(if "ptkfθ".contains(c) {
        format!("{stem}s")
    } else if "szʃʒʧʤ".contains(c) {
        format!("{stem}{}z", if british { 'ɪ' } else { 'ᵻ' })
    } else {
        format!("{stem}z")
    })
}

/// en.py `stem_s`. Candidate order matters: word[:-1] is tried before
/// word[:-2], and the `-ies -> y` branch (missing from the original port)
/// comes last.
fn stem_s(
    lex: &Lexicon,
    word: &str,
    stress: Option<f32>,
    future_vowel: Option<bool>,
) -> Option<(String, u8)> {
    if word.len() < 3 || !word.ends_with('s') {
        return None;
    }
    let stem = if !word.ends_with("ss") && is_known(lex, &word[..word.len() - 1]) {
        word[..word.len() - 1].to_string()
    } else if (word.ends_with("'s")
        || (word.len() > 4 && word.ends_with("es") && !word.ends_with("ies")))
        && is_known(lex, &word[..word.len() - 2])
    {
        word[..word.len() - 2].to_string()
    } else if word.len() > 4 && word.ends_with("ies") {
        let candidate = format!("{}y", &word[..word.len() - 3]);
        if is_known(lex, &candidate) {
            candidate
        } else {
            return None;
        }
    } else {
        return None;
    };
    let ps = lookup(lex, &stem, stress, future_vowel)?;
    suffix_s(&ps, lex.british).map(|p| (p, 3))
}

/// en.py `_ed`. Note the voiceless-stop set here does NOT include `t` --
/// `t`-final stems fall through to the US t-flapping / british branches.
fn suffix_ed(stem: &str, british: bool) -> Option<String> {
    let c = stem.chars().last()?;
    if "pkfθʃsʧ".contains(c) {
        return Some(format!("{stem}t"));
    }
    if c == 'd' {
        return Some(format!("{stem}{}d", if british { 'ɪ' } else { 'ᵻ' }));
    }
    if c != 't' {
        return Some(format!("{stem}d"));
    }
    let chars: Vec<char> = stem.chars().collect();
    if british || chars.len() < 2 {
        return Some(format!("{stem}ɪd"));
    }
    let prev = chars[chars.len() - 2];
    if US_TAUS.contains(prev) {
        let stripped: String = chars[..chars.len() - 1].iter().collect();
        Some(format!("{stripped}ɾᵻd"))
    } else {
        Some(format!("{stem}ᵻd"))
    }
}

/// en.py `stem_ed`.
fn stem_ed(
    lex: &Lexicon,
    word: &str,
    stress: Option<f32>,
    future_vowel: Option<bool>,
) -> Option<(String, u8)> {
    if word.len() < 4 || !word.ends_with('d') {
        return None;
    }
    let stem = if !word.ends_with("dd") && is_known(lex, &word[..word.len() - 1]) {
        word[..word.len() - 1].to_string()
    } else if word.len() > 4
        && word.ends_with("ed")
        && !word.ends_with("eed")
        && is_known(lex, &word[..word.len() - 2])
    {
        word[..word.len() - 2].to_string()
    } else {
        return None;
    };
    let ps = lookup(lex, &stem, stress, future_vowel)?;
    suffix_ed(&ps, lex.british).map(|p| (p, 3))
}

/// en.py `_ing`.
fn suffix_ing(stem: &str, british: bool) -> Option<String> {
    let c = stem.chars().last()?;
    if british {
        if c == 'ə' || c == 'ː' {
            return None;
        }
    } else {
        let chars: Vec<char> = stem.chars().collect();
        if chars.len() > 1 && c == 't' && US_TAUS.contains(chars[chars.len() - 2]) {
            let stripped: String = chars[..chars.len() - 1].iter().collect();
            return Some(format!("{stripped}ɾɪŋ"));
        }
    }
    Some(format!("{stem}ɪŋ"))
}

/// en.py's `stem_ing` third-branch regex: `([bcdgklmnprstvxz])\1ing$|cking$`
/// -- a doubled consonant from that set directly before "ing", or a bare
/// "cking" ending (covers e.g. "picking" without doubling).
fn doubled_consonant_before_ing(word: &str) -> bool {
    if word.ends_with("cking") {
        return true;
    }
    let chars: Vec<char> = word.chars().collect();
    let n = chars.len();
    if n < 5 {
        return false;
    }
    let a = chars[n - 5];
    let b = chars[n - 4];
    a == b && "bcdgklmnprstvxz".contains(a)
}

/// en.py `stem_ing`.
fn stem_ing(
    lex: &Lexicon,
    word: &str,
    stress: Option<f32>,
    future_vowel: Option<bool>,
) -> Option<(String, u8)> {
    if word.len() < 5 || !word.ends_with("ing") {
        return None;
    }
    let stem = if word.len() > 5 && is_known(lex, &word[..word.len() - 3]) {
        word[..word.len() - 3].to_string()
    } else if is_known(lex, &format!("{}e", &word[..word.len() - 3])) {
        format!("{}e", &word[..word.len() - 3])
    } else if word.len() > 5 && doubled_consonant_before_ing(word) {
        let candidate = word[..word.len() - 4].to_string();
        if is_known(lex, &candidate) {
            candidate
        } else {
            return None;
        }
    } else {
        return None;
    };
    let ps = lookup(lex, &stem, stress, future_vowel)?;
    suffix_ing(&ps, lex.british).map(|p| (p, 3))
}

/// Try -s, -ed, -ing stemming in that order, mirroring en.py `get_word`'s
/// `stem_s` / `stem_ed` / `stem_ing` fallback chain. `future_vowel` is
/// threaded through to the stem's own lexicon lookup (see `lookup` above);
/// it's the same value `special_case::next_future_vowel` computed for this
/// token's position, not re-derived from the suffixed word.
pub fn stem_lookup(
    lex: &Lexicon,
    word: &str,
    stress: Option<f32>,
    future_vowel: Option<bool>,
) -> Option<(String, u8)> {
    let lower = word.to_lowercase();
    stem_s(lex, &lower, stress, future_vowel)
        .or_else(|| stem_ed(lex, &lower, stress, future_vowel))
        .or_else(|| stem_ing(lex, &lower, stress, future_vowel))
}

#[cfg(test)]
mod tests {
    use super::stem_lookup;
    use crate::Lexicon;

    #[test]
    fn plural_s_suffixes() {
        let lex = Lexicon::new(false);
        // voiceless stem -> /s/, voiced -> /z/, sibilant -> /ᵻz/
        assert_eq!(stem_lookup(&lex, "cats", None, None).unwrap().0, "kˈæts");
        assert_eq!(stem_lookup(&lex, "dogs", None, None).unwrap().0, "dˈɔɡz");
        assert_eq!(stem_lookup(&lex, "buses", None, None).unwrap().0, "bˈʌsᵻz");
    }

    #[test]
    fn past_ed_suffixes() {
        let lex = Lexicon::new(false);
        assert_eq!(stem_lookup(&lex, "walked", None, None).unwrap().0, "wˈɔkt");
        assert_eq!(stem_lookup(&lex, "played", None, None).unwrap().0, "plˈAd");
        assert_eq!(stem_lookup(&lex, "wanted", None, None).unwrap().0, "wˈɑntᵻd");
    }

    #[test]
    fn unknown_stems_return_none() {
        let lex = Lexicon::new(false);
        assert!(stem_lookup(&lex, "zzzqqqs", None, None).is_none());
    }

    // -- Findings-fix regression tests, expectations from controller-verified
    // Python misaki (with the G2P-final ɾ->T substitution translated back to
    // ɾ, since stem_lookup must not apply that substitution itself). --

    #[test]
    fn ies_branch_stems_via_y() {
        // Finding 2: the "-ies -> y" branch was missing entirely.
        let lex = Lexicon::new(false);
        assert_eq!(stem_lookup(&lex, "cities", None, None).unwrap().0, "sˈɪɾiz");
    }

    #[test]
    fn word_minus_one_candidate_wins_over_word_minus_two() {
        // Finding 1: word[:-1] must be tried before word[:-2] for both -s and
        // -ed. "regaled"/"regales" are only known via the word[:-1] == "regale"
        // candidate; the word[:-2] == "regal" candidate exists too but must be
        // shadowed, not preferred.
        let lex = Lexicon::new(false);
        assert_eq!(stem_lookup(&lex, "regaled", None, None).unwrap().0, "ɹəɡˈAld");
        assert_eq!(stem_lookup(&lex, "regales", None, None).unwrap().0, "ɹəɡˈAlz");
    }

    #[test]
    fn us_ing_flap_rule() {
        // Finding 3: US t-flapping in `_ing` (stem ends in t, preceded by a
        // US_TAUS vowel) was missing.
        let lex = Lexicon::new(false);
        assert_eq!(stem_lookup(&lex, "yeeting", None, None).unwrap().0, "jˈiɾɪŋ");
    }

    // future_vowel threading: "gets" stems to "get", which has a
    // 'None'-tagged gold entry (DEFAULT="ɡɛt", None="ɡˈɛt" -- see
    // Entry::resolve). The stem's own `future_vowel` context, not a
    // hardcoded value, must decide which one `_s` glues onto. Verified
    // against reference Python misaki:
    //   g("gets")[0]        -> "ɡˈɛts"  (last/only word: future_vowel=None)
    //   g("He gets it")[0]  -> "hˌi ɡɛts ɪt" (mid-sentence: future_vowel=Some(_))

    #[test]
    fn stemmed_none_tag_word_takes_none_variant_when_isolated() {
        let lex = Lexicon::new(false);
        assert_eq!(stem_lookup(&lex, "gets", None, None).unwrap().0, "ɡˈɛts");
    }

    #[test]
    fn stemmed_none_tag_word_takes_default_when_future_vowel_is_known() {
        let lex = Lexicon::new(false);
        assert_eq!(stem_lookup(&lex, "gets", None, Some(true)).unwrap().0, "ɡɛts");
        assert_eq!(stem_lookup(&lex, "gets", None, Some(false)).unwrap().0, "ɡɛts");
    }
}
