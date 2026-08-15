//! Which applications have notified this run, and what icon they last used.
//!
//! This exists for the settings window (a later task, not this one): when a
//! user opens the notification allowlist editor, the most useful suggestions
//! are not a curated static list but the applications `sayd` has actually
//! watched notify, each shown with its own icon rather than a placeholder.
//! `monitor` already discards that pairing the moment `decide` runs; this
//! module is the one place that keeps it.
//!
//! Process-global rather than threaded through `run`'s state, on purpose:
//! the settings window lives on the glib main thread, `monitor::run` lives
//! on a tokio task, and the two do not otherwise share a value to hang this
//! off -- plumbing one through `EngineHandle` or a new channel would be a
//! second copy of exactly the kind of cross-thread bookkeeping
//! `config_watch::ConfigStore` already does with a `Mutex`, for a fraction
//! of its state. `record` is called from tokio threads, `snapshot` from the
//! glib main thread; `Mutex<VecDeque<SeenApp>>` behind a `static` is
//! `Send + Sync` by construction, which is all either side needs. Poison-
//! tolerant like `ConfigStatus::slot` (`config_watch.rs`): this is
//! bookkeeping for a UI hint, not the engine's own state, so a panic while
//! the lock was held (there should be none -- see below) must not turn
//! every later `record` or `snapshot` into a panic of its own for the rest
//! of the process.
//!
//! Nothing under the lock may ever be slow: no I/O, nothing async, not even
//! an allocation-heavy operation beyond the small string clones the shape
//! below already needs. This daemon has already had two things this
//! milestone go wrong from a lock held across something that turned out not
//! to be instant; this registry has no excuse to be the third, since
//! `record` and `snapshot` are pure in-memory bookkeeping from end to end.
//!
//! `VecDeque` over a plain `Vec`: the operations this module needs are
//! front-insert (a fresh sighting), move-to-front (a repeat sighting), and
//! truncate-from-the-back (the cap dropping the oldest). `VecDeque::
//! push_front` is O(1) amortized where `Vec::insert(0, ..)` would shift
//! every other element down on every single call; `truncate` drops from the
//! back on both, so the cap falls out the same way either type. At 64
//! entries the constant-factor difference is not the point -- `VecDeque` is
//! simply the type whose name says "front and back both matter here"
//! instead of leaving that as a comment on a `Vec`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use super::{truncate_chars, Notification, MAX_APP_NAME_LEN};

/// One application `sayd` has watched call `Notify`, and the icon strings it
/// most recently carried.
///
/// All three icon fields, in the order `settings::model` prefers them, for
/// the reason [`Notification`] spells out: `app_icon` is empty for
/// essentially every real sender, and the icon is in the `desktop-entry` or
/// `image-path` hint instead. Each is stored exactly as the application sent
/// it, trimmed and length-bounded and nothing else -- what a given string
/// *means* (a theme name, a path, a `file://` URI) is
/// `settings::model::icon_source`'s question, not this registry's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenApp {
    pub app_name: String,
    pub desktop_entry: String,
    pub image_path: String,
    pub app_icon: String,
}

/// Hard cap on how many distinct applications this registry remembers.
///
/// `app_name` is chosen by the calling application, not validated by `sayd`
/// (see `decode::Notification`'s doc comment), so nothing stops a
/// compromised application, or a `notify-send -a "$counter"` loop, from
/// minting a fresh name on every call -- the same hazard `monitor::
/// MAX_ANNOUNCED` exists for. The number is smaller than that cap's 256
/// because this is not a log a user reads once and forgets; it is a list the
/// settings window renders on every open, so it stays short enough to be a
/// suggestion list rather than a second scrollbar.
pub const MAX_SEEN: usize = 64;

