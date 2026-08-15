//! Whether a notification is spoken, and what it says.
//!
//! Everything here is pure and takes its clock as a parameter. That is not
//! tidiness: the rate-limit window, the counting and the coalesced follow-up
//! are the only real state in this milestone, and a test that had to sleep
//! for a 30-second window would not be written.
//!
//! `compose` does no whitespace normalisation of its own: a body can carry
//! runs of internal spaces or newlines straight through `strip_markup`, and
//! joining a body onto a summary can leave a run of two spaces behind. That
//! is fine, but only *because* the composed string is meant to be spoken
//! through `sayd_core::cleanup::clean` before it reaches the speaker, and
//! `CleanupConfig::default`'s `collapse_whitespace` is `true` -- Task 4 must
//! route every announcement through `clean`. Making that dependency
//! explicit here rather than leaving it implicit is the point of this
//! paragraph: nothing in this module enforces it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use sayd_core::config::NotificationConfig;

use super::Notification;

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Speak this now.
    Speak(String),
    /// Inside an open window: counted, and announced as "N more" when it
    /// closes.
    Count,
    /// `app_name` is not on the allowlist. Distinct from `NothingToSay`
    /// because §4's discovery log has to tell them apart: an unrecognised
    /// name is logged once per name per run so the user has something to
    /// add to `allow`, but a recognised application that simply had nothing
    /// to say this time must never hit that log, or a chat application with
    /// the occasional empty-summary notification would flood the very log
    /// the allowlist exists to keep quiet. Splitting the variant here means
    /// Task 4's logging branch reads as what it is instead of branching on
    /// a boolean, and it reuses `decide`'s own case-folded allowlist check
    /// rather than re-implementing it against a private `is_allowed`.
    NotAllowed,
    /// Allowed, but composed to nothing worth speaking (see `compose`).
    NothingToSay,
}

/// Per-application rate limiting.
///
/// One instance is meant to live for the daemon's lifetime, called from a
/// single task: `decide` on every notification, `due` on a tick (a second
/// is plenty against a 30s default window). Neither reads the clock itself
/// -- see the module doc -- so the caller owns the tick and the tests own
/// whatever timeline they like.
#[derive(Default)]
pub struct Limiter {
    /// Per application: when its window opened, and how many notifications
    /// have arrived since the one that opened it.
    ///
    /// Keyed by the lowercased application name so a burst from "signal"
    /// and one from "Signal" -- the same application, spelled however that
    /// particular `Notify` call happened to spell it -- share one window
    /// rather than each getting to speak once.
    windows: HashMap<String, Window>,
    /// Follow-up announcements for windows that were retired by `decide`
    /// reopening them, rather than by `due` finding them expired.
    ///
    /// `decide` checks a window's own expiry before deciding whether this
    /// notification counts against it or opens a fresh one -- it has to, or
    /// an application that has been quiet for longer than `cooldown` would
    /// wrongly `Count` against a window that closed minutes ago instead of
    /// `Speak`ing again. That means `decide` can be the one to discover a
    /// window has closed, up to a second before the next `due` tick would
    /// have (Task 4 drives `due` on a one-second timer). When that happens
    /// the closed window's count must not simply be dropped on the floor by
    /// the fresh window overwriting it in the map -- it is queued here and
    /// handed back on the next `due` call, so the announcement is only ever
    /// delayed, never lost. This is what makes the ordering-independence
    /// promised above true: whether `due` or `decide` notices the closed
    /// window first, the follow-up still gets spoken.
    ///
    /// `(display_name, announcement)`, not a bare announcement: `due` drops a
    /// window whose application has since left `allow` (see its doc comment),
    /// and a follow-up parked here has to be subject to exactly the same
    /// check -- it is the same window's line, queued a moment earlier by a
    /// different code path. MINOR 1: it was not, so removing an application
    /// from the allowlist could still be followed by one last "N more
    /// notifications" from it, and `due`'s own doc comment promised
    /// otherwise. The name is kept alongside because the announcement string
    /// has already been composed by then and is not something to re-parse a
    /// name back out of.
    pending: Vec<(String, String)>,
}

