//! Text normalization: numbers, currency, ordinals, symbols -> English words.
//!
//! misaki does this per-token with currency context; we do it as a text pass
//! before tokenization. Equivalent output, simpler control flow. Validated
//! against the golden corpus, not against misaki's internal structure.

use regex::Regex;
use std::sync::LazyLock;

const ONES: [&str; 20] = [
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight",
    "nine", "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen",
    "sixteen", "seventeen", "eighteen", "nineteen",
];
const TENS: [&str; 10] = [
    "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy",
    "eighty", "ninety",
];
const ORDINAL_SMALL: [&str; 20] = [
    "zeroth", "first", "second", "third", "fourth", "fifth", "sixth",
    "seventh", "eighth", "ninth", "tenth", "eleventh", "twelfth",
    "thirteenth", "fourteenth", "fifteenth", "sixteenth", "seventeenth",
    "eighteenth", "nineteenth",
];

/// Scale name for each base-1000 group, indexed from the least significant
/// group (index 0, no suffix) upward. i64::MIN/MAX need up to index 6
/// ("quintillion"); the extra tiers give headroom for digit runs in input
/// text (e.g. tracking numbers) that exceed i64.
const SCALE_NAMES: [&str; 12] = [
    "", "thousand", "million", "billion", "trillion", "quadrillion",
    "quintillion", "sextillion", "septillion", "octillion", "nonillion",
    "decillion",
];

fn under_thousand(n: u32) -> String {
    let mut parts = Vec::new();
    let h = n / 100;
    let r = n % 100;
    if h > 0 {
        parts.push(format!("{} hundred", ONES[h as usize]));
    }
    if r > 0 {
        if r < 20 {
            parts.push(ONES[r as usize].to_string());
        } else {
            let t = TENS[(r / 10) as usize];
            let o = r % 10;
            parts.push(if o == 0 { t.to_string() } else { format!("{t} {}", ONES[o as usize]) });
        }
    }
    parts.join(" ")
}

/// Converts a run of decimal digits (already comma-stripped, no sign, may
/// have leading zeros) into words by grouping into base-1000 chunks from the
/// right. This works directly on the digit string rather than a fixed-width
/// integer, so it never overflows. Returns `None` only when the number of
/// groups exceeds `SCALE_NAMES` (a magnitude too vast to occur in realistic
/// text), so callers can fall back instead of panicking or losing data.
fn digits_to_words(digits: &str) -> Option<String> {
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        return Some("zero".to_string());
    }
    let n_groups = trimmed.len().div_ceil(3);
    if n_groups > SCALE_NAMES.len() {
        return None;
    }
    let mut groups: Vec<u32> = Vec::with_capacity(n_groups);
    let mut end = trimmed.len();
    while end > 0 {
        let start = end.saturating_sub(3);
        groups.push(trimmed[start..end].parse().expect("group is 1-3 digits"));
        end = start;
    }
    let mut parts = Vec::new();
    for (i, &g) in groups.iter().enumerate().rev() {
        if g == 0 {
            continue;
        }
        let chunk = under_thousand(g);
        let scale = SCALE_NAMES[i];
        parts.push(if scale.is_empty() { chunk } else { format!("{chunk} {scale}") });
    }
    Some(parts.join(" "))
}

pub fn number_to_words(n: i64) -> String {
    if n == 0 {
        return "zero".into();
    }
    // unsigned_abs() gives the correct magnitude even for i64::MIN, where
    // `-n` would overflow.
    let mag = n.unsigned_abs();
    let words = digits_to_words(&mag.to_string())
        .expect("i64 magnitude always fits within SCALE_NAMES's 12 tiers");
    if n < 0 { format!("minus {words}") } else { words }
}

/// Best-effort words for a comma-stripped decimal-digit string of arbitrary
/// length: uses the i64 fast path when it fits, otherwise falls back to the
/// unbounded group-based conversion. Only falls back to the raw digits
/// themselves if the magnitude exceeds even that (astronomically large).
fn big_number_words(stripped: &str) -> String {
    match stripped.parse::<i64>() {
        Ok(n) => number_to_words(n),
        Err(_) => digits_to_words(stripped).unwrap_or_else(|| stripped.to_string()),
    }
}

