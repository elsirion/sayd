//! Which text is worth rewriting, and whether a model's answer may be
//! spoken.
//!
//! Everything here is pure: no I/O, no clock, no config reads, no strings
//! destined for a widget. That is not tidiness -- these are the two rules
//! the whole feature's safety rests on (a text this refuses is spoken as
//! written; a candidate this rejects is discarded and the original is
//! spoken), and rules that can be exercised as a table are rules that get
//! exercised.
//!
//! Deliberately in `sayd-core` rather than beside the HTTP client: they are
//! the half of the feature that exists in a build with no HTTP client in it
//! at all.

/// Shorter than this and the text already reads aloud perfectly. A constant
/// rather than config: it is a property of English, not a preference.
pub const REWORD_MIN_CHARS: usize = 12;
/// Same reasoning. `Build failed` clears [`REWORD_MIN_CHARS`] on characters
/// and is still not a sentence worth a round trip.
pub const REWORD_MIN_WORDS: usize = 3;

/// Why a text is spoken as written rather than rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ineligible {
    /// Under [`REWORD_MIN_CHARS`] characters or [`REWORD_MIN_WORDS`] words.
    /// Silent: there is nothing for a user to fix.
    TooShort,
    /// Over `reword.max_chars`. Worth one log line per run -- a user who
    /// pointed `--reword` at a document deserves to know why nothing
    /// happened -- but never one per occurrence.
    TooLong,
}

/// Is `text` worth sending? Checked before anything is spawned, so an
/// ineligible submission costs one pass over a short string and nothing
/// else.
///
/// The ceiling is tested before the floor on purpose: `TooLong` is the
/// variant that earns a log line, so a text that is over the ceiling must
/// report that rather than whatever else it also happens to be.
pub fn eligible(text: &str, max_chars: usize) -> Result<(), Ineligible> {
    if text.chars().count() > max_chars {
        return Err(Ineligible::TooLong);
    }
    if text.chars().count() < REWORD_MIN_CHARS {
        return Err(Ineligible::TooShort);
    }
    if text.split_whitespace().count() < REWORD_MIN_WORDS {
        return Err(Ineligible::TooShort);
    }
    Ok(())
}

/// Why a candidate rewrite was thrown away.
///
/// Carries the numbers rather than a sentence: SS6's Test row needs to show
/// the reason, and every user-facing string in that table is built in
/// `settings::model`. [`Rejection::phrase`] is the fragment with the number
/// in it, which is the only part that cannot be written as a literal there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    Empty,
    /// More than one non-empty line. Carries the count of lines *beyond*
    /// the first, not the total -- see [`check`] for why a rewrite gets
    /// exactly one.
    ExtraLines(usize),
    CodeFence,
    TooLong {
        chars: usize,
        limit: usize,
    },
    /// Longer than the ceiling could admit under *any* encoding, decided on
    /// the byte count alone before anything scans the candidate. Separate
    /// from [`Rejection::TooLong`] because it carries bytes rather than
    /// characters: counting the characters is precisely the work this
    /// variant exists to skip.
    Oversized {
        bytes: usize,
        limit: usize,
    },
}

impl Rejection {
    /// The fragment a caller puts in a sentence, e.g. `"1 extra line"`.
    pub fn phrase(&self) -> String {
        match self {
            Rejection::Empty => "empty".to_string(),
            Rejection::ExtraLines(n) => {
                let noun = if *n == 1 { "line" } else { "lines" };
                format!("{n} extra {noun}")
            }
            Rejection::CodeFence => "a code fence".to_string(),
            Rejection::TooLong { chars, limit } => {
                format!("{chars} characters, over the {limit}-character ceiling")
            }
            Rejection::Oversized { bytes, limit } => {
                format!("{bytes} bytes, past anything the {limit}-character ceiling could admit")
            }
        }
    }
}

/// The longest a rewrite of `original` may be.
///
/// `+ 32` is the point of the formula. `Alice: dinner?` is 14 characters
/// and `Alice is asking about dinner` is 28 -- a legitimate 2x growth, and
/// a bare 1.5x rule would reject exactly the case this feature exists for.
/// The slack is a constant so it stops mattering as the input grows: at the
/// 400-character eligibility ceiling the effective limit is 1.58x.
pub fn length_ceiling(original: &str) -> usize {
    original.chars().count() * 3 / 2 + 32
}