/// Longest icon string this registry keeps, per field.
///
/// The count cap alone bounds nothing: an icon string is as
/// application-controlled as the name is, and the spec gives it no length
/// limit either (CRITICAL 2 is the same hazard on the other field).
/// Generous rather than tight, because unlike `app_name` these strings are
/// never rendered -- they are handed to an icon theme or `stat`ted -- so the
/// only cost of a long one is the bytes, and truncating a legitimate deep
/// `XDG_DATA_DIRS` path would break an icon that would otherwise have
/// resolved. 4096 is `PATH_MAX`, past which no path can name a real file
/// anyway; the worst case for the whole registry is then under a megabyte
/// instead of unbounded.
pub const MAX_ICON_LEN: usize = 4096;

/// Most-recent-first, capped at [`MAX_SEEN`].
static SEEN: Mutex<VecDeque<SeenApp>> = Mutex::new(VecDeque::new());

/// Bumped whenever [`record`] actually changes what [`snapshot`] would
/// return.
///
/// IMPORTANT 7: every `Ui::redraw` call site is config-driven, so an
/// application that notified while the settings window sat open never
/// appeared under "Seen notifying" until the window was closed and
/// reopened -- which is exactly the discovery loop the suggestions exist to
/// close. The window polls this instead of the registry itself: an atomic
/// load is cheap enough to do on a timer, where cloning the whole snapshot
/// once a second to compare it would not be, and it takes no lock the
/// recording side could be holding.
///
/// Not bumped when a repeat sighting changes nothing (the same application,
/// already at the front, with the same icons), which is what a chatty
/// allowlisted application produces: a counter that moved on every
/// notification would have the window rebuilding rows it had just built,
/// once per tick, for as long as the chatter lasted.
///
/// `Relaxed` on both sides: a UI hint that arrives a tick late is a UI hint
/// that arrives a tick late. What ordering there is comes from the mutex --
/// the bump happens while the registry lock is held, so a poller that sees
/// a new value and then locks sees at least the state that produced it.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The lock, tolerant of a poisoned mutex.
///
/// A panic while this lock is held should not happen -- see the module
/// doc's "nothing slow" rule -- but if it ever did, poisoning it would take
/// down every later `record` and `snapshot` call for the rest of the
/// process, over a UI suggestion list. That is a worse failure than reading
/// whatever the interrupted write left behind, the same trade `config_watch
/// ::ConfigStatus::slot` makes for the same reason.
fn lock() -> MutexGuard<'static, VecDeque<SeenApp>> {
    match SEEN.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Record that `n`'s application notified, with the icon strings it carried.
