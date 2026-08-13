//! Turn selected text into something worth hearing.
//!
//! Order matters: code fences are dropped before anything inspects their
//! contents, URLs are handled before markdown strips their punctuation, and
//! whitespace is collapsed last so earlier removals do not leave gaps.

use std::sync::LazyLock;

use regex::Regex;

use crate::config::{CleanupConfig, UrlPolicy};

static CODE_FENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)```.*?(?:```|$)").expect("static regex"));
static URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://[^\s<>\)\]]+").expect("static regex"));
static HYPHEN_BREAK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\w)-\s*\n\s*(\w)").expect("static regex"));
static EMPHASIS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\*\*|\*|__|_|`)").expect("static regex"));
static LIST_OR_HEADING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*(?:#{1,6}\s+|[-*+]\s+|\d+\.\s+)").expect("static regex")
});
// Match only 3+ letter acronyms; two-letter words like OK and ID are left alone intentionally.
static ACRONYM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[A-Z]{3,}\b").expect("static regex"));
static WHITESPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+").expect("static regex"));

pub fn clean(text: &str, cfg: &CleanupConfig) -> String {
    let mut s = text.to_string();

    if cfg.drop_code_blocks {
        s = CODE_FENCE.replace_all(&s, " ").into_owned();
    }

    if cfg.rejoin_hyphenation {
        s = HYPHEN_BREAK.replace_all(&s, "$1$2").into_owned();
    }

    match cfg.urls {
        UrlPolicy::Link => {
            s = URL.replace_all(&s, "link").into_owned();
        }
        UrlPolicy::Domain => {
            s = URL
                .replace_all(&s, |caps: &regex::Captures| host_of(&caps[0]))
                .into_owned();
        }
        UrlPolicy::Keep => {}
    }

    if cfg.strip_markdown {
        s = LIST_OR_HEADING.replace_all(&s, "").into_owned();
        s = EMPHASIS.replace_all(&s, "").into_owned();
        s = s.replace('|', " ");
    }

    if cfg.spell_acronyms {
        s = ACRONYM
            .replace_all(&s, |caps: &regex::Captures| {
                caps[0]
                    .chars()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .into_owned();
    }

    s = s
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect();

    if cfg.collapse_whitespace {
        s = WHITESPACE.replace_all(&s, " ").trim().to_string();
    }

    s
}

/// The host part of a URL, without scheme, port, path or credentials.
fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url)
        .rsplit('@')
        .next()
        .unwrap_or(url)
        .split(':')
        .next()
        .unwrap_or(url)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CleanupConfig, UrlPolicy};

    fn all_on() -> CleanupConfig {
        CleanupConfig::default()
    }

    #[test]
    fn collapses_whitespace_runs() {
        let c = all_on();
        assert_eq!(clean("a   b\n\n\tc", &c), "a b c");
    }

    #[test]
    fn rejoins_hyphenated_line_breaks() {
        let c = all_on();
        assert_eq!(clean("inter-\nnational", &c), "international");
    }

    #[test]
    fn replaces_urls_with_the_word_link() {
        let c = all_on();
        assert_eq!(clean("see https://example.com/x?y=1 now", &c), "see link now");
    }

    #[test]
    fn url_policy_domain_keeps_the_host() {
        let mut c = all_on();
        c.urls = UrlPolicy::Domain;
        assert_eq!(clean("see https://example.com/x now", &c), "see example.com now");
    }

    #[test]
    fn url_policy_keep_leaves_it_alone() {
        let mut c = all_on();
        c.urls = UrlPolicy::Keep;
        assert_eq!(clean("see https://example.com now", &c), "see https://example.com now");
    }

    #[test]
    fn strips_markdown_emphasis_and_code_ticks() {
        let c = all_on();
        assert_eq!(clean("**bold** and `code` and _em_", &c), "bold and code and em");
    }

    #[test]
    fn strips_heading_hashes_and_list_bullets() {
        let c = all_on();
        assert_eq!(clean("# Title\n- one\n* two\n1. three", &c), "Title one two three");
    }

    #[test]
    fn drops_fenced_code_blocks_entirely() {
        let c = all_on();
        let input = "before\n```rust\nfn main() {}\n```\nafter";
        assert_eq!(clean(input, &c), "before after");
    }

    #[test]
    fn unterminated_code_fence_drops_to_end_of_text() {
        let c = all_on();
        assert_eq!(clean("before\n```\nnever closed", &c), "before");
    }

    #[test]
    fn spells_out_allcaps_acronyms() {
        let c = all_on();
        assert_eq!(clean("the HTLC failed", &c), "the H T L C failed");
    }

    #[test]
    fn leaves_single_letters_and_normal_words_alone() {
        let c = all_on();
        assert_eq!(clean("I am OK with A and the DKG", &c), "I am OK with A and the D K G");
    }

    #[test]
    fn strips_control_characters() {
        let c = all_on();
        assert_eq!(clean("a\u{0007}b\u{001b}c", &c), "abc");
    }

    #[test]
    fn every_transform_can_be_disabled() {
        let c = CleanupConfig {
            collapse_whitespace: false,
            rejoin_hyphenation: false,
            urls: UrlPolicy::Keep,
            strip_markdown: false,
            drop_code_blocks: false,
            spell_acronyms: false,
        };
        let input = "**x**  https://a.b\nHTLC";
        assert_eq!(clean(input, &c), input);
    }

    #[test]
    fn a_realistic_terminal_selection() {
        let c = all_on();
        let input = "error[E0308]: mismatched types\n  --> src/main.rs:12:5\n\nsee https://doc.rust-lang.org/E0308";
        let out = clean(input, &c);
        assert!(out.contains("mismatched types"));
        assert!(out.contains("link"));
        assert!(!out.contains('\n'));
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(clean("", &all_on()), "");
    }

    #[test]
    fn strips_embedded_nul_bytes() {
        let c = all_on();
        assert_eq!(clean("before\u{0000}after", &c), "beforeafter");
    }
}
