//! Split text into synthesis units.
//!
//! Two-phase, because the binding constraint is not characters. Kokoro accepts
//! at most 509 phoneme tokens per call, and the phoneme count of a string is
//! unknowable until after G2P. So `chunk` splits on sentence boundaries to a
//! character target, and `refit` re-splits anything that turns out to overrun
//! once phonemized. Skipping the second phase truncates audio mid-word on long
//! sentences.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub text: String,
    /// True when this chunk begins a paragraph, so playback can insert a pause.
    pub starts_paragraph: bool,
}

/// Sentence-boundary split, merged up to `target_chars`, paragraph-aware.
pub fn chunk(text: &str, target_chars: usize) -> Vec<Chunk> {
    let target = target_chars.max(1);
    let mut out: Vec<Chunk> = Vec::new();

    for para in text.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        let mut first_of_para = true;
        let mut buf = String::new();

        for sentence in sentences(para) {
            for piece in split_oversized(&sentence, target) {
                if !buf.is_empty() && buf.chars().count() + 1 + piece.chars().count() > target {
                    out.push(Chunk {
                        text: std::mem::take(&mut buf),
                        starts_paragraph: first_of_para,
                    });
                    first_of_para = false;
                }
                if !buf.is_empty() {
                    buf.push(' ');
                }
                buf.push_str(&piece);
            }
        }
        if !buf.is_empty() {
            out.push(Chunk { text: buf, starts_paragraph: first_of_para });
        }
    }
    out
}

