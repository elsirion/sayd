//! Whether a notification is spoken, and what it says.
//!
//! Everything here is pure and takes its clock as a parameter. That is not
//! tidiness: the rate-limit window, the counting and the coalesced follow-up
//! are the only real state in this milestone, and a test that had to sleep
//! for a 30-second window would not be written.

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
    /// Not ours -- not allowed, or nothing to say.
    Ignore,
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
}

struct Window {
    opened: Instant,
    suppressed: u32,
    /// The name as the application spells it, for the follow-up announcement:
    /// the map is keyed by a lowercased name so matching is case-insensitive,
    /// but "SIGNAL: 3 more notifications" is not what the user wants to hear.
    display_name: String,
}

impl Limiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide what happens to one notification, as of `now`.
    ///
    /// In order: not on the allowlist, or nothing to say once composed, is
    /// `Ignore` -- neither opens a window, because there was never going to
    /// be anything to speak for it. `cooldown_secs == 0` means rate
    /// limiting is off, not a zero-length window that nothing could ever
    /// land inside: everything speaks. Otherwise, a still-open window from
    /// this application counts against it; a closed or absent one opens
    /// (or reopens) and speaks.
    pub fn decide(&mut self, n: &Notification, cfg: &NotificationConfig, now: Instant) -> Decision {
        if !is_allowed(&n.app_name, cfg) {
            return Decision::Ignore;
        }
        let Some(text) = compose(n, cfg) else {
            return Decision::Ignore;
        };
        if cfg.cooldown_secs == 0 {
            return Decision::Speak(text);
        }

        let cooldown = Duration::from_secs(cfg.cooldown_secs);
        let key = n.app_name.to_lowercase();
        if let Some(window) = self.windows.get_mut(&key) {
            if now < window.opened + cooldown {
                window.suppressed += 1;
                return Decision::Count;
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

    /// The coalesced "N more notifications" announcements whose windows have
    /// closed as of `now`.
    ///
    /// Every window whose cooldown has elapsed is removed here whether or
    /// not it produced a line -- a window that saw no follow-up traffic
    /// (`suppressed == 0`) is retired in silence rather than announced as
    /// "0 more notifications", which would be worse than nothing.
    pub fn due(&mut self, cfg: &NotificationConfig, now: Instant) -> Vec<String> {
        let cooldown = Duration::from_secs(cfg.cooldown_secs);
        let mut expired: Vec<String> = self
            .windows
            .iter()
            .filter(|(_, w)| now >= w.opened + cooldown)
            .map(|(key, _)| key.clone())
            .collect();
        // Deterministic output: two windows closing on the same tick must
        // not depend on hash-map iteration order.
        expired.sort();

        expired
            .into_iter()
            .filter_map(|key| {
                let w = self.windows.remove(&key)?;
                (w.suppressed > 0).then(|| {
                    let noun = if w.suppressed == 1 {
                        "notification"
                    } else {
                        "notifications"
                    };
                    format!("{}: {} more {noun}", w.display_name, w.suppressed)
                })
            })
            .collect()
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
            if summary.ends_with(['.', '!', '?']) {
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

    Some(if cfg.speak_app_name {
        format!("{}: {text}", n.app_name)
    } else {
        text
    })
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
    /// second full stop.
    #[test]
    fn a_punctuated_summary_is_not_given_another_full_stop() {
        let mut c = cfg();
        c.speak_body = true;
        let note = n("Signal", "Alice replied!", "See you at five");
        assert_eq!(
            compose(&note, &c).as_deref(),
            Some("Signal: Alice replied! See you at five")
        );
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
            Decision::Ignore
        ));
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
    /// counted -- it was never going to be spoken.
    #[test]
    fn an_empty_notification_is_ignored_not_counted() {
        let c = cfg();
        let mut l = Limiter::new();
        let t0 = Instant::now();
        assert!(matches!(
            l.decide(&n("Signal", "", ""), &c, t0),
            Decision::Ignore
        ));
        assert!(matches!(
            l.decide(&n("Signal", "real", ""), &c, t0),
            Decision::Speak(_)
        ));
    }
}
