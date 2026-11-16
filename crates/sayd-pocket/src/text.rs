//! The package's text preparation (`pocket_tts/models/text_chunking.py`),
//! fixed to the english_2026-04 bundle: no padding of short inputs, no
//! semicolon rewriting, terminal punctuation appended, first letter
//! capitalized, curly single quotes replaced by `'`.

use crate::tokenizer::Tokenizer;

const TERMINAL: &str = ".!?\u{2026}";
const WEAK: &str = ",;:-\u{2013}\u{2014}";
const CLOSERS: &str = "\"'\u{201d}\u{2019})]\u{00bb}";

fn replace_characters(c: char) -> char {
    match c {
        '\u{2019}' | '\u{2018}' => '\'',
        c => c,
    }
}

/// Drop a `,;:` that follows a sentence mark (`Hi?, she` becomes `Hi? she`),
/// the package's `re.sub(r"([.!?…])\s*[,;:]", r"\1", text)`.
fn drop_comma_after_sentence_mark(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        out.push(c);
        i += 1;
        if ".!?\u{2026}".contains(c) {
            let mut j = i;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && ",;:".contains(chars[j]) {
                i = j + 1;
            }
        }
    }
    out
}

fn ensure_terminal_punctuation(text: String) -> String {
    let core = text.trim_end_matches(|c: char| CLOSERS.contains(c) || c == ' ');
    let closers = text[core.len()..].trim();
    match core.chars().next_back() {
        None => text,
        Some(c) if TERMINAL.contains(c) => text,
        Some(c) if WEAK.contains(c) => {
            let core = core.trim_end_matches(|c: char| WEAK.contains(c) || c == ' ');
            format!("{core}.{closers}")
        }
        Some(_) => text + ".",
    }
}

/// The text the model is fed, and the package's guess at how many frames to
/// keep generating after the end-of-speech signal. Empty (or blank) input
/// gives `("", 0)`, where the package raises.
pub fn prepare_text_prompt(text: &str) -> (String, usize) {
    let text: String = text.trim().chars().map(replace_characters).collect();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = drop_comma_after_sentence_mark(&text);
    if text.is_empty() {
        return (String::new(), 0);
    }
    let text = text.replace(['\n', '\r'], " ").replace("  ", " ");
    let frames_after_eos_guess = if text.split_whitespace().count() <= 4 {
        3
    } else {
        1
    };
    let mut chars = text.chars();
    let first = chars.next().expect("text is not empty");
    let text = if first.is_uppercase() {
        text
    } else {
        first.to_uppercase().chain(chars).collect()
    };
    (ensure_terminal_punctuation(text), frames_after_eos_guess)
}

fn is_digit(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_ascii_digit())
}

/// True when `tokens[start..]` begins right after the period of a number
/// like `3.5`.
fn is_decimal_period_boundary(tok: &Tokenizer, tokens: &[i64], start: usize) -> bool {
    let prefix: Vec<char> = tok.decode(&tokens[..start]).chars().collect();
    let suffix = tok.decode(&tokens[start..]);
    prefix.len() >= 2
        && prefix[prefix.len() - 1] == '.'
        && is_digit(Some(prefix[prefix.len() - 2]))
        && is_digit(suffix.chars().next())
}

/// Indices that cut `tokens` after each run of boundary tokens; first is 0
/// and last is `tokens.len()`.
fn find_boundary_indices(
    tokens: &[i64],
    boundary: &[i64],
    decimals: Option<&Tokenizer>,
) -> Vec<usize> {
    let mut indices = vec![0];
    let mut previous_was_boundary = false;
    for (idx, t) in tokens.iter().enumerate() {
        if boundary.contains(t) {
            previous_was_boundary = true;
            continue;
        }
        if previous_was_boundary {
            if decimals.is_some_and(|tok| is_decimal_period_boundary(tok, tokens, idx)) {
                previous_was_boundary = false;
                continue;
            }
            indices.push(idx);
        }
        previous_was_boundary = false;
    }
    indices.push(tokens.len());
    indices
}

fn segments(tok: &Tokenizer, tokens: &[i64], boundaries: &[usize]) -> Vec<(usize, String)> {
    boundaries
        .windows(2)
        .map(|w| (w[1] - w[0], tok.decode(&tokens[w[0]..w[1]])))
        .collect()
}

/// Split `text` into chunks of at most `max_tokens` tokens where it can:
/// at sentence ends first, then at `,;:` inside an oversized sentence,
/// regrouping neighbours that fit together.
pub fn split_into_best_sentences(tok: &Tokenizer, text: &str, max_tokens: usize) -> Vec<String> {
    let (prepared, _) = prepare_text_prompt(text);
    let tokens = tok.encode(prepared.trim());
    // The first id is the leading `▁`.
    let end_of_sentence = tok.encode(".!...?");
    let fallback = tok.encode(",;:");
    let sentence_boundaries = find_boundary_indices(&tokens, &end_of_sentence[1..], Some(tok));

    let mut refined = Vec::new();
    for (nb_tokens, sentence) in segments(tok, &tokens, &sentence_boundaries) {
        if nb_tokens <= max_tokens {
            refined.push((nb_tokens, sentence));
            continue;
        }
        let sub_tokens = tok.encode(sentence.trim());
        let sub_boundaries = find_boundary_indices(&sub_tokens, &fallback[1..], None);
        let sub = segments(tok, &sub_tokens, &sub_boundaries);
        if sub.len() > 1 {
            refined.extend(sub);
        } else {
            refined.push((nb_tokens, sentence));
        }
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_tokens = 0;
    for (nb_tokens, sentence) in refined {
        if current.is_empty() {
            current = sentence;
            current_tokens = nb_tokens;
        } else if current_tokens + nb_tokens > max_tokens {
            chunks.push(current.trim().to_string());
            current = sentence;
            current_tokens = nb_tokens;
        } else {
            current.push(' ');
            current.push_str(&sentence);
            current_tokens += nb_tokens;
        }
    }
    if !current.is_empty() {
        chunks.push(current.trim().to_string());
    }
    chunks
}