struct Window {
    opened: Instant,
    suppressed: u32,
    /// The name as the application spells it, for the follow-up
    /// announcement: the map is keyed by a lowercased name so matching and
    /// counting are case-insensitive, but the follow-up itself echoes back
    /// whichever spelling opened the window -- "signal: 3 more
    /// notifications" if "signal" spoke first, "SIGNAL: ..." if "SIGNAL"
    /// did. That matches §3: `app_name` is what the application calls
    /// itself, and the allowlist, the discovery log and this follow-up are
    /// all supposed to agree with it rather than each picking their own
    /// canonical casing.
    display_name: String,
}

/// The follow-up line for a window being retired, or `None` when it saw no
/// suppressed traffic -- a bare "0 more notifications" would be worse than
/// silence, so a quiet window is retired without a word.
fn announcement(w: &Window) -> Option<String> {
    (w.suppressed > 0).then(|| {
        let noun = if w.suppressed == 1 {
            "notification"
        } else {
            "notifications"
        };
        format!("{}: {} more {noun}", w.display_name, w.suppressed)
    })
}

/// Has the window that opened at `opened` closed by `now`, given `cooldown`?
///
/// `opened + cooldown` panics on overflow, and `cooldown` comes straight
/// from hot-reloaded config: a fat-fingered `cooldown_secs` in the 20-digit
/// range must not crash a running daemon. `Instant::checked_add` turns that
/// into `None`; treating `None` as "not expired" is the only sane reading --
/// a cooldown that overflows `Instant` arithmetic is, for any timeline a
/// real clock will ever produce, one that never elapses.
fn is_expired(opened: Instant, cooldown: Duration, now: Instant) -> bool {
    match opened.checked_add(cooldown) {
        Some(closes) => now >= closes,
        None => false,
    }
}

impl Limiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide what happens to one notification, as of `now`.
    ///
    /// In order: not on the allowlist is `NotAllowed`; nothing to say once
    /// composed is `NothingToSay` -- neither opens a window, because there
    /// was never going to be anything to speak for it. `cooldown_secs == 0`
    /// means rate limiting is off, not a zero-length window that nothing
    /// could ever land inside: everything speaks. Otherwise, a still-open
    /// window from this application counts against it; a closed or absent
    /// one opens (or reopens) and speaks.
    ///
    /// A window `decide` finds already expired is retired right here rather
    /// than left for the caller's next `due` tick to notice: overwriting it
    /// in place with a fresh window would silently throw away whatever it
    /// had counted, and by design `due` isn't the only place a window's
    /// expiry gets checked (see the module doc and `Limiter::pending`). Its
    /// follow-up, if it has one, is queued in `pending` and comes back out
    /// of the next `due` call.
    pub fn decide(&mut self, n: &Notification, cfg: &NotificationConfig, now: Instant) -> Decision {
        if !is_allowed(&n.app_name, cfg) {
            return Decision::NotAllowed;
        }
        let Some(text) = compose(n, cfg) else {
            return Decision::NothingToSay;
        };
        if cfg.cooldown_secs == 0 {
            return Decision::Speak(text);
        }

        let cooldown = Duration::from_secs(cfg.cooldown_secs);
        let key = n.app_name.to_lowercase();
        if let Some(window) = self.windows.get_mut(&key) {
            if !is_expired(window.opened, cooldown, now) {
                window.suppressed += 1;
                return Decision::Count;
            }
            if let Some(w) = self.windows.remove(&key) {
                if let Some(text) = announcement(&w) {
                    self.pending.push((w.display_name, text));
                }
            }
        }
        self.windows.insert(
            key,
            Window {
                opened: now,
                suppressed: 0,
                display_name: n.app_name.clone(),
            },
        );
        Decision::Speak(text)
    }

    /// The coalesced "N more notifications" announcements due to be spoken
    /// as of `now`: anything `decide` already retired into `pending` since
    /// the last call, plus every window whose cooldown has newly elapsed.
    ///
    /// Every window whose cooldown has elapsed is removed here whether or
    /// not it produced a line -- a window that saw no follow-up traffic
    /// (`suppressed == 0`) is retired in silence rather than announced as
    /// "0 more notifications", which would be worse than nothing.
    ///
    /// A window is also dropped here, unannounced, if its application is no
    /// longer on `cfg.allow`: §6 says a config change to `allow` takes
    /// effect on the next notification, and this follow-up is spoken after
    /// that change, not before it, so the user who just removed the
    /// application must not hear one more thing from it. `decide` never
    /// lets an unallowed application open a window in the first place, so
    /// this only ever fires for a window that was allowed when it opened
    /// and had its permission revoked while it was still counting.
    ///
    /// MINOR 1: that rule applies to both halves, and used to apply only to
    /// the second. A follow-up `decide` had already retired into `pending`
    /// -- a window that closed in the up-to-one-second gap before this tick
    /// -- went straight out unfiltered, so whether a de-allowlisted
    /// application got the last word depended on which of the two noticed
    /// its window closing first.
    pub fn due(&mut self, cfg: &NotificationConfig, now: Instant) -> Vec<String> {
        let cooldown = Duration::from_secs(cfg.cooldown_secs);
        let mut expired: Vec<String> = self
            .windows
            .iter()
            .filter(|(_, w)| is_expired(w.opened, cooldown, now))
            .map(|(key, _)| key.clone())
            .collect();
        // Deterministic output: two windows closing on the same tick must
        // not depend on hash-map iteration order.
        expired.sort();

        let mut out: Vec<String> = std::mem::take(&mut self.pending)
            .into_iter()
            .filter(|(display_name, _)| is_allowed(display_name, cfg))
            .map(|(_, text)| text)
            .collect();
        out.extend(expired.into_iter().filter_map(|key| {
            let w = self.windows.remove(&key)?;
            if !is_allowed(&w.display_name, cfg) {
                return None;
            }
            announcement(&w)
        }));
        out
    }
}

