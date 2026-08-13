//! Turn selected text into something worth hearing.
//!
//! Order matters: code fences are dropped before anything inspects their
//! contents, and whitespace is collapsed last so earlier removals do not
//! leave gaps.
//!
//! URLs get special handling. Whatever `UrlPolicy` decides a URL should
//! become (the literal word "link", a bare host, or the URL verbatim) is
//! resolved up front, then the resolved text is hidden behind a placeholder
//! built from private-use codepoints (`U+E000`/`U+E001`) while the markdown
//! and acronym passes run. Those codepoints are not control characters, not
//! whitespace, and not markdown syntax, so nothing in between disturbs them.
//! The placeholder is swapped back for the resolved text after the acronym
//! pass but before the control-character strip and whitespace collapse:
//! after acronym spelling so a `Keep`-policy URL's uppercase letters are not
//! read out letter by letter, and before the control-character strip and
//! whitespace collapse so those two unconditional/default passes still see
//! (and can act on) the real, final text.

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

/// Placeholder delimiters. Private-use codepoints: never produced by any
/// other transform in this module, so they round-trip untouched.
const PLACEHOLDER_START: char = '\u{E000}';
const PLACEHOLDER_END: char = '\u{E001}';

pub fn clean(text: &str, cfg: &CleanupConfig) -> String {
    let mut s = text.to_string();

    if cfg.drop_code_blocks {
        s = CODE_FENCE.replace_all(&s, " ").into_owned();
    }

    if cfg.rejoin_hyphenation {
        s = HYPHEN_BREAK.replace_all(&s, "$1$2").into_owned();
    }

    // Resolve URLs per policy now, then hide the resolved text behind an
    // opaque placeholder so later transforms (markdown stripping, acronym
    // spelling) cannot corrupt it. See the module doc comment for why the
    // restore happens where it does.
    let mut resolved_urls: Vec<String> = Vec::new();
    s = URL
        .replace_all(&s, |caps: &regex::Captures| {
            let replacement = match cfg.urls {
                UrlPolicy::Link => "link".to_string(),
                UrlPolicy::Domain => host_of(&caps[0]),
                UrlPolicy::Keep => caps[0].to_string(),
            };
            let placeholder =
                format!("{PLACEHOLDER_START}{}{PLACEHOLDER_END}", resolved_urls.len());
            resolved_urls.push(replacement);
            placeholder
        })
        .into_owned();

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

    for (i, replacement) in resolved_urls.iter().enumerate() {
        let placeholder = format!("{PLACEHOLDER_START}{i}{PLACEHOLDER_END}");
        s = s.replace(&placeholder, replacement);
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

/// The host part of a URL, without scheme, port, path, query, fragment or
/// credentials.
fn host_of(url: &str) -> String {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    // The authority ends at the first path, query, or fragment delimiter.
    let authority_end = after_scheme.find(['/', '?', '#']).unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .split(':')
        .next()
        .unwrap_or(authority)
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

    #[test]
    fn url_policy_domain_strips_query_string_from_host() {
        let mut c = all_on();
        c.urls = UrlPolicy::Domain;
        assert_eq!(
            clean("go to https://example.com?x=1&y=2 now", &c),
            "go to example.com now"
        );
    }

    #[test]
    fn url_policy_domain_strips_fragment_from_host() {
        let mut c = all_on();
        c.urls = UrlPolicy::Domain;
        assert_eq!(clean("see https://example.com#section", &c), "see example.com");
    }

    #[test]
    fn url_policy_domain_preserves_underscore_in_hostname() {
        let mut c = all_on();
        c.urls = UrlPolicy::Domain;
        assert_eq!(
            clean("see https://my_site.example.com/path now", &c),
            "see my_site.example.com now"
        );
    }

    #[test]
    fn url_policy_keep_preserves_underscore_when_stripping_markdown() {
        let mut c = all_on();
        c.urls = UrlPolicy::Keep;
        c.strip_markdown = true;
        assert_eq!(
            clean("see https://example.com/foo_bar/baz now", &c),
            "see https://example.com/foo_bar/baz now"
        );
    }

    #[test]
    fn url_policy_domain_strips_credentials_and_port() {
        let mut c = all_on();
        c.urls = UrlPolicy::Domain;
        assert_eq!(
            clean("see https://user:pass@example.com:8080/path now", &c),
            "see example.com now"
        );
    }
}