fn frac_digits_to_words(frac: &str) -> String {
    frac.chars()
        .filter_map(|c| c.to_digit(10))
        .map(|d| ONES[d as usize].to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Strips trailing zeros from a fractional digit string (`"500"` -> `"5"`,
/// `"10"` -> `"1"`). Returns `None` when nothing remains (all zeros, or
/// empty), signaling that the whole "point ..." clause should be dropped:
/// misaki reads `5.0` as "five", not "five point zero". Used only by
/// `normalize`'s call sites, not by the public `decimal_to_words`, since that
/// function's contract is to read `frac` literally digit-by-digit and other
/// callers may rely on that.
fn strip_trailing_frac_zeros(frac: &str) -> Option<&str> {
    let trimmed = frac.trim_end_matches('0');
    if trimmed.is_empty() { None } else { Some(trimmed) }
}

fn ordinalize_last_word(words: &str) -> String {
    let (head, last) = words.rsplit_once(' ').unwrap_or(("", words));
    let last_ord = match last {
        "one" => "first".to_string(),
        "two" => "second".to_string(),
        "three" => "third".to_string(),
        "five" => "fifth".to_string(),
        "eight" => "eighth".to_string(),
        "nine" => "ninth".to_string(),
        "twelve" => "twelfth".to_string(),
        w if w.ends_with('y') => format!("{}ieth", &w[..w.len() - 1]),
        w => format!("{w}th"),
    };
    if head.is_empty() { last_ord } else { format!("{head} {last_ord}") }
}

fn ordinal_to_words(n: i64) -> String {
    if n < 20 {
        return ORDINAL_SMALL[n as usize].to_string();
    }
    ordinalize_last_word(&number_to_words(n))
}

/// Years 1100-1999 and 2000-2099 read as pairs: 2026 -> "twenty twenty six".
fn year_to_words(n: i64) -> String {
    if !(1100..=2099).contains(&n) {
        return number_to_words(n);
    }
    let (hi, lo) = (n / 100, n % 100);
    if lo == 0 {
        return format!("{} hundred", number_to_words(hi));
    }
    if lo < 10 {
        return format!("{} oh {}", number_to_words(hi), number_to_words(lo));
    }
    format!("{} {}", number_to_words(hi), number_to_words(lo))
}

pub fn decimal_to_words(int_part: i64, frac: &str) -> String {
    format!("{} point {}", number_to_words(int_part), frac_digits_to_words(frac))
}

fn strip_commas(s: &str) -> String {
    s.replace(',', "")
}

// Compiling a regex is orders of magnitude more expensive than running it;
// measured at ~391 microseconds per normalize() call before this change (with
// five fresh Regex::new calls dominating that at ~99% of the total), so these
// are compiled once and reused across calls instead of per-call.
static CURRENCY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$(\d(?:[\d,]*\d)?)(?:\.(\d{1,2}))?").unwrap());
static PERCENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d[\d,]*)(?:\.(\d+))?\s*%").unwrap());
static ORDINAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(\d[\d,]*)(?:st|nd|rd|th)\b").unwrap());
static DECIMAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(\d[\d,]*)\.(\d+)\b").unwrap());
static INTEGER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d[\d,]*\b").unwrap());
static WS_RUN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" {2,}").unwrap());

