//! Port of misaki `en.apply_stress`.

pub const PRIMARY_STRESS: char = 'ˈ';
pub const SECONDARY_STRESS: char = 'ˌ';

fn restress(ps: &str) -> String {
    // Move a stress mark to just before the syllable's vowel-ish nucleus.
    // misaki does this by re-sorting; a stable single pass is equivalent for
    // the shapes that occur in the lexicon.
    let chars: Vec<char> = ps.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == PRIMARY_STRESS || c == SECONDARY_STRESS {
            let mut j = i + 1;
            while j < chars.len() && !is_vowel(chars[j]) {
                j += 1;
            }
            if j < chars.len() {
                out.extend(&chars[i + 1..j]);
                out.push(c);
                out.push(chars[j]);
                i = j + 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out.into_iter().collect()
}

fn is_vowel(c: char) -> bool {
    "AIOQWYaiuæɑɒɔəɛɜɪʊʌᵻ".contains(c)
}

fn contains_vowel(ps: &str) -> bool {
    ps.chars().any(is_vowel)
}

pub fn apply_stress(ps: &str, stress: Option<f32>) -> String {
    let Some(stress) = stress else {
        return ps.to_string();
    };
    if stress < -1.0 {
        return ps.replace([PRIMARY_STRESS, SECONDARY_STRESS], "");
    }
    if stress == -1.0 || ((stress == 0.0 || stress == -0.5) && ps.contains(PRIMARY_STRESS)) {
        return ps.replace(SECONDARY_STRESS, "").replace(PRIMARY_STRESS, &SECONDARY_STRESS.to_string());
    }
    if (stress == 0.0 || stress == 0.5 || stress == 1.0)
        && !ps.contains(PRIMARY_STRESS)
        && !ps.contains(SECONDARY_STRESS)
    {
        if !contains_vowel(ps) {
            return ps.to_string();
        }
        return restress(&format!("{SECONDARY_STRESS}{ps}"));
    }
    if stress >= 1.0 && !ps.contains(PRIMARY_STRESS) && ps.contains(SECONDARY_STRESS) {
        return ps.replace(SECONDARY_STRESS, &PRIMARY_STRESS.to_string());
    }
    if stress > 1.0 && !ps.contains(PRIMARY_STRESS) && !ps.contains(SECONDARY_STRESS) {
        if !contains_vowel(ps) {
            return ps.to_string();
        }
        return restress(&format!("{PRIMARY_STRESS}{ps}"));
    }
    ps.to_string()
}

#[cfg(test)]
mod tests {
    use super::apply_stress;

    #[test]
    fn none_is_identity() {
        assert_eq!(apply_stress("həlˈO", None), "həlˈO");
    }

    #[test]
    fn below_minus_one_removes_all_stress() {
        assert_eq!(apply_stress("həlˈO", Some(-2.0)), "həlO");
        assert_eq!(apply_stress("ˌɪnˈtɛnt", Some(-2.0)), "ɪntɛnt");
    }

    #[test]
    fn minus_one_demotes_rather_than_removes() {
        // misaki: stress == -1 downgrades primary to secondary and drops
        // existing secondaries. It does NOT strip stress entirely.
        assert_eq!(apply_stress("həlˈO", Some(-1.0)), "həlˌO");
    }

    #[test]
    fn zero_downgrades_primary_to_secondary() {
        assert_eq!(apply_stress("həlˈO", Some(0.0)), "həlˌO");
    }

    #[test]
    fn high_upgrades_secondary_to_primary() {
        assert_eq!(apply_stress("ˌɪntɛnt", Some(1.0)), "ˈɪntɛnt");
    }

    #[test]
    fn no_vowels_returns_unchanged() {
        assert_eq!(apply_stress("ptk", Some(0.0)), "ptk");
        assert_eq!(apply_stress("ptk", Some(2.0)), "ptk");
    }
}