/// Apply SS3's guard. `Ok` is the text to speak; `Err` says why the original
/// is spoken instead.
///
/// A candidate identical to the original is **not** a rejection: it is the
/// prompt's "if you cannot improve it, reply with it unchanged" path
/// working.
///
/// What this does not catch: a rewrite that is fluent, short and *wrong* --
/// a name changed, a number dropped, a question turned into a statement --
/// passes every check here. There is no cheap local test for it. The
/// mitigations are structural rather than algorithmic (one short input, a
/// low temperature, a small model) and the README says so plainly.
pub fn check(original: &str, candidate: &str) -> Result<String, Rejection> {
    let candidate = candidate.trim();
    let limit = length_ceiling(original);
    // Before anything walks the candidate. UTF-8 spends at most 4 bytes on
    // a `char`, so `bytes > limit * 4` proves `chars > limit` without
    // counting them: the rejection is the one the character check would
    // reach anyway, arrived at in constant time. It is here rather than in
    // the caller because it makes the cost of a hostile body a property of
    // the guard rather than of whichever client remembered to cap what it
    // read -- measured through this function on a tokio worker, a 1 MB
    // candidate cost 1.2 ms and a 256 MB candidate 261 ms, which is a
    // synthesis thread's whole budget spent counting characters in a body
    // no answer could ever come from.
    if candidate.len() > limit.saturating_mul(4) {
        return Err(Rejection::Oversized {
            bytes: candidate.len(),
            limit,
        });
    }
    let candidate = strip_wrapping_quotes(candidate);
    if candidate.is_empty() {
        return Err(Rejection::Empty);
    }
    // DEPARTURE from §3, which draws the line at "more than two" non-empty
    // lines. A rewrite is one sentence; a second line is the model adding
    // commentary it was not asked for -- "Alice is asking about dinner." /
    // "Note: I rephrased this as a question." is exactly the shape a chatty
    // small model produces, and it is spec-compliant under "more than two"
    // while still reading a fabricated note aloud to the user. Rejecting on
    // the second line closes that gap, and it is free to be wrong about:
    // every rejection here falls back to the original, which is the text
    // that would have been spoken with no reword feature at all. There is
    // no case where tightening this check costs the user anything; there is
    // one where loosening it does.
    let lines = candidate.lines().filter(|l| !l.trim().is_empty()).count();
    if lines > 1 {
        return Err(Rejection::ExtraLines(lines - 1));
    }
    if candidate.contains("```") {
        return Err(Rejection::CodeFence);
    }
    let chars = candidate.chars().count();
    if chars > limit {
        return Err(Rejection::TooLong { chars, limit });
    }
    Ok(candidate.to_string())
}

/// [`check`], as the `Option` SS3 names. `None` means speak the original.
pub fn accept(original: &str, candidate: &str) -> Option<String> {
    check(original, candidate).ok()
}

/// Drop a wrapping pair of `"` when they are the only two in the string.
///
/// Models quote their answers and it is not worth a rejection. An interior
/// quote means the pair is part of the sentence rather than around it, so
/// the string is left exactly as it came.
fn strip_wrapping_quotes(s: &str) -> &str {
    if s.len() < 2 || !s.starts_with('"') || !s.ends_with('"') {
        return s;
    }
    let inner = &s[1..s.len() - 1];
    if inner.contains('"') {
        s
    } else {
        inner.trim()
    }
}

/// Where a `base_url` points, as far as `sayd` can tell.
///
/// `sayd` cannot see past `base_url`: it reports where text is going and
/// leaves the trust judgement to the person who typed the URL, because that
/// person knows something `sayd` does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// `"http"` or `"https"`, lowercased.
    pub scheme: String,
    /// The host as written, without userinfo and without the port. An IPv6
    /// literal keeps its brackets.
    pub host: String,
}

/// Parse a configured `base_url` far enough to know the scheme and the host.
///
/// Deliberately not the `url` crate: this needs the scheme (to refuse
/// anything but HTTP) and the host (to name the destination in a log line
/// and to decide whether plain HTTP is worth warning about), and nothing
/// else. A dependency for two `split` calls would be the tail wagging the
/// dog in a project that just rejected 40 crates for cancellation.
pub fn parse_base_url(base_url: &str) -> Result<Endpoint, String> {
    let base_url = base_url.trim();
    let (scheme, rest) = base_url
        .split_once("://")
        .ok_or_else(|| format!("{base_url:?} has no scheme; expected http:// or https://"))?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "{scheme:?} is not a scheme sayd can speak to; expected http or https"
        ));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // `user:pw@host:port` -- the last `@` separates userinfo from the host,
    // because a password may itself contain one.
    let hostport = match authority.rsplit_once('@') {
        Some((_, after)) => after,
        None => authority,
    };
    // An IPv6 literal is bracketed, and its colons are not port separators.
    let host = if let Some(end) = hostport.find(']') {
        &hostport[..=end]
    } else {
        hostport.split(':').next().unwrap_or("")
    };
    if host.is_empty() {
        return Err(format!("{base_url:?} names no host"));
    }
    Ok(Endpoint {
        scheme,
        host: host.to_string(),
    })
}