pub fn normalize(text: &str) -> String {
    let mut out = text.to_string();

    out = CURRENCY
        .replace_all(&out, |c: &regex::Captures| {
            let stripped = strip_commas(&c[1]);
            let is_zero = stripped.chars().all(|ch| ch == '0');
            let is_one = !is_zero && stripped.trim_start_matches('0') == "1";
            let unit = if is_one { "dollar" } else { "dollars" };
            match c.get(2).map(|m| m.as_str()) {
                Some(cents) => {
                    // 1-2 fractional digits are read as the literal cents
                    // value (".5" -> "five cents", not "fifty cents").
                    let cv: i64 = cents.parse().unwrap_or(0);
                    let cu = if cv == 1 { "cent" } else { "cents" };
                    // Omit the cents clause whenever the cents value is
                    // zero ("$5.00" -> "five dollars", not "... and zero
                    // cents"). Omit the dollars clause only when the whole
                    // part is zero AND the cents value is non-zero
                    // ("$0.05" -> "five cents"); "$0.00" still reads as
                    // "zero dollars" since there's no cents clause to fall
                    // back on.
                    match (is_zero, cv == 0) {
                        (true, false) => format!("{} {cu}", number_to_words(cv)),
                        (_, true) => format!("{} {unit}", big_number_words(&stripped)),
                        (false, false) => format!(
                            "{} {unit} and {} {cu}",
                            big_number_words(&stripped),
                            number_to_words(cv)
                        ),
                    }
                }
                None => format!("{} {unit}", big_number_words(&stripped)),
            }
        })
        .into_owned();

    out = PERCENT
        .replace_all(&out, |c: &regex::Captures| {
            let stripped = strip_commas(&c[1]);
            match c.get(2).map(|m| m.as_str()) {
                Some(frac) => match strip_trailing_frac_zeros(frac) {
                    Some(f) => match stripped.parse::<i64>() {
                        Ok(whole) => format!("{} percent", decimal_to_words(whole, f)),
                        Err(_) => format!(
                            "{} point {} percent",
                            big_number_words(&stripped),
                            frac_digits_to_words(f)
                        ),
                    },
                    None => format!("{} percent", big_number_words(&stripped)),
                },
                None => format!("{} percent", big_number_words(&stripped)),
            }
        })
        .into_owned();

    out = ORDINAL
        .replace_all(&out, |c: &regex::Captures| {
            let stripped = strip_commas(&c[1]);
            match stripped.parse::<i64>() {
                Ok(n) => ordinal_to_words(n),
                Err(_) => {
                    let words = digits_to_words(&stripped).unwrap_or(stripped);
                    ordinalize_last_word(&words)
                }
            }
        })
        .into_owned();

    out = DECIMAL
        .replace_all(&out, |c: &regex::Captures| {
            let stripped = strip_commas(&c[1]);
            match strip_trailing_frac_zeros(&c[2]) {
                Some(f) => match stripped.parse::<i64>() {
                    Ok(int_part) => decimal_to_words(int_part, f),
                    Err(_) => format!(
                        "{} point {}",
                        big_number_words(&stripped),
                        frac_digits_to_words(f)
                    ),
                },
                None => match stripped.parse::<i64>() {
                    Ok(int_part) => number_to_words(int_part),
                    Err(_) => big_number_words(&stripped),
                },
            }
        })
        .into_owned();

    out = INTEGER
        .replace_all(&out, |c: &regex::Captures| {
            let raw = &c[0];
            let stripped = strip_commas(raw);
            // a bare 4-digit token with no separators reads as a year
            if raw.len() == 4 && !raw.contains(',') {
                let n: i64 = stripped.parse().unwrap_or(0);
                if (1100..=2099).contains(&n) {
                    return year_to_words(n);
                }
            }
            big_number_words(&stripped)
        })
        .into_owned();

    // Pad each replacement with spaces so it can't fuse into its neighbors
    // when there's no whitespace around the symbol in the source ("yes&no"
    // must become "yes and no", not "yesandno" -- the latter is absent from
    // the lexicon and yields empty output). Where the symbol already had
    // surrounding whitespace ("a & b"), padding both sides creates a run of
    // two spaces, so collapse runs of spaces back down to one afterward.
    out = out.replace('&', " and ").replace('+', " plus ").replace('@', " at ");
    WS_RUN.replace_all(&out, " ").into_owned()
}

#[cfg(test)]
mod tests {
    use super::{normalize, number_to_words};

    #[test]
    fn cardinals() {
        assert_eq!(number_to_words(0), "zero");
        assert_eq!(number_to_words(21), "twenty one");
        assert_eq!(number_to_words(1234), "one thousand two hundred thirty four");
        assert_eq!(number_to_words(1000000), "one million");
    }

