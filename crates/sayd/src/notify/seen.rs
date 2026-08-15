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
use std::sync::{Mutex, MutexGuard};

/// One application `sayd` has watched call `Notify`, and the icon name it
/// most recently carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenApp {
    pub app_name: String,
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

/// Most-recent-first, capped at [`MAX_SEEN`].
static SEEN: Mutex<VecDeque<SeenApp>> = Mutex::new(VecDeque::new());

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

/// Record that `app_name` notified, carrying `app_icon`.
///
/// Idempotent per name, case-insensitively: matched the same way `allow` is
/// (`policy::is_allowed`), so an application that spells itself "Signal"
/// today and "signal" tomorrow is one suggestion, not two, and one the
/// window can correctly recognise as already-allowed however either side
/// happened to capitalise it. A repeat sighting moves the entry to the
/// front and refreshes its icon -- a theme change or an update means the
/// old icon name is stale, and the app a user is trying to allow right now
/// is almost always the one that just notified them, so it belongs at the
/// top of the suggestion list.
///
/// Called for every decoded notification, not only ones the allowlist
/// declines (Important, matching `monitor::run`'s call site): an already-
/// allowed application still notifies, still may change its icon, and the
/// settings window is the one place responsible for filtering out what is
/// already allowed -- this registry does not need to know or care.
pub fn record(app_name: &str, app_icon: &str) {
    let mut seen = lock();
    let key = app_name.to_lowercase();
    if let Some(idx) = seen.iter().position(|a| a.app_name.to_lowercase() == key) {
        seen.remove(idx);
    }
    seen.push_front(SeenApp {
        app_name: app_name.to_string(),
        app_icon: app_icon.to_string(),
    });
    // Drops from the back -- the oldest entries -- exactly like `Vec::
    // truncate` would; `VecDeque` just gives that operation a name that
    // matches what push_front is doing at the other end.
    seen.truncate(MAX_SEEN);
}

/// Every application recorded so far this run, most recent first.
pub fn snapshot() -> Vec<SeenApp> {
    lock().iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is process-global, so these tests share it. Each uses
    /// its own distinct app names rather than clearing between runs, which
    /// would race the other tests in this binary.
    #[test]
    fn a_recorded_app_appears_in_the_snapshot_with_its_icon() {
        record("t1-Signal", "signal-desktop");
        let s = snapshot();
        let e = s
            .iter()
            .find(|a| a.app_name == "t1-Signal")
            .expect("recorded");
        assert_eq!(e.app_icon, "signal-desktop");
    }

    /// The same app notifying twice is one entry, not two -- and the
    /// allowlist matches case-insensitively, so the registry must agree or
    /// a suggestion will be offered for an app that is already allowed.
    #[test]
    fn the_same_app_in_a_different_case_is_one_entry() {
        record("t2-Fractal", "org.gnome.Fractal");
        record("t2-FRACTAL", "org.gnome.Fractal");
        let n = snapshot()
            .iter()
            .filter(|a| a.app_name.eq_ignore_ascii_case("t2-Fractal"))
            .count();
        assert_eq!(n, 1);
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
        record("t3-a", "old-icon");
        record("t3-b", "other");
        record("t3-a", "new-icon");

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
            record(&format!("t4-{i}"), "icon");
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
}