/// Is `app_name` on the config's allowlist? Compared case-insensitively,
/// like the map `Limiter` keys its windows by: an application does not get
/// to lose its rate limit, or its place on the allowlist, by capitalising
/// its name differently between two calls.
fn is_allowed(app_name: &str, cfg: &NotificationConfig) -> bool {
    let name = app_name.to_lowercase();
    cfg.allow.iter().any(|a| a.to_lowercase() == name)
}

/// The announcement for one notification, or `None` when there is nothing to
/// say.
///
/// The summary is spoken whenever present; the body is spoken only when
/// `speak_body` asks for it, and it is `strip_markup`ped first -- bodies may
/// carry the freedesktop spec's small set of HTML-like tags, which
/// `strip_markdown` in the cleanup pipeline does not touch because it is a
/// markdown filter, not an HTML one. Some applications put everything in the
/// body and leave the summary empty; when that happens the body stands in
/// for it rather than being glued onto nothing.
pub fn compose(n: &Notification, cfg: &NotificationConfig) -> Option<String> {
    let summary = n.summary.trim();
    let body = if cfg.speak_body {
        let stripped = strip_markup(&n.body);
        (!stripped.trim().is_empty()).then(|| stripped.trim().to_string())
    } else {
        None
    };

    let text = match (summary.is_empty(), body) {
        (false, Some(body)) => {
            // A summary that already ends in sentence punctuation must not
            // gain a second full stop -- "Alice replied!" followed by ". See
            // you at five" would read as two sentences stuck together with
            // a stray period.
            if ends_with_sentence_punctuation(summary) {
                format!("{summary} {body}")
            } else {
                format!("{summary}. {body}")
            }
        }
        (false, None) => summary.to_string(),
        (true, Some(body)) => body,
        // Nothing to say means nothing is spoken -- never a bare app name.
        (true, None) => return None,
    };

    Some(match cfg.speak_app_name {
        // An allowlist entry (or a `Notify` call) can hand back an empty or
        // padded name -- the M5 settings window's editable list makes a
        // stray blank row easy to produce. A blank prefix reads as a lone
        // leading colon ("`: hi`"), which is worse than no prefix, so a
        // name that is empty once trimmed is treated the same as
        // `speak_app_name = false` rather than spoken.
        true if !n.app_name.trim().is_empty() => format!("{}: {text}", n.app_name.trim()),
        _ => text,
    })
}