    #[test]
    fn currency_reads_as_dollars_and_cents() {
        assert_eq!(
            normalize("$1,234.56"),
            "one thousand two hundred thirty four dollars and fifty six cents"
        );
        assert_eq!(normalize("$5"), "five dollars");
        assert_eq!(normalize("$1"), "one dollar");
    }

    #[test]
    fn percent_and_symbols() {
        assert_eq!(normalize("50%"), "fifty percent");
        assert_eq!(normalize("a & b"), "a and b");
    }

    #[test]
    fn ordinals() {
        assert_eq!(normalize("the 1st and 2nd"), "the first and second");
        assert_eq!(normalize("4th"), "fourth");
    }

    #[test]
    fn bare_integers_and_years() {
        assert_eq!(normalize("I have 3 apples"), "I have three apples");
        assert_eq!(normalize("in 2026"), "in twenty twenty six");
    }

    // Regression tests for review findings (task 6 fix pass). Reference
    // values verified against Python misaki (misaki.en.G2P), see
    // .superpowers/sdd/task-6-report.md for the commands run.

    #[test]
    fn percent_with_decimal_reads_point_not_literal_dot() {
        // finding 1: "3.5%" used to yield "three.five percent"
        assert_eq!(normalize("3.5%"), "three point five percent");
    }

    #[test]
    fn ordinal_with_comma_grouped_thousands() {
        // finding 2: the ordinal regex used to strand the "1," in "1,234th"
        assert_eq!(
            normalize("the 1,234th visitor"),
            "the one thousand two hundred thirty fourth visitor"
        );
    }

    #[test]
    fn currency_with_single_fractional_digit_reads_as_cents_digit() {
        // finding 3: ".5" is five cents, not fifty cents, and the "." must
        // not survive as a literal character.
        assert_eq!(
            normalize("$1234.5"),
            "one thousand two hundred thirty four dollars and five cents"
        );
    }

    #[test]
    fn currency_zero_dollars_drops_dollars_clause() {
        // finding 4: misaki drops "zero dollars and" when the whole part is
        // zero.
        assert_eq!(normalize("$0.99"), "ninety nine cents");
    }

    #[test]
    fn number_to_words_handles_magnitudes_past_billion() {
        // finding 5: SCALES used to stop at billion, so any i64 needing a
        // trillion+ tier would panic indexing ONES out of bounds.
        assert_eq!(
            number_to_words(2_000_000_000_000),
            "two trillion"
        );
        assert_eq!(
            number_to_words(i64::MAX),
            "nine quintillion two hundred twenty three quadrillion three \
             hundred seventy two trillion thirty six billion eight hundred \
             fifty four million seven hundred seventy five thousand eight \
             hundred seven"
        );
    }

    #[test]
    fn number_to_words_handles_i64_min_without_overflow() {
        // finding 6: `-n` on i64::MIN overflows; unsigned_abs() must be used
        // instead. Magnitude of i64::MIN is 9223372036854775808.
        assert_eq!(
            number_to_words(i64::MIN),
            "minus nine quintillion two hundred twenty three quadrillion \
             three hundred seventy two trillion thirty six billion eight \
             hundred fifty four million seven hundred seventy five thousand \
             eight hundred eight"
        );
    }

    #[test]
    fn digit_run_past_i64_max_is_not_silently_zeroed() {
        // finding 7: a 20-digit run used to parse().unwrap_or(0) into the
        // single word "zero". Python misaki (num2words) instead spells out
        // the full magnitude; verified with:
        //   from misaki import en, espeak
        //   g = en.G2P(trf=False, british=False,
        //              fallback=espeak.EspeakFallback(british=False))
        //   g("12345678901234567890")
        // -> "twelve quintillion three hundred forty five quadrillion six
        //     hundred seventy eight trillion nine hundred one billion two
        //     hundred thirty four million five hundred sixty seven thousand
        //     eight hundred ninety"
        assert_eq!(
            normalize("a tracking number 12345678901234567890 here"),
            "a tracking number twelve quintillion three hundred forty five \
             quadrillion six hundred seventy eight trillion nine hundred one \
             billion two hundred thirty four million five hundred sixty \
             seven thousand eight hundred ninety here"
        );
    }