///
/// Idempotent per name, case-insensitively **and** ignoring surrounding
/// whitespace: matched the same way `allow` is (`policy::is_allowed`), so an
/// application that spells itself "Signal" today and "signal" tomorrow is
/// one suggestion, not two, and one the window can correctly recognise as
/// already-allowed however either side happened to capitalise it. The trim
/// is IMPORTANT 4's half of that: the bus accepts an `app_name` of
/// `"Signal "`, and a registry keyed on the untrimmed name held it as a
/// second entry that then won the single suggestion slot from the entry the
/// settings window (which trims) had already deduplicated it against.
///
/// A repeat sighting moves the entry to the front and refreshes its icons --
/// a theme change or an update means the old icon name is stale, and the app
/// a user is trying to allow right now is almost always the one that just
/// notified them, so it belongs at the top of the suggestion list.
///
/// Two bounds, both on what one entry can cost rather than how many there
/// are (CRITICAL 2). `MAX_SEEN` capped the count and nothing capped the
/// size, which left the whole registry unbounded: `app_name` is truncated to
/// [`MAX_APP_NAME_LEN`] like `monitor`'s discovery log does, and each icon
/// string to [`MAX_ICON_LEN`].
///
/// A name that is empty once trimmed records nothing at all (IMPORTANT 5).
/// `notify-send -a ""` is accepted by the bus, and an empty name is on no
/// list `allow_contains` can answer for, so it was suggested as a blank row
/// whose Add button did nothing (`allow_add` no-ops on empty) and whose
/// redraw rebuilt it identically -- a row no click could clear.
///
/// Called for every decoded notification, not only ones the allowlist
/// declines (Important, matching `monitor::run`'s call site): an already-
/// allowed application still notifies, still may change its icon, and the
/// settings window is the one place responsible for filtering out what is
/// already allowed -- this registry does not need to know or care.
pub fn record(n: &Notification) {
    // Built before the lock is taken, not under it: the module doc's
    // "nothing slow under the lock" rule, and these are the only
    // allocations on this path.
    let app_name = truncate_chars(n.app_name.trim(), MAX_APP_NAME_LEN);
    if app_name.is_empty() {
        return;
    }
    let fresh = SeenApp {
        app_name: app_name.to_string(),
        desktop_entry: bounded_icon(&n.desktop_entry),
        image_path: bounded_icon(&n.image_path),
        app_icon: bounded_icon(&n.app_icon),
    };
    let key = app_name.to_lowercase();

    let mut seen = lock();
    if let Some(idx) = seen.iter().position(|a| a.app_name.to_lowercase() == key) {
        // Already at the front, unchanged: a chatty application repeating
        // itself. Nothing to move and nothing to refresh, so the generation
        // must not move either -- see [`GENERATION`].
        if idx == 0 && seen[0] == fresh {
            return;
        }
        seen.remove(idx);
    }
    seen.push_front(fresh);
    // Drops from the back -- the oldest entries -- exactly like `Vec::
    // truncate` would; `VecDeque` just gives that operation a name that
    // matches what push_front is doing at the other end.
    seen.truncate(MAX_SEEN);
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Trim and bound one icon string. See [`MAX_ICON_LEN`].
fn bounded_icon(icon: &str) -> String {
    truncate_chars(icon.trim(), MAX_ICON_LEN).to_string()
}

/// Every application recorded so far this run, most recent first.
pub fn snapshot() -> Vec<SeenApp> {
    lock().iter().cloned().collect()
}

/// A value that changes exactly when [`snapshot`] would return something
/// different. See [`GENERATION`].
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A notification from `app_name` carrying `app_icon` and nothing else,
    /// the shape every test here that does not care about the hints wants.
    fn notified(app_name: &str, app_icon: &str) -> Notification {
        Notification {
            app_name: app_name.to_string(),
            desktop_entry: String::new(),
            image_path: String::new(),
            app_icon: app_icon.to_string(),
            summary: "s".into(),
            body: "b".into(),
        }
    }

    /// The registry is process-global, so these tests share it. Each uses
    /// its own distinct app names rather than clearing between runs, which
    /// would race the other tests in this binary.
    #[test]
    fn a_recorded_app_appears_in_the_snapshot_with_its_icon() {
        record(&notified("t1-Signal", "signal-desktop"));
        let s = snapshot();
        let e = s
            .iter()
            .find(|a| a.app_name == "t1-Signal")
            .expect("recorded");
        assert_eq!(e.app_icon, "signal-desktop");
    }

    /// CRITICAL 1: the icon strings a real sender actually uses are the two
    /// hints, not `app_icon` -- so all three have to survive into the
    /// registry for the settings window to have anything to draw.
    #[test]
    fn the_hint_icons_are_recorded_alongside_app_icon() {
        record(&Notification {
            app_name: "t1b-Fractal".into(),
            desktop_entry: "org.gnome.Fractal".into(),
            image_path: "mail-unread".into(),
            app_icon: String::new(),
            summary: "s".into(),
            body: "b".into(),
        });
        let s = snapshot();
        let e = s
            .iter()
            .find(|a| a.app_name == "t1b-Fractal")
            .expect("recorded");
        assert_eq!(e.desktop_entry, "org.gnome.Fractal");
        assert_eq!(e.image_path, "mail-unread");
        assert_eq!(e.app_icon, "");
    }

    /// The same app notifying twice is one entry, not two -- and the
    /// allowlist matches case-insensitively, so the registry must agree or
    /// a suggestion will be offered for an app that is already allowed.
    #[test]
    fn the_same_app_in_a_different_case_is_one_entry() {
        record(&notified("t2-Fractal", "org.gnome.Fractal"));
        record(&notified("t2-FRACTAL", "org.gnome.Fractal"));
        let n = snapshot()
            .iter()
            .filter(|a| a.app_name.eq_ignore_ascii_case("t2-Fractal"))
            .count();
        assert_eq!(n, 1);
    }

    /// IMPORTANT 4: and the same app with a stray space is one entry too.
    /// The bus accepts an `app_name` of `"Signal "`, and every layer that
    /// consumes this list -- `allow_contains`, `allow_add`, `is_allowed`,
    /// `suggestions`'s dedupe -- trims before it compares. A registry that
    /// did not held both spellings, and the more recent one won the single
    /// suggestion slot the window had already deduplicated them into.
    #[test]
    fn a_name_with_surrounding_space_is_the_same_entry_trimmed() {
        record(&notified("t2b-Signal", "one"));
        record(&notified("  t2b-Signal  ", "two"));
        let s = snapshot();
        let matching: Vec<&SeenApp> = s
            .iter()
            .filter(|a| a.app_name.trim().eq_ignore_ascii_case("t2b-Signal"))
            .collect();
        assert_eq!(matching.len(), 1, "a padded name is not a second entry");
        assert_eq!(
            matching[0].app_name, "t2b-Signal",
            "the name is stored trimmed, as every consumer compares it"
        );
        assert_eq!(matching[0].app_icon, "two", "the later sighting wins");
    }

    /// An app that changes its icon (a theme change, an update) must show
    /// the current one, and re-notifying must move it ahead of apps that
    /// notified before it -- the app someone is trying to allow is almost
    /// always the one that just interrupted them.
    ///
    /// Asserted as a *relative* order, not an absolute position. The
    /// registry is process-global and `monitor`'s integration tests record
    /// into it from a real bus while this runs, so anything of the form
    /// `snapshot()[0] == ...` is a race: it passed on a warm run and failed
    /// three times out of three when the suite had to compile first, which
    /// left the dbus tests running for seconds alongside this one.
    #[test]
    fn recording_again_refreshes_the_icon_and_moves_the_entry_ahead() {
        record(&notified("t3-a", "old-icon"));
        record(&notified("t3-b", "other"));
        record(&notified("t3-a", "new-icon"));

        let s = snapshot();
        let pos = |name: &str| s.iter().position(|a| a.app_name == name);
        let a = pos("t3-a").expect("t3-a is recorded");
        let b = pos("t3-b").expect("t3-b is recorded");

        assert!(a < b, "re-recording must move an app ahead of older ones");
        assert_eq!(
            s[a].app_icon, "new-icon",
            "the icon must be refreshed, not left at what it first sent"
        );
    }

    /// The cap is what stops a hostile or buggy sender growing this without
    /// bound -- `app_name` is attacker-controlled, exactly as the discovery
    /// log's own cap exists for.
    #[test]
    fn the_registry_is_capped_and_drops_the_oldest() {
        for i in 0..(MAX_SEEN + 10) {
            record(&notified(&format!("t4-{i}"), "icon"));
        }
        let s = snapshot();
        assert!(s.len() <= MAX_SEEN, "capped at {MAX_SEEN}, got {}", s.len());
        assert!(
            s.iter()
                .any(|a| a.app_name == format!("t4-{}", MAX_SEEN + 9)),
            "the newest entry must survive"
        );
        assert!(
            !s.iter().any(|a| a.app_name == "t4-0"),
            "the oldest must have been dropped"
        );
    }

    /// CRITICAL 2: the count cap bounds nothing on its own. One `record`
    /// with a megabyte-long name held the lock for 2.9 ms, `snapshot` for
    /// 21.3 ms, and 64 such entries retained 128 MB -- before the name ever
    /// reached the Pango label that costs 171 ms to lay out. Every field
    /// one caller controls has to be bounded, not just the number of them.
    #[test]
    fn one_entry_cannot_be_arbitrarily_large() {
        let huge = "a".repeat(1_000_000);
        record(&Notification {
            app_name: format!("t5-{huge}"),
            desktop_entry: huge.clone(),
            image_path: huge.clone(),
            app_icon: huge.clone(),
            summary: "s".into(),
            body: "b".into(),
        });
        let s = snapshot();
        let e = s
            .iter()
            .find(|a| a.app_name.starts_with("t5-"))
            .expect("recorded");
        assert_eq!(
            e.app_name.chars().count(),
            MAX_APP_NAME_LEN,
            "the name must be truncated like the discovery log's is"
        );
        for icon in [&e.desktop_entry, &e.image_path, &e.app_icon] {
            assert_eq!(
                icon.chars().count(),
                MAX_ICON_LEN,
                "an icon string is as caller-controlled as the name is"
            );
        }
    }

    /// Truncation is by `char`: a name of multi-byte characters must be cut
    /// on a boundary rather than panicking, and two names sharing a
    /// truncated prefix are one entry afterwards.
    #[test]
    fn a_long_multibyte_name_is_truncated_on_a_char_boundary() {
        let long = "é".repeat(MAX_APP_NAME_LEN * 2);
        record(&notified(&format!("t6{long}"), "icon"));
        let s = snapshot();
        let e = s
            .iter()
            .find(|a| a.app_name.starts_with("t6é"))
            .expect("recorded");
        assert_eq!(e.app_name.chars().count(), MAX_APP_NAME_LEN);
    }

    /// IMPORTANT 5: `notify-send -a ""` is accepted by the bus, and an
    /// empty name is on no allowlist and cannot be added to one -- it was
    /// offered as a blank suggestion row whose Add button did nothing and
    /// whose redraw put the identical row straight back. Nothing to
    /// suggest, so nothing recorded.
    #[test]
    fn an_empty_or_blank_name_is_not_recorded() {
        record(&notified("", "icon"));
        record(&notified("   \t ", "icon"));
        assert!(
            !snapshot().iter().any(|a| a.app_name.trim().is_empty()),
            "a blank name must never become a suggestion row"
        );
    }

    /// IMPORTANT 7: the settings window polls this to notice an application
    /// that notified while it sat open. It has to move when the list
    /// changes...
    #[test]
    fn the_generation_moves_when_the_registry_changes() {
        let before = generation();
        record(&notified("t7-new", "icon"));
        assert!(
            generation() > before,
            "a fresh sighting must be visible to a poller"
        );
    }

    /// ...and it must *not* move for a repeat sighting that changes
    /// nothing, or a chatty allowlisted application would have the window
    /// rebuilding identical rows once per tick for as long as it chatted.
    ///
    /// Retried rather than asserted once, for the reason
    /// `recording_again_refreshes_the_icon_and_moves_the_entry_ahead`
    /// asserts a relative order: this registry is process-global and
    /// `monitor`'s bus-backed tests record into it while this runs, so a
    /// sighting landing between the two calls below legitimately moves this
    /// entry off the front and legitimately bumps the generation. Ten
    /// attempts to observe one uninterrupted pair is not a race; asserting
    /// on the first attempt would have been.
    #[test]
    fn the_generation_does_not_move_for_an_unchanged_repeat() {
        for attempt in 0..10 {
            record(&notified("t8-chatty", "icon"));
            let after_first = generation();
            record(&notified("t8-chatty", "icon"));
            if generation() == after_first {
                return;
            }
            assert!(attempt < 9, "an identical repeat sighting is not a change");
        }
    }
}