/// Sentence-ending punctuation that means a summary already reads as a
/// complete sentence, so joining a body onto it needs no separating full
/// stop of its own.
///
/// A trailing closing quote or bracket is skipped first: `She said "hi."`
/// ends the sentence at the period, not at the quote mark that happens to
/// come after it.
fn ends_with_sentence_punctuation(s: &str) -> bool {
    const ENDERS: [char; 6] = ['.', '!', '?', '…', ':', ';'];
    let s = s.trim_end_matches(['"', '\'', '”', '’', ')', ']', '}']);
    s.ends_with(ENDERS)
}

/// The five named entities the freedesktop notification spec expects a body
/// to use in place of the literal characters.
const ENTITIES: [(&str, char); 5] = [
    ("&amp;", '&'),
    ("&lt;", '<'),
    ("&gt;", '>'),
    ("&apos;", '\''),
    ("&quot;", '"'),
];

/// Strip the notification spec's small set of HTML-like tags out of a body
/// and decode its named entities, so "<b>Alice</b> replied" is heard as
/// "Alice replied" rather than "b Alice b replied".
///
/// Tags are stripped first, entities decoded second, so an entity spelling
/// out a literal angle bracket (`&lt;`) can never be mistaken for the start
/// or end of a tag -- only a real `<`/`>` character does that.
pub fn strip_markup(s: &str) -> String {
    decode_entities(&strip_tags(s))
}