    // Regression tests for review findings (task 6 fix round 2). Reference
    // values verified against Python misaki (misaki.en.G2P), see
    // .superpowers/sdd/task-6-report.md for the commands run.

    #[test]
    fn currency_zero_cents_drops_cents_clause() {
        // finding 1: cents clause must be omitted whenever the cents value
        // is zero, regardless of the whole part.
        assert_eq!(normalize("$5.00"), "five dollars");
        assert_eq!(normalize("$10.00"), "ten dollars");
        assert_eq!(normalize("$1.00"), "one dollar");
    }

    #[test]
    fn currency_zero_whole_and_zero_cents_reads_zero_dollars() {
        // finding 1: "$0.00" has no non-zero cents to fall back on, so
        // unlike "$0.05" it still reads as "zero dollars", not "zero cents".
        assert_eq!(normalize("$0.00"), "zero dollars");
    }

    #[test]
    fn decimal_trailing_zeros_are_stripped() {
        // finding 2: trailing fractional zeros are dropped, and the whole
        // "point ..." clause disappears when nothing remains.
        assert_eq!(normalize("5.0"), "five");
        assert_eq!(normalize("5.10"), "five point one");
        assert_eq!(normalize("5.500"), "five point five");
        assert_eq!(normalize("0.0"), "zero");
        assert_eq!(normalize("0.5"), "zero point five");
        assert_eq!(normalize("3.14159"), "three point one four one five nine");
    }

    #[test]
    fn percent_with_trailing_zero_decimal_drops_point_clause() {
        // finding 2: the first fix round routed percent-with-decimal through
        // decimal_to_words, so this regressed to "fifty point zero zero
        // percent" until the trailing-zero strip was applied here too.
        assert_eq!(normalize("50.00%"), "fifty percent");
    }

    // Final-review finding: the currency regex's `[\d,]*` had no trailing
    // `\b`, so it greedily consumed a clause-terminating comma. The sibling
    // integer regex has always had that `\b`; currency didn't. Kokoro uses
    // `,` as a prosodic pause token, so this silently deleted a pause after
    // every "$N," construction. Reference misaki keeps the comma:
    // "I paid $20, then left" -> "I paid twenty dollars, then left".

    #[test]
    fn currency_does_not_swallow_a_trailing_comma() {
        assert_eq!(normalize("I paid $20, then left"), "I paid twenty dollars, then left");
    }

    #[test]
    fn currency_with_cents_does_not_swallow_a_trailing_comma() {
        assert_eq!(
            normalize("It cost $1,234.56, unfortunately"),
            "It cost one thousand two hundred thirty four dollars and fifty \
             six cents, unfortunately"
        );
    }

    #[test]
    fn currency_trailing_comma_with_no_following_space() {
        assert_eq!(normalize("$5,for real"), "five dollars,for real");
    }

    // Final-review finding: '&'/'+'/'@' were substituted without surrounding
    // spaces, so a symbol with no adjacent whitespace fused into one token
    // with its neighbors ("yes&no" -> "yesandno", not in the lexicon ->
    // empty G2P output). The substitution now pads both sides and collapses
    // any resulting run of spaces, so already-spaced input isn't doubled.

    #[test]
    fn symbols_are_padded_when_glued_to_neighbors() {
        assert_eq!(normalize("yes&no"), "yes and no");
        assert_eq!(normalize("a+b"), "a plus b");
        assert_eq!(normalize("me@work"), "me at work");
    }

    #[test]
    fn already_spaced_symbols_do_not_get_double_spaced() {
        assert_eq!(normalize("a & b"), "a and b");
        assert_eq!(normalize("a + b"), "a plus b");
        assert_eq!(normalize("a @ b"), "a at b");
    }
}
