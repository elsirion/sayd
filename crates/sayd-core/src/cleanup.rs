//! Turn selected text into something worth hearing.
//!
//! Order matters: code fences are dropped before anything inspects their
//! contents, and whitespace is collapsed last so earlier removals do not
//! leave gaps.
//!
//! URLs get special handling. After code fences are dropped, the remaining
//! text is segmented into alternating runs of non-URL text and URL matches.
//! Transforms that must never touch URL text — hyphenation rejoin, markdown
//! stripping, acronym spelling — run only on the non-URL segments; the
//! `UrlPolicy` replacement (the literal word "link", a bare host, or the URL
//! verbatim) is computed only for the URL segments. The results are
//! concatenated back together in their original order.
//!
//! This replaces an earlier placeholder-based scheme (hide URLs behind
//! `\u{E000}<index>\u{E001}` markers, restore after the markdown/acronym
//! passes) that assumed those private-use codepoints never occur in real
//! input. That assumption doesn't hold: Nerd Font and Powerline glyphs live
//! in exactly that codepoint range, and this daemon's primary input is
//! terminal selections, so users routinely paste text containing them.
//! Segmentation makes no assumption about which codepoints appear anywhere
//! in the input — URL text and non-URL text are simply never in the same
//! string at the same time while the URL-unsafe transforms run.
//!
//! The two remaining passes — the control-character strip and whitespace
//! collapse — run once, globally, on the concatenated result, and that is
//! safe:
//!
//! - The control-character strip is unconditional and must stay that way:
//!   it is the only thing standing between an embedded NUL and a downstream
//!   FFI `CString::new` call, and a test pins that guarantee. Running it
//!   globally cannot corrupt a URL span because it only ever *removes*
//!   characters, and a legitimate URL cannot contain a control character in
//!   the first place — there is nothing there to protect.
//! - Whitespace collapse is safe to run globally because the `URL` regex's
//!   exclusion set already excludes whitespace from a URL match, so a URL
//!   span can never contain, start with, or end with whitespace. Collapsing
//!   whitespace runs elsewhere in the string can therefore never reach into
//!   a URL span or merge two URL spans together.

use std::sync::LazyLock;

use regex::Regex;

use crate::config::{CleanupConfig, UrlPolicy};