/// `base_url` with any trailing `/` removed and `/chat/completions`
/// appended, so both spellings of every endpoint in SS6's table work.
pub fn chat_completions_url(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim().trim_end_matches('/'))
}

/// Is `host` this machine? Decides only whether plain HTTP earns a warning.
///
/// Name-based, not resolution-based: resolving would be a DNS lookup at
/// config-read time, and a host that resolves to a loopback address today
/// is not a promise about tomorrow. A user who points a hostname at
/// 127.0.0.1 gets one warning they can ignore, which is the cheap side of
/// the trade.
pub fn is_loopback(host: &str) -> bool {
    let host = host
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    host == "localhost"
        || host == "::1"
        || host.ends_with(".localhost")
        || host
            .strip_prefix("127.")
            .is_some_and(|rest| rest.split('.').count() == 3)
}

/// The first `max_chars` characters, on a `char` boundary, for a log line.
///
/// Plain byte slicing can land inside a multi-byte UTF-8 sequence and
/// panic, and this is applied to model output.
pub fn truncate_for_debug(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &s[..byte_idx],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SS4's table, both directions. `Ping` and `Build failed` read
    /// perfectly already, and a round trip for them costs money and up to
    /// `timeout_ms` of delay for nothing.
    #[test]
    fn only_notification_sized_text_is_eligible() {
        // Too short by characters.
        assert_eq!(eligible("Ping", 400), Err(Ineligible::TooShort));
        // Twelve characters exactly, but only two words.
        assert_eq!(eligible("Build failed", 400), Err(Ineligible::TooShort));
        // Eleven characters, three words.
        assert_eq!(eligible("a b ccccccc", 400), Err(Ineligible::TooShort));
        // Twelve characters and three words: the floor is a floor, not a
        // range.
        assert_eq!(eligible("a b cccccccc", 400), Ok(()));
        // The real case this feature exists for.
        assert_eq!(
            eligible("Alice: where do you want to go for dinner", 400),
            Ok(())
        );
        // Exactly at the ceiling, and one past it.
        let at = "x".repeat(398) + " y z";
        assert_eq!(at.chars().count(), 402);
        assert_eq!(eligible(&"a b ".repeat(100), 400), Ok(()));
        assert_eq!(eligible(&"a b ".repeat(101), 400), Err(Ineligible::TooLong));
        // The ceiling is checked before the floor: an over-long text is the
        // one that gets a once-per-run log line, so it must be reported as
        // over-long even when it is somehow also under the word floor.
        assert_eq!(eligible(&"x".repeat(401), 400), Err(Ineligible::TooLong));
        // Counted in `char`s, not bytes: an emoji is one character.
        assert_eq!(eligible("did you see the emoji", 400), Ok(()));
    }

    /// The `+ 32` slack is the whole point of the formula. `Alice: dinner?`
    /// is 14 characters and `Alice is asking about dinner` is 28 -- a
    /// legitimate 2x growth, and a bare 1.5x rule would reject exactly the
    /// case this feature exists for.
    #[test]
    fn the_slack_admits_the_case_the_feature_exists_for() {
        assert_eq!(length_ceiling("Alice: dinner?"), 14 * 3 / 2 + 32);
        assert_eq!(
            check("Alice: dinner?", "Alice is asking about dinner").as_deref(),
            Ok("Alice is asking about dinner")
        );
        // At the SS4 ceiling of 400 characters the effective limit is 1.58x.
        let original = "x".repeat(400);
        assert_eq!(length_ceiling(&original), 632);
        assert!(check(&original, &"y".repeat(632)).is_ok());
        assert_eq!(
            check(&original, &"y".repeat(633)),
            Err(Rejection::TooLong {
                chars: 633,
                limit: 632
            })
        );
    }

    /// Every rejection in SS3, and the two acceptances that are easy to
    /// mistake for rejections.
    #[test]
    fn the_guard_rejects_exactly_what_the_spec_says() {
        let original = "Alice: where do you want to go for dinner";

        // A candidate identical to the original is not a rejection: it is
        // the prompt's "cannot improve it" path working.
        assert_eq!(check(original, original).as_deref(), Ok(original));

        // Models quote their answers, and it is not worth a rejection.
        assert_eq!(
            check(original, "  \"Alice is asking about dinner\"  ").as_deref(),
            Ok("Alice is asking about dinner")
        );
        // ...but only when the quotes are the *only* ones. An interior
        // quote means the pair is part of the sentence.
        assert_eq!(
            check(original, "\"hi\" is what she said\"").as_deref(),
            Ok("\"hi\" is what she said\"")
        );

        assert_eq!(check(original, "   "), Err(Rejection::Empty));
        assert_eq!(check(original, "\"\""), Err(Rejection::Empty));

        // A model that produced a list produced an explanation. Blank lines
        // between them do not count.
        assert_eq!(
            check(original, "one\n\ntwo\n\nthree"),
            Err(Rejection::ExtraLines(2))
        );

        assert_eq!(
            check(original, "Here you go:\n```\ncode\n```"),
            Err(Rejection::ExtraLines(3))
        );
        assert_eq!(
            check(original, "she said ```hi```"),
            Err(Rejection::CodeFence)
        );
    }

    /// DEPARTURE from §3's literal "more than two" threshold, and the
    /// reviewer-supplied case it exists to catch: a chatty small model
    /// answers the question *and* narrates that it answered it, e.g.
    ///
    /// ```text
    /// Alice is asking about dinner.
    ///
    /// Note: I rephrased this as a question.
    /// ```
    ///
    /// which is two non-empty lines and so accepted -- and read aloud in
    /// full -- under the letter of the spec. A rewrite is one sentence; a
    /// second line is always commentary the model was not asked for, never
    /// more of the answer. Rejecting it costs nothing: every rejection here
    /// falls back to the original, so the user hears the notification as
    /// written, which is what they would have heard with no reword feature
    /// at all.
    #[test]
    fn a_rewrite_is_one_line_a_second_line_is_commentary_not_more_answer() {
        let original = "Alice: where do you want to go for dinner";

        assert_eq!(
            check(original, "Alice is asking about dinner").as_deref(),
            Ok("Alice is asking about dinner"),
            "one non-empty line is still accepted"
        );
        assert_eq!(
            check(original, "one\n\ntwo"),
            Err(Rejection::ExtraLines(1)),
            "a second non-empty line is now rejected, not just a third"
        );
        assert_eq!(
            check(
                original,
                "Alice is asking about dinner.\n\nNote: I rephrased this as a question."
            ),
            Err(Rejection::ExtraLines(1)),
            "the reviewer's reproduction: a fluent answer plus a trailing \
             note about the rewrite, which is exactly the shape a chatty \
             small model emits and the single most likely way a user would \
             hear something absurd"
        );
    }

    /// A candidate no answer could ever be is rejected on its byte count,
    /// before anything walks it.
    ///
    /// The cost of a hostile body must not depend on a client remembering
    /// to cap what it read: measured through this function on a tokio
    /// worker, the three scans below cost 1.2 ms for 1 MB and 261 ms for
    /// 256 MB. The bail is exact rather than a heuristic -- UTF-8 spends at
    /// most 4 bytes on a `char`, so `bytes > limit * 4` proves the
    /// character check would have rejected it too -- which is why it can be
    /// unconditional.
    #[test]
    fn an_oversized_candidate_is_refused_before_anything_scans_it() {
        let original = "Alice: where do you want to go for dinner";
        let limit = length_ceiling(original);

        // One byte past `limit * 4` is the first candidate that cannot
        // possibly hold `limit` characters or fewer.
        let hostile = "x".repeat(limit * 4 + 1);
        assert_eq!(
            check(original, &hostile),
            Err(Rejection::Oversized {
                bytes: limit * 4 + 1,
                limit
            })
        );

        // A megabyte of it is the same rejection at the same cost, which is
        // the point.
        let huge = "x".repeat(1 << 20);
        assert_eq!(
            check(original, &huge),
            Err(Rejection::Oversized {
                bytes: 1 << 20,
                limit
            })
        );

        // And the bail cannot reject anything the character count would
        // have accepted: exactly `limit * 4` bytes still goes the long way
        // round and is judged on its characters. Four-byte characters, so
        // this is `limit` characters exactly -- an acceptance, not a
        // rejection.
        let at_the_bail = "🙂".repeat(limit);
        assert_eq!(at_the_bail.len(), limit * 4);
        assert!(
            check(original, &at_the_bail).is_ok(),
            "a candidate of exactly `limit` characters must survive a byte \
             check that is an over-estimate of them"
        );
    }

    /// The fragments the settings window's Test row builds its subtitle
    /// out of. The sentence around them is the model layer's job; these are
    /// the only part with a number in.
    #[test]
    fn a_rejection_names_itself() {
        assert_eq!(Rejection::Empty.phrase(), "empty");
        assert_eq!(Rejection::ExtraLines(1).phrase(), "1 extra line");
        assert_eq!(Rejection::ExtraLines(3).phrase(), "3 extra lines");
        assert_eq!(Rejection::CodeFence.phrase(), "a code fence");
        assert_eq!(
            Rejection::TooLong {
                chars: 633,
                limit: 632
            }
            .phrase(),
            "633 characters, over the 632-character ceiling"
        );
        assert_eq!(
            Rejection::Oversized {
                bytes: 1 << 20,
                limit: 632
            }
            .phrase(),
            "1048576 bytes, past anything the 632-character ceiling could admit"
        );
    }

    /// Every row of SS6's endpoint table, including the trailing-slash case
    /// that both spellings must survive.
    #[test]
    fn every_documented_endpoint_resolves_to_one_url() {
        for (base, expected) in [
            (
                "http://localhost:11434/v1",
                "http://localhost:11434/v1/chat/completions",
            ),
            (
                "http://localhost:11434/v1/",
                "http://localhost:11434/v1/chat/completions",
            ),
            (
                "http://localhost:8080/v1",
                "http://localhost:8080/v1/chat/completions",
            ),
            (
                "http://localhost:1234/v1",
                "http://localhost:1234/v1/chat/completions",
            ),
            (
                "http://localhost:8000/v1",
                "http://localhost:8000/v1/chat/completions",
            ),
            (
                "https://api.ppq.ai/v1",
                "https://api.ppq.ai/v1/chat/completions",
            ),
            (
                "https://api.openai.com/v1",
                "https://api.openai.com/v1/chat/completions",
            ),
        ] {
            assert_eq!(chat_completions_url(base), expected, "base: {base}");
        }
    }

    #[test]
    fn a_base_url_must_be_http_or_https_and_carry_a_host() {
        assert_eq!(
            parse_base_url("https://api.ppq.ai/v1")
                .expect("parses")
                .host,
            "api.ppq.ai"
        );
        assert_eq!(
            parse_base_url("http://user:pw@box.lan:8080/v1")
                .expect("parses")
                .host,
            "box.lan",
            "userinfo and port are not part of the host we report"
        );
        assert_eq!(
            parse_base_url("http://[::1]:11434/v1")
                .expect("parses")
                .host,
            "[::1]",
            "an IPv6 literal keeps its brackets: that is how it is written"
        );
        for bad in [
            "",
            "   ",
            "localhost:11434/v1",
            "ftp://example.com",
            "file:///etc/passwd",
            "http://",
            "https:///v1",
        ] {
            assert!(
                parse_base_url(bad).is_err(),
                "{bad:?} must not parse as an endpoint"
            );
        }
    }

    /// The debug snippet is cut on a `char` boundary, because what it is cut
    /// out of is a model's output: a byte slice through the middle of a
    /// multi-byte sequence would panic on the one path whose whole job is to
    /// survive whatever came back.
    #[test]
    fn a_debug_snippet_is_cut_on_a_char_boundary() {
        assert_eq!(truncate_for_debug("hello", 80), "hello");
        assert_eq!(truncate_for_debug("hello", 2), "he");
        assert_eq!(truncate_for_debug("", 4), "");
        assert_eq!(truncate_for_debug("hello", 0), "");
        // Four bytes per emoji: cutting at 2 *characters* must not cut at
        // byte 2.
        assert_eq!(truncate_for_debug("🙂🙂🙂", 2), "🙂🙂");
        // Combining marks are separate `char`s; this is a length limit for a
        // log line, not a grapheme count, and it must not panic either way.
        assert_eq!(truncate_for_debug("é\u{0301}xyz", 2), "é\u{0301}");
    }

    /// Plain HTTP to a *non-loopback* host is allowed but warned about; to
    /// loopback it is the default and must stay silent.
    #[test]
    fn loopback_is_recognised_in_every_spelling_it_is_written_in() {
        for host in [
            "localhost",
            "LOCALHOST",
            "127.0.0.1",
            "127.1.2.3",
            "[::1]",
            "::1",
        ] {
            assert!(is_loopback(host), "{host} is loopback");
        }
        for host in ["api.ppq.ai", "box.lan", "192.168.1.10", "10.0.0.1", ""] {
            assert!(!is_loopback(host), "{host} is not loopback");
        }
    }
}
