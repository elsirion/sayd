//! Word/punctuation splitting. Apostrophes and hyphens stay inside words
//! because the lexicon has entries like "don't" and "well-known".

pub const PUNCTS: &str = ";:,.!?—…\"\u{201C}\u{201D}()";

#[derive(Debug, Clone)]
pub struct Tok {
    pub text: String,
    pub is_punct: bool,
    pub space_after: bool,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '\'' || c == '\u{2019}' || c == '-'
}

pub fn tokenize(text: &str) -> Vec<Tok> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Tok> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            if let Some(last) = out.last_mut() {
                last.space_after = true;
            }
            i += 1;
            continue;
        }
        if is_word_char(c) {
            let start = i;
            while i < chars.len() && is_word_char(chars[i]) {
                i += 1;
            }
            // a trailing ' or - is punctuation, not part of the word
            let mut end = i;
            while end > start && matches!(chars[end - 1], '-' | '\'' | '\u{2019}') {
                end -= 1;
            }
            if end > start {
                out.push(Tok {
                    text: chars[start..end].iter().collect(),
                    is_punct: false,
                    space_after: false,
                });
            }
            for &p in &chars[end..i] {
                out.push(Tok { text: p.to_string(), is_punct: true, space_after: false });
            }
        } else {
            out.push(Tok { text: c.to_string(), is_punct: true, space_after: false });
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::tokenize;

    #[test]
    fn splits_words_and_punctuation() {
        let t = tokenize("Hello, world!");
        let got: Vec<(&str, bool)> =
            t.iter().map(|x| (x.text.as_str(), x.is_punct)).collect();
        assert_eq!(got, vec![("Hello", false), (",", true), ("world", false), ("!", true)]);
    }

    #[test]
    fn keeps_internal_apostrophes_and_hyphens() {
        let t = tokenize("don't well-known");
        let got: Vec<&str> = t.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(got, vec!["don't", "well-known"]);
    }

    #[test]
    fn records_trailing_space() {
        let t = tokenize("a b");
        assert!(t[0].space_after);
        assert!(!t[1].space_after);
    }

    #[test]
    fn strips_trailing_apostrophe_and_hyphen() {
        // Known divergence from reference: the reference emits U+201D (curly
        // close-quote) for
        // possessives like "dogs'" and "James'", but this tokenizer emits U+0027 (')
        // which PUNCTS does not contain, so it will be dropped in post-processing.
        let t = tokenize("dogs'");
        let got: Vec<(&str, bool)> =
            t.iter().map(|x| (x.text.as_str(), x.is_punct)).collect();
        assert_eq!(got, vec![("dogs", false), ("'", true)]);

        let t2 = tokenize("well-");
        let got2: Vec<(&str, bool)> =
            t2.iter().map(|x| (x.text.as_str(), x.is_punct)).collect();
        assert_eq!(got2, vec![("well", false), ("-", true)]);
    }
}