/// Remove every `<...>` span. The one subtlety: a `<` with no `>` anywhere
/// after it is not a tag, because nothing closes it -- it is literal text
/// ("a < b and c" is a comparison, not markup), and swallowing everything
/// from it to the end of the string would turn a notification about "5 < 6"
/// into silence.
///
/// Known gap, not fixed: a `>` inside a quoted attribute value ends the tag
/// early, e.g. `<a href="a > b">link</a>` strips to `` b">link"`` instead of
/// `link`. A real HTML parser would track quote state to avoid this; this
/// function does not, because the freedesktop notification spec requires
/// senders to escape `>` as `&gt;` inside attribute values in the first
/// place -- reaching this needs a sender that already violates the spec it
/// is sending under, which is not worth a parser for.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let after_lt = &rest[lt + '<'.len_utf8()..];
        match after_lt.find('>') {
            Some(gt) => rest = &after_lt[gt + '>'.len_utf8()..],
            None => {
                out.push('<');
                rest = after_lt;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Replace each of the five named entities with its character. A bare `&`
/// that starts none of them is left alone rather than treated as an error --
/// a body is free-form text, not validated markup.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        if rest.starts_with('&') {
            if let Some((pat, ch)) = ENTITIES.iter().find(|(pat, _)| rest.starts_with(pat)) {
                out.push(*ch);
                rest = &rest[pat.len()..];
                continue;
            }
        }
        let mut chars = rest.chars();
        out.push(chars.next().expect("rest is non-empty"));
        rest = chars.as_str();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(app: &str, summary: &str, body: &str) -> Notification {
        Notification {
            app_name: app.into(),
            app_icon: String::new(),
            summary: summary.into(),
            body: body.into(),
        }
    }

    fn cfg() -> NotificationConfig {
        NotificationConfig {
            enabled: true,
            allow: vec!["Signal".into()],
            ..NotificationConfig::default()
        }
    }

    /// All four combinations of the two switches, which is the whole of the
    /// spec's announcement table.
    #[test]
    fn the_two_switches_compose_the_four_announcements() {
        let note = n("Signal", "Alice sent a message", "See you at five");

        let mut c = cfg();
        c.speak_app_name = true;
        c.speak_body = false;
        assert_eq!(
            compose(&note, &c).as_deref(),
            Some("Signal: Alice sent a message")
        );

        c.speak_app_name = false;
        assert_eq!(compose(&note, &c).as_deref(), Some("Alice sent a message"));

        c.speak_app_name = true;
        c.speak_body = true;
        assert_eq!(
            compose(&note, &c).as_deref(),
            Some("Signal: Alice sent a message. See you at five")
        );

        c.speak_app_name = false;
        assert_eq!(
            compose(&note, &c).as_deref(),
            Some("Alice sent a message. See you at five")
        );
    }

    /// A summary that already ends in sentence punctuation must not gain a
    /// second full stop -- across the whole set of enders, not just `!`,
    /// and past a trailing closing quote.
    #[test]
    fn a_punctuated_summary_is_not_given_another_full_stop() {
        let mut c = cfg();
        c.speak_body = true;
        for summary in [
            "Alice replied!",
            "Did you see this?",
            "Alice said:",
            "Typing…",
            "Backup complete;",
            "She said \"hi.\"",
        ] {
            let note = n("Signal", summary, "See you at five");
            assert_eq!(
                compose(&note, &c).as_deref(),
                Some(format!("Signal: {summary} See you at five").as_str()),
                "summary: {summary:?}"
            );
        }
    }

    /// Nothing to say means nothing is spoken -- never a bare app name.
    #[test]
    fn an_empty_notification_produces_no_announcement() {
        let c = cfg();
        assert_eq!(compose(&n("Signal", "", ""), &c), None);
        assert_eq!(compose(&n("Signal", "   ", ""), &c), None);

        let mut with_body = cfg();
        with_body.speak_body = true;
        assert_eq!(compose(&n("Signal", "", ""), &with_body), None);
        // Some applications put everything in the body.
        assert_eq!(
            compose(&n("Signal", "", "Alice sent a message"), &with_body).as_deref(),
            Some("Signal: Alice sent a message")
        );
    }

    /// Bodies may carry the freedesktop spec's small set of HTML-like tags,
    /// which `strip_markdown` in the cleanup pipeline does not touch because
    /// it is a markdown filter. Unstripped, a notification reads out "b Alice
    /// b".
    #[test]
    fn body_markup_is_stripped_and_entities_decoded() {
        assert_eq!(strip_markup("<b>Alice</b> replied"), "Alice replied");
        assert_eq!(strip_markup("<i>psst</i>"), "psst");
        assert_eq!(
            strip_markup("<a href=\"http://example.com\">a link</a>"),
            "a link"
        );
        assert_eq!(strip_markup("Tom &amp; Jerry"), "Tom & Jerry");
        assert_eq!(strip_markup("5 &lt; 6 &gt; 4"), "5 < 6 > 4");
        assert_eq!(strip_markup("it&apos;s &quot;fine&quot;"), "it's \"fine\"");
        // An unclosed tag must not eat the rest of the text.
        assert_eq!(strip_markup("a < b and c"), "a < b and c");
    }

    /// A fresh `Limiter` per call: `signal` and `SIGNAL` are the same
    /// application as far as the rate limiter is concerned (its windows are
    /// keyed case-insensitively too, see `Window::display_name`'s doc
    /// comment), so sharing one `Limiter` across all three assertions would
    /// make the second call `Count` against the first's just-opened window
    /// instead of `Speak` -- a rate-limiting effect this test is not about.
    /// What it is about is covered on its own further down (`a_burst_speaks_
    /// once_then_counts`).
    #[test]
    fn only_allowed_applications_are_spoken_and_case_does_not_matter() {
        let c = cfg(); // allows "Signal"
        let t = Instant::now();
        assert!(matches!(
            Limiter::new().decide(&n("signal", "hi", ""), &c, t),
            Decision::Speak(_)
        ));
        assert!(matches!(
            Limiter::new().decide(&n("SIGNAL", "hi", ""), &c, t),
            Decision::Speak(_)
        ));
        assert!(matches!(
            Limiter::new().decide(&n("Fractal", "hi", ""), &c, t),
            Decision::NotAllowed
        ));
    }

    /// The map that keys windows is case-insensitive, not just the
    /// allowlist check in front of it: a burst split across "signal" and
    /// "SIGNAL" spellings is one application's worth of rate limiting, not
    /// two. (Measured: keying on `n.app_name` verbatim instead of its
    /// lowercased form still passes every other test in this file, because
    /// none of them mixes case within one `Limiter` -- this is the only one
    /// that would catch it.) It also pins the display-name rule at the same
    /// time: the follow-up echoes back "signal", the spelling that opened
    /// the window, not "SIGNAL".
    #[test]
    fn differently_cased_names_share_one_window() {
        let c = cfg(); // allows "Signal"
        let mut l = Limiter::new();
        let t0 = Instant::now();
        assert!(matches!(
            l.decide(&n("signal", "first", ""), &c, t0),
            Decision::Speak(_)
        ));
        assert!(matches!(
            l.decide(&n("SIGNAL", "second", ""), &c, t0 + Duration::from_secs(1)),
            Decision::Count
        ));
        assert_eq!(
            l.due(&c, t0 + Duration::from_secs(31)),
            vec!["signal: 1 more notification".to_string()]
        );
    }

    /// The first notification speaks; the rest of the window counts. This is
    /// the whole point of the limiter: a busy channel costs one short
    /// utterance per window instead of talking continuously.
    #[test]
    fn a_burst_speaks_once_then_counts() {
        let c = cfg(); // 30s window
        let mut l = Limiter::new();
        let t0 = Instant::now();
        assert!(matches!(
            l.decide(&n("Signal", "first", ""), &c, t0),
            Decision::Speak(_)
        ));
        for i in 0..3 {
            let t = t0 + Duration::from_secs(1 + i);
            assert!(matches!(
                l.decide(&n("Signal", "more", ""), &c, t),
                Decision::Count
            ));
        }
        // Nothing is due until the window closes.
        assert!(l.due(&c, t0 + Duration::from_secs(29)).is_empty());
        assert_eq!(
            l.due(&c, t0 + Duration::from_secs(31)),
            vec!["Signal: 3 more notifications".to_string()]
        );
        // ...and only once.
        assert!(l.due(&c, t0 + Duration::from_secs(32)).is_empty());
    }

    /// One notification in a window is spoken and produces no follow-up: a
    /// bare "0 more notifications" would be worse than silence.
    #[test]
    fn a_quiet_window_produces_no_followup() {
        let c = cfg();
        let mut l = Limiter::new();
        let t0 = Instant::now();
        assert!(matches!(
            l.decide(&n("Signal", "only", ""), &c, t0),
            Decision::Speak(_)
        ));
        assert!(l.due(&c, t0 + Duration::from_secs(31)).is_empty());
    }

    /// Singular reads as a sentence, not as a template with a number in it.
    #[test]
    fn one_coalesced_notification_reads_singular() {
        let c = cfg();
        let mut l = Limiter::new();
        let t0 = Instant::now();
        let _ = l.decide(&n("Signal", "first", ""), &c, t0);
        let _ = l.decide(&n("Signal", "second", ""), &c, t0 + Duration::from_secs(1));
        assert_eq!(
            l.due(&c, t0 + Duration::from_secs(31)),
            vec!["Signal: 1 more notification".to_string()]
        );
    }

    /// Windows are per application: a noisy chat must not delay a build
    /// finishing.
    #[test]
    fn windows_are_per_application() {
        let mut c = cfg();
        c.allow = vec!["Signal".into(), "Builds".into()];
        let mut l = Limiter::new();
        let t0 = Instant::now();
        assert!(matches!(
            l.decide(&n("Signal", "chatter", ""), &c, t0),
            Decision::Speak(_)
        ));
        assert!(matches!(
            l.decide(
                &n("Builds", "build finished", ""),
                &c,
                t0 + Duration::from_secs(1)
            ),
            Decision::Speak(_)
        ));
    }

    /// After a window closes, the next notification speaks immediately again.
    #[test]
    fn the_window_reopens_after_it_closes() {
        let c = cfg();
        let mut l = Limiter::new();
        let t0 = Instant::now();
        let _ = l.decide(&n("Signal", "first", ""), &c, t0);
        let later = t0 + Duration::from_secs(31);
        let _ = l.due(&c, later);
        assert!(matches!(
            l.decide(&n("Signal", "next", ""), &c, later),
            Decision::Speak(_)
        ));
    }

    /// `cooldown_secs = 0` disables rate limiting rather than dividing by a
    /// zero-length window: every notification speaks.
    #[test]
    fn a_zero_cooldown_speaks_everything() {
        let mut c = cfg();
        c.cooldown_secs = 0;
        let mut l = Limiter::new();
        let t0 = Instant::now();
        assert!(matches!(
            l.decide(&n("Signal", "a", ""), &c, t0),
            Decision::Speak(_)
        ));
        assert!(matches!(
            l.decide(&n("Signal", "b", ""), &c, t0),
            Decision::Speak(_)
        ));
    }

    /// A notification that composes to nothing must not open a window or be
    /// counted -- it was never going to be spoken. And it is `NothingToSay`,
    /// not `NotAllowed`: "Signal" is on the allowlist, it just had nothing
    /// to say this time, and Task 4's discovery log must not log it.
    #[test]
    fn an_empty_notification_is_ignored_not_counted() {
        let c = cfg();
        let mut l = Limiter::new();
        let t0 = Instant::now();
        assert!(matches!(
            l.decide(&n("Signal", "", ""), &c, t0),
            Decision::NothingToSay
        ));
        assert!(matches!(
            l.decide(&n("Signal", "real", ""), &c, t0),
            Decision::Speak(_)
        ));
    }

    /// CRITICAL: reopening an expired window inside `decide` must not throw
    /// away what it had counted. Before this was fixed, `decide` overwrote
    /// the closed window with a fresh one in place, and its 3 suppressed
    /// notifications simply vanished -- no `due` call ever got a chance to
    /// announce them, because there was nothing left in the map to find.
    /// This test calls `decide` again on an expired window *without* an
    /// intervening `due` -- unlike `the_window_reopens_after_it_closes`
    /// above, which calls `due` first and so never exercises this path.
    /// Task 4 ticks `due` on a one-second timer, so any notification that
    /// arrives in the up-to-one-second gap after a window's cooldown elapses
    /// hits exactly this.
    #[test]
    fn reopening_a_window_without_an_intervening_due_still_announces_it() {
        let c = cfg(); // 30s window
        let mut l = Limiter::new();
        let t0 = Instant::now();
        assert!(matches!(
            l.decide(&n("Signal", "first", ""), &c, t0),
            Decision::Speak(_)
        ));
        for i in 0..3 {
            let t = t0 + Duration::from_secs(1 + i);
            assert!(matches!(
                l.decide(&n("Signal", "more", ""), &c, t),
                Decision::Count
            ));
        }
        // The window has closed; nothing has called `due` yet. `decide`
        // itself notices the expiry, retires the old window's count, and
        // opens a fresh one.
        let reopened = t0 + Duration::from_secs(31);
        assert!(matches!(
            l.decide(&n("Signal", "next", ""), &c, reopened),
            Decision::Speak(_)
        ));
        // The 3 notifications from the closed window are still owed --
        // `due` hands them back even though it never saw that window expire
        // itself.
        assert_eq!(
            l.due(&c, reopened),
            vec!["Signal: 3 more notifications".to_string()]
        );
    }

    /// An application removed from `allow` after its window opened must not
    /// be heard from again: §6 says an `allow` change takes effect on the
    /// next notification, and the follow-up is spoken strictly after that
    /// change, so by the time `due` fires the user has already un-named this
    /// application. The window is still dropped either way -- it must not
    /// linger and leak a follow-up later if the application is re-added.
    #[test]
    fn due_does_not_announce_an_application_removed_from_allow() {
        let mut c = cfg(); // allows "Signal"
        let mut l = Limiter::new();
        let t0 = Instant::now();
        let _ = l.decide(&n("Signal", "first", ""), &c, t0);
        let _ = l.decide(&n("Signal", "more", ""), &c, t0 + Duration::from_secs(1));

        c.allow.clear();
        assert!(l.due(&c, t0 + Duration::from_secs(31)).is_empty());

        // And it does not linger: re-allowing the application does not
        // resurrect the old window's count on a later tick.
        c.allow = vec!["Signal".into()];
        assert!(l.due(&c, t0 + Duration::from_secs(32)).is_empty());
    }

    /// MINOR 1: the same rule as `due_does_not_announce_an_application_
    /// removed_from_allow` above, for the follow-up that took the *other*
    /// route into `due` -- queued by `decide` when it found the window
    /// already expired and reopened it, rather than found expired by `due`
    /// itself. Whether an application removed from `allow` gets one last
    /// word must not depend on which of the two noticed the closed window
    /// first; before this, `due` returned `pending` verbatim and it did.
    ///
    /// The `decide` that retires the window here happens *before* the
    /// allowlist change, deliberately: that is what puts the follow-up in
    /// `pending` in the first place (an application removed from `allow`
    /// cannot `decide` at all -- it returns `NotAllowed` before it ever
    /// reaches a window).
    #[test]
    fn due_does_not_announce_a_pending_followup_from_a_de_allowlisted_application() {
        let mut c = cfg(); // allows "Signal", 30s window
        let mut l = Limiter::new();
        let t0 = Instant::now();
        let _ = l.decide(&n("Signal", "first", ""), &c, t0);
        for i in 0..3 {
            let _ = l.decide(
                &n("Signal", "more", ""),
                &c,
                t0 + Duration::from_secs(1 + i),
            );
        }
        // The window has closed and nothing has called `due` yet: this
        // `decide` retires the old window into `pending` and opens a fresh
        // one.
        let reopened = t0 + Duration::from_secs(31);
        assert!(matches!(
            l.decide(&n("Signal", "next", ""), &c, reopened),
            Decision::Speak(_)
        ));

        // The user removes the application before the next tick.
        c.allow.clear();
        assert!(
            l.due(&c, reopened).is_empty(),
            "a follow-up queued by `decide` must be dropped when its \
             application has left the allowlist, exactly like one `due` \
             retires itself"
        );

        // And it is gone rather than merely withheld: re-allowing the
        // application does not resurrect it on a later tick.
        c.allow = vec!["Signal".into()];
        assert!(l.due(&c, reopened + Duration::from_secs(1)).is_empty());
    }

    /// A cooldown large enough to overflow `Instant` arithmetic must not
    /// panic the daemon -- `cooldown_secs` is hot-reloaded config, so a
    /// fat-fingered value here is one bad TOML edit away, not a programming
    /// error. Such a window simply never closes.
    #[test]
    fn an_overflowing_cooldown_does_not_panic() {
        let mut c = cfg();
        c.cooldown_secs = u64::MAX;
        let mut l = Limiter::new();
        let t0 = Instant::now();
        assert!(matches!(
            l.decide(&n("Signal", "first", ""), &c, t0),
            Decision::Speak(_)
        ));
        assert!(matches!(
            l.decide(&n("Signal", "more", ""), &c, t0 + Duration::from_secs(1)),
            Decision::Count
        ));
        assert!(l.due(&c, t0 + Duration::from_secs(1_000_000)).is_empty());
    }

    /// An empty or whitespace-only app name must not be spoken as a bare
    /// leading colon, and a padded one must not carry its padding into the
    /// announcement. Both are reachable from the M5 settings window's
    /// editable allowlist, which does not stop the user typing either.
    #[test]
    fn compose_trims_and_skips_an_empty_app_name() {
        let c = cfg();
        assert_eq!(
            compose(&n("", "hi", ""), &c).as_deref(),
            Some("hi"),
            "empty app name must not produce a leading colon"
        );
        assert_eq!(
            compose(&n("   ", "hi", ""), &c).as_deref(),
            Some("hi"),
            "whitespace-only app name must not produce a leading colon"
        );
        assert_eq!(
            compose(&n("Signal ", "hi", ""), &c).as_deref(),
            Some("Signal: hi"),
            "a padded app name must not carry its padding into the prefix"
        );
    }
}