/// Split on sentence-final punctuation, keeping the punctuation attached.
///
/// A run of consecutive terminal punctuation (`?!`, `!!!`, `...`) stays in
/// the sentence it ends, rather than each character starting a new
/// (degenerate, one-character) sentence -- the merge step in `chunk` would
/// otherwise glue those back together with spaces that were never in the
/// source. `\n` is excluded from the run so newline-triggered breaks are
/// unaffected.
fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        cur.push(ch);
        if matches!(ch, '.' | '!' | '?' | ';' | ':' | '\n') {
            if ch != '\n' {
                while let Some(&next) = chars.peek() {
                    if matches!(next, '.' | '!' | '?' | ';' | ':') {
                        cur.push(next);
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            let t = cur.trim().to_string();
            if !t.is_empty() {
                out.push(t);
            }
            cur.clear();
        }
    }
    let t = cur.trim().to_string();
    if !t.is_empty() {
        out.push(t);
    }
    out
}

/// Break a single over-long sentence, preferring comma boundaries, then spaces.
fn split_oversized(sentence: &str, target: usize) -> Vec<String> {
    if sentence.chars().count() <= target {
        return vec![sentence.to_string()];
    }
    let mut out = Vec::new();
    let mut rest = sentence.to_string();
    while rest.chars().count() > target {
        let limit = match rest.char_indices().nth(target) {
            Some((i, _)) => i,
            None => break,
        };
        let head = &rest[..limit];
        // Prefer a comma, then a space, inside the target window. If neither
        // exists the window falls inside a single long word: rather than
        // slicing mid-word, extend forward to that word's end (the next
        // space) so the whole word survives intact -- the chunk may then
        // exceed `target`, which is fine, since `target` is approximate and
        // `refit` enforces the hard limit. Mirrors `halve_until`, which
        // keeps an unsplittable word whole rather than truncating it.
        let cut = head
            .rfind(", ")
            .map(|i| i + 1)
            .or_else(|| head.rfind(' '))
            .or_else(|| rest[limit..].find(' ').map(|off| limit + off))
            .unwrap_or(rest.len());
        let (a, b) = rest.split_at(cut);
        let a = a.trim().to_string();
        if a.is_empty() {
            break; // no progress possible; emit the remainder below
        }
        out.push(a);
        rest = b.trim().to_string();
    }
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// Re-split chunks that overrun the phoneme-token budget.
///
/// `fits` is called with a chunk's text and answers whether its phonemized
/// form is within budget. A chunk that cannot be split further is passed
/// through unchanged rather than looped on -- synthesis will truncate it,
/// which is audible but finite.
pub fn refit(chunks: Vec<Chunk>, fits: impl Fn(&str) -> bool) -> Vec<Chunk> {
    let mut out = Vec::with_capacity(chunks.len());
    for c in chunks {
        if fits(&c.text) {
            out.push(c);
            continue;
        }
        let mut first = true;
        for piece in halve_until(&c.text, &fits) {
            out.push(Chunk {
                text: piece,
                starts_paragraph: first && c.starts_paragraph,
            });
            first = false;
        }
    }
    out
}

/// Repeatedly halve on word boundaries until each piece fits or is a single
/// word. Terminates because every recursion strictly reduces the word count.
fn halve_until(text: &str, fits: &impl Fn(&str) -> bool) -> Vec<String> {
    if fits(text) {
        return vec![text.to_string()];
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < 2 {
        return vec![text.to_string()]; // unsplittable; caller accepts truncation
    }
    let mid = words.len() / 2;
    let left = words[..mid].join(" ");
    let right = words[mid..].join(" ");
    let mut out = halve_until(&left, fits);
    out.extend(halve_until(&right, fits));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_sentence_boundaries() {
        let cs = chunk("One. Two. Three.", 6);
        assert_eq!(cs.len(), 3);
        assert_eq!(cs[0].text, "One.");
        assert_eq!(cs[2].text, "Three.");
    }

    #[test]
    fn merges_short_sentences_up_to_the_target() {
        let cs = chunk("One. Two. Three.", 100);
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].text, "One. Two. Three.");
    }

    #[test]
    fn marks_paragraph_starts() {
        let cs = chunk("First para.\n\nSecond para.", 100);
        assert_eq!(cs.len(), 2, "a blank line must force a chunk break");
        assert!(cs[0].starts_paragraph);
        assert!(cs[1].starts_paragraph);
    }

    #[test]
    fn splits_an_oversized_sentence_on_commas_then_spaces() {
        let long = "alpha bravo, charlie delta, echo foxtrot golf hotel india juliet";
        let cs = chunk(long, 20);
        assert!(cs.len() > 1);
        for c in &cs {
            assert!(c.text.chars().count() <= 25, "chunk too long: {:?}", c.text);
        }
    }

    #[test]
    fn never_produces_an_empty_chunk() {
        for input in ["", "   ", "\n\n\n", ".", "a"] {
            for c in chunk(input, 50) {
                assert!(!c.text.trim().is_empty(), "empty chunk from {input:?}");
            }
        }
    }

    #[test]
    fn preserves_all_words() {
        let input = "The quick brown fox. Jumps over the lazy dog, twice.";
        let rejoined: String = chunk(input, 15)
            .iter()
            .map(|c| c.text.clone())
            .collect::<Vec<_>>()
            .join(" ");
        for word in ["quick", "brown", "jumps", "lazy", "twice"] {
            assert!(
                rejoined.to_lowercase().contains(word),
                "lost {word:?} in {rejoined:?}"
            );
        }
    }

    #[test]
    fn refit_splits_chunks_that_overrun_the_token_budget() {
        // A chunk that fits the character target but not the token budget.
        let cs = vec![Chunk { text: "aaa bbb ccc ddd".into(), starts_paragraph: true }];
        // Pretend anything over 7 characters overruns.
        let out = refit(cs, |s| s.chars().count() <= 7);
        assert!(out.len() > 1, "expected a split, got {out:?}");
        for c in &out {
            assert!(c.text.chars().count() <= 7, "still too long: {:?}", c.text);
        }
    }

    #[test]
    fn refit_keeps_chunks_that_already_fit() {
        let cs = vec![Chunk { text: "short".into(), starts_paragraph: false }];
        let out = refit(cs.clone(), |_| true);
        assert_eq!(out, cs);
    }

    #[test]
    fn refit_only_the_first_piece_keeps_the_paragraph_flag() {
        let cs = vec![Chunk { text: "aaa bbb ccc".into(), starts_paragraph: true }];
        let out = refit(cs, |s| s.chars().count() <= 3);
        assert!(out[0].starts_paragraph);
        assert!(out[1..].iter().all(|c| !c.starts_paragraph));
    }

    #[test]
    fn refit_gives_up_on_an_unsplittable_chunk_rather_than_looping() {
        // A single word that can never fit. Must terminate and return it.
        let cs = vec![Chunk { text: "supercalifragilistic".into(), starts_paragraph: false }];
        let out = refit(cs, |s| s.chars().count() <= 3);
        assert_eq!(out.len(), 1, "unsplittable input must be passed through, not looped on");
    }

    /// Every whitespace-separated piece in the chunk output must equal some
    /// whitespace-separated word from the input (punctuation aside) -- i.e.
    /// `chunk` never slices a word in two.
    fn assert_no_word_is_shredded(input: &str, target: usize) {
        let cs = chunk(input, target);
        let input_words: std::collections::HashSet<String> = input
            .split_whitespace()
            .map(|w| w.trim_matches(|c: char| ",.;:!?".contains(c)).to_string())
            .collect();
        for c in &cs {
            for piece in c.text.split_whitespace() {
                let stripped = piece.trim_matches(|c: char| ",.;:!?".contains(c));
                assert!(
                    input_words.contains(stripped),
                    "chunk {:?} contains fragment {:?} not present as a whole word in {:?}",
                    c.text,
                    stripped,
                    input
                );
            }
        }
    }

    #[test]
    fn split_oversized_keeps_a_long_word_whole_when_alone() {
        let word = "supercalifragilisticexpialidocious";
        let cs = chunk(word, 10);
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].text, word, "a lone unsplittable word must come back whole");
    }

    #[test]
    fn split_oversized_keeps_a_long_word_whole_when_embedded() {
        // The exact fixture from the bug report: without the fix, "charlie"
        // was sliced into "charli" + "e".
        assert_no_word_is_shredded(
            "alpha bravo, charlie delta, echo foxtrot golf hotel india juliet",
            6,
        );
    }

    #[test]
    fn supercalifragilisticexpialidocious_is_long_at_a_small_target() {
        let input = "supercalifragilisticexpialidocious is long";
        assert_no_word_is_shredded(input, 20);
        let cs = chunk(input, 20);
        let rejoined: String = cs.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join(" ");
        assert!(
            rejoined.split_whitespace().any(|w| w == "supercalifragilisticexpialidocious"),
            "the long word must survive whole: {rejoined:?}"
        );
    }

    #[test]
    fn word_preservation_property() {
        let inputs = [
            "The quick brown fox jumps over the lazy dog.",
            "alpha bravo, charlie delta, echo foxtrot golf hotel india juliet",
            "supercalifragilisticexpialidocious is a very long word indeed.",
            "One. Two. Three. Four. Five. Six. Seven.",
            "Short sentence here, followed by another, and yet another one for good measure.",
            "Yes!!! What?! Wait... okay.",
        ];
        let targets = [1usize, 3, 6, 10, 20, 50, 100];
        let strip = |w: &str| w.trim_matches(|c: char| ",.;:!?".contains(c)).to_string();

        for input in inputs {
            let expected: Vec<String> =
                input.split_whitespace().map(strip).filter(|w| !w.is_empty()).collect();

            for &target in &targets {
                let cs = chunk(input, target);
                let rejoined: String =
                    cs.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join(" ");
                let actual: Vec<String> =
                    rejoined.split_whitespace().map(strip).filter(|w| !w.is_empty()).collect();
                assert_eq!(
                    actual, expected,
                    "word sequence mismatch for {input:?} at target {target}: got chunks {cs:?}"
                );
            }
        }
    }

    #[test]
    fn sentences_keeps_runs_of_terminal_punctuation_together() {
        assert_eq!(sentences("Yes!!!"), vec!["Yes!!!".to_string()]);
        assert_eq!(sentences("What?!"), vec!["What?!".to_string()]);
        assert_eq!(sentences("Wait..."), vec!["Wait...".to_string()]);
    }

    #[test]
    fn chunk_does_not_insert_spaces_into_a_run_of_terminal_punctuation() {
        // The exact fixture from the bug report: without the fix this
        // produced the chunk text "... Yes! ! !".
        let cs = chunk("... Yes!!!", 100);
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].text, "... Yes!!!");
    }

    #[test]
    fn sentences_still_breaks_at_a_newline_after_terminal_punctuation() {
        let got = sentences("Wait...\nNext line.");
        assert_eq!(got, vec!["Wait...".to_string(), "Next line.".to_string()]);
    }
}