static CODE_FENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)```.*?(?:```|$)").expect("static regex"));
// Excludes whitespace and the delimiters a URL is typically wrapped in
// (`<>()[]`), plus `*` and backtick, which are markdown emphasis/code
// syntax rather than realistic URL content. `_` and `-` are deliberately
// left in: both are common and legal in URLs (including hostnames).
static URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://[^\s<>\)\]*`]+").expect("static regex"));
static HYPHEN_BREAK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\w)-\s*\n\s*(\w)").expect("static regex"));
static EMPHASIS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\*\*|\*|__|_|`)").expect("static regex"));
static LIST_OR_HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*(?:#{1,6}\s+|[-*+]\s+|\d+\.\s+)").expect("static regex"));
// Match only 3+ letter acronyms; two-letter words like OK and ID are left alone intentionally.
static ACRONYM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[A-Z]{3,}\b").expect("static regex"));
static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("static regex"));

pub fn clean(text: &str, cfg: &CleanupConfig) -> String {
    let mut s = text.to_string();

    if cfg.drop_code_blocks {
        s = CODE_FENCE.replace_all(&s, " ").into_owned();
    }

    // Segment into alternating non-URL / URL runs so no transform below can
    // ever see both a URL and URL-unsafe syntax at once. See the module doc
    // comment for the full reasoning.
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for m in URL.find_iter(&s) {
        out.push_str(&clean_non_url(&s[last..m.start()], cfg));
        out.push_str(&resolve_url(m.as_str(), cfg));
        last = m.end();
    }
    out.push_str(&clean_non_url(&s[last..], cfg));
    s = out;

    s = s
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect();

    if cfg.collapse_whitespace {
        s = WHITESPACE.replace_all(&s, " ").trim().to_string();
    }

    s
}

/// Apply the transforms that must never touch URL text to a non-URL segment.
fn clean_non_url(segment: &str, cfg: &CleanupConfig) -> String {
    let mut s = segment.to_string();

    if cfg.rejoin_hyphenation {
        s = HYPHEN_BREAK.replace_all(&s, "$1$2").into_owned();
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

    s
}

/// Resolve a single matched URL span per `UrlPolicy`.
fn resolve_url(url: &str, cfg: &CleanupConfig) -> String {
    match cfg.urls {
        UrlPolicy::Link => "link".to_string(),
        UrlPolicy::Domain => host_of(url),
        UrlPolicy::Keep => url.to_string(),
    }
}

/// The host part of a URL, without scheme, port, path, query, fragment or
/// credentials.
///
/// Known-wrong, deferred: this splits the authority on `:` to strip the
/// port, which mangles IPv6 literals in brackets (e.g.
/// `https://[2001:db8::1]:8080/path`) because they contain colons of their
/// own. See `host_of_mangles_ipv6_literals_known_wrong_deferred` below for
/// the pinned baseline.
fn host_of(url: &str) -> String {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    // The authority ends at the first path, query, or fragment delimiter.
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
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
        assert_eq!(
            clean("see https://example.com/x?y=1 now", &c),
            "see link now"
        );
    }

    #[test]
    fn url_policy_domain_keeps_the_host() {
        let mut c = all_on();
        c.urls = UrlPolicy::Domain;
        assert_eq!(
            clean("see https://example.com/x now", &c),
            "see example.com now"
        );
    }

    #[test]
    fn url_policy_keep_leaves_it_alone() {
        let mut c = all_on();
        c.urls = UrlPolicy::Keep;
        assert_eq!(
            clean("see https://example.com now", &c),
            "see https://example.com now"
        );
    }

    #[test]
    fn strips_markdown_emphasis_and_code_ticks() {
        let c = all_on();
        assert_eq!(
            clean("**bold** and `code` and _em_", &c),
            "bold and code and em"
        );
    }

    #[test]
    fn strips_heading_hashes_and_list_bullets() {
        let c = all_on();
        assert_eq!(
            clean("# Title\n- one\n* two\n1. three", &c),
            "Title one two three"
        );
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
        assert_eq!(
            clean("I am OK with A and the DKG", &c),
            "I am OK with A and the D K G"
        );
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
        assert_eq!(
            clean("see https://example.com#section", &c),
            "see example.com"
        );
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

    // -- Placeholder-delimiter collision (Finding 1) -----------------------

    #[test]
    fn stray_placeholder_codepoints_alongside_a_real_url_are_not_swapped() {
        // The old scheme hid URLs behind `\u{E000}<index>\u{E001}` and
        // restored them with a whole-string `str::replace`. Input already
        // containing those exact codepoints collided with a real
        // placeholder of the same index and got silently overwritten with
        // unrelated URL text. With segmentation there is no placeholder to
        // collide with, so the stray codepoints must survive untouched and
        // the URL must resolve independently.
        let c = all_on();
        let out = clean("\u{E000}0\u{E001} see https://good.example.com/x now", &c);
        assert!(out.contains('\u{E000}') && out.contains('\u{E001}'));
        assert!(out.contains("link"));
        assert_ne!(out, "link see link now");
    }

    #[test]
    fn private_use_codepoints_with_no_url_pass_through_unmolested() {
        // Nerd Font / Powerline glyphs live in this exact private-use
        // range, and terminal selections are this daemon's primary input,
        // so these codepoints show up with no URL anywhere nearby. The old
        // scheme leaked raw, unresolved placeholder codepoints into speech
        // whenever the index didn't match a real URL; segmentation never
        // introduces a placeholder in the first place.
        let c = all_on();
        let input = "prompt \u{E0B0} branch \u{E000}\u{E001} done";
        assert_eq!(clean(input, &c), input);
    }

    // -- URL regex absorbing trailing markdown (Finding 2) ------------------

    #[test]
    fn url_policy_keep_strips_surrounding_markdown_emphasis() {
        let mut c = all_on();
        c.urls = UrlPolicy::Keep;
        c.strip_markdown = true;
        assert_eq!(
            clean("**https://example.com/a_b**", &c),
            "https://example.com/a_b"
        );
    }

    #[test]
    fn multiple_urls_domain_policy() {
        let mut c = all_on();
        c.urls = UrlPolicy::Domain;
        assert_eq!(
            clean(
                "see https://a.example.com/x and https://b.example.com/y now",
                &c
            ),
            "see a.example.com and b.example.com now"
        );
    }

    #[test]
    fn multiple_urls_keep_policy() {
        let mut c = all_on();
        c.urls = UrlPolicy::Keep;
        assert_eq!(
            clean(
                "see https://a.example.com/x and https://b.example.com/y now",
                &c
            ),
            "see https://a.example.com/x and https://b.example.com/y now"
        );
    }

    // -- Deferred: IPv6 literal baseline -------------------------------------

    #[test]
    fn host_of_mangles_ipv6_literals_known_wrong_deferred() {
        // `host_of` strips the port by splitting the authority on `:`,
        // which is wrong for a bracketed IPv6 literal: the literal's own
        // colons get split too, truncating the host to `[2001`. This
        // predates both fix rounds and is intentionally left as-is; this
        // test only pins the current (wrong) behaviour as a baseline for a
        // later fix.
        let mut c = all_on();
        c.urls = UrlPolicy::Domain;
        // The URL regex also stops at the literal's own `]` (excluded as a
        // URL-wrapping delimiter), so only `https://[2001:db8::1` is
        // matched as the URL; the rest becomes a trailing non-URL segment.
        assert_eq!(
            clean("see https://[2001:db8::1]:8080/path now", &c),
            "see [2001]:8080/path now"
        );
    }
}
