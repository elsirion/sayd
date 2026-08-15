//! The settings model: what the window is allowed to show and to write.
//!
//! Nothing here draws anything. `SettingsModel` owns the one path a change
//! takes -- mutate a copy, validate it, write it through the `ConfigStore`
//! from Task 2, and only then let the window see it -- so the window layer
//! (`window.rs`, filled in by Task 5) can be nothing but widgets that read
//! `current()`/`voices()`/`MODELS` and call `edit`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use sayd_core::config::Config;

use crate::config_watch::ConfigStore;

/// The model values, with the measured trade-off shown inline in the
/// window. Numbers are from the benchmark recorded in the design doc; do
/// not adjust them without re-measuring.
pub const MODELS: [(&str, &str); 3] = [
    ("fp32", "best quality, RTF 4.78"),
    ("fp16", "RTF 4.66"),
    ("q8", "fastest, RTF 1.40, some quality loss"),
];

/// Speed bounds, matching `Engine`'s clamp exactly. Two places enforcing
/// different bounds would let the window write a value the engine then
/// silently changed.
pub const SPEED_MIN: f32 = 0.5;
pub const SPEED_MAX: f32 = 2.0;

/// What the window's spin rows offer, per spec §8. These live here, not in
/// `window.rs`, for the same reason everything else does: the window is the
/// one layer with no test coverage, so it must contain no number of its own.
///
/// Unlike `SPEED_MIN`/`SPEED_MAX` these are deliberately *not* enforced by
/// `validate`. `edit` seeds its copy from whatever the file currently holds,
/// so clamping here would mean a hand-edited `threads = 64` got silently
/// rewritten to 32 the next time the user nudged an unrelated row -- the
/// same shape of "an edit rewrites a field nobody touched" bug that `edit`'s
/// seeding was changed to avoid. The spinner simply cannot *produce* a value
/// outside these; a value that arrived some other way is left alone.
///
/// `f64` because that is what `gtk::Adjustment` takes, and a cast in the
/// widget layer is one more place for the two to disagree.
pub const THREADS_MIN: f64 = 1.0;
pub const THREADS_MAX: f64 = 32.0;
pub const THREADS_STEP: f64 = 1.0;
pub const SPEED_STEP: f64 = 0.05;
/// `0` means never unload, which is why the minimum is 0 and not 1.
pub const IDLE_UNLOAD_MIN: f64 = 0.0;
pub const IDLE_UNLOAD_MAX: f64 = 3600.0;
pub const IDLE_UNLOAD_STEP: f64 = 30.0;
pub const MAX_CHARS_MIN: f64 = 100.0;
pub const MAX_CHARS_MAX: f64 = 200_000.0;
pub const MAX_CHARS_STEP: f64 = 500.0;
/// `0` is the minimum because it means something, and something the row has
/// to be able to offer: `Limiter::decide`'s `cooldown_secs == 0` arm turns
/// rate limiting off entirely, so every notification from an allowed
/// application is spoken. See the Notifications group's subtitle, which says
/// that rather than letting `0` read as "no wait between announcements".
pub const COOLDOWN_MIN: f64 = 0.0;
/// An hour, the same ceiling `IDLE_UNLOAD_MAX` uses: past it the spinner is
/// no longer a control anyone drives to the end, and a longer window is a
/// hand edit (which, as with every other row here, is left alone rather than
/// clamped -- see the doc comment above).
pub const COOLDOWN_MAX: f64 = 3600.0;
pub const COOLDOWN_STEP: f64 = 5.0;

/// How long a burst of edits is allowed to keep collapsing into one write.
///
/// The number that matters is `GtkSpinButton`'s auto-repeat: holding one of
/// the +/- buttons steps the value roughly every 20 ms after a 200 ms
/// initial delay, and every step is a `value` notify and so an `edit`.
/// Dragging Threads from 1 to 32 is 31 of them. Written through one by one
/// that is 31 `config.toml` rewrites and -- because `threads` is one of the
/// two fields that invalidates the ORT session -- 31 teardowns and rebuilds
/// of a ~1.27 GB session, for a value the user was only passing through.
///
/// Debounce rather than rate-limit: the timer restarts on each edit, so
/// nothing is written until the user lets go. 250 ms is comfortably above
/// the repeat interval (so a held button really is one write) and well
/// below the threshold at which "changes write through immediately" stops
/// being true from the user's side. It is also the window in which a change
/// would be lost if the daemon were killed the instant after making it;
/// `SettingsModel` is never dropped in the daemon, so its flush-on-drop
/// does not cover SIGTERM.
const WRITE_DEBOUNCE: Duration = Duration::from_millis(250);

/// What the writer thread and the window's thread share.
struct Pending {
    /// What the window should draw: the newest config this model has
    /// accepted, whether or not it has reached the disk yet.
    ///
    /// Not the source of truth. It is seeded from the store at construction
    /// and by `refresh`, and updated by this model's own accepted `edit`s;
    /// nothing here subscribes to `ConfigStore::reload`, so a hand edit
    /// landing while the window is *already* open leaves it stale. That is
    /// display staleness only -- `edit` does not build its write on top of
    /// it (see `edit`) -- and closing it would need a change signal the
    /// store does not expose. The spec asks for a view of the config, not
    /// for live-updating widgets.
    current: Config,
    /// The part of `current` still owed to the disk, or `None` when the two
    /// agree. Deliberately *not* cleared while a write is in flight: while
    /// it is set, `edit` seeds from it and so never touches
    /// `ConfigStore::current`, whose lock the writer is holding across the
    /// disk write at that very moment.
    write: Option<Config>,
    /// What `write` was seeded from at the *start* of the burst currently
    /// accumulating in it -- `None` exactly when `write` is. IMPORTANT 2:
    /// `write_loop` cannot just write `write` verbatim, because a burst is
    /// seeded once and can sit debouncing for up to `WRITE_DEBOUNCE` while
    /// something else (a tray mute, an MPRIS rate change, a hand edit)
    /// writes the file in between -- writing the whole seeded copy back
    /// would silently clobber it. Comparing `write` against `write_seed`
    /// field by field is how `write_loop` tells "this burst actually
    /// changed this field" from "this field just happens to match its
    /// starting value"; see `ConfigStore::save_merging` and
    /// `merge_untouched` in `config_watch.rs`. Set only when a burst starts
    /// (`edit` finds `write` already `None`) and cleared in lock-step with
    /// `write` by `write_loop` once nothing is owed -- see both.
    write_seed: Option<Config>,
    /// Set by `Drop` to bring the writer thread down, after one last
    /// undebounced flush so a change made just before the window closed is
    /// not lost.
    stop: bool,
    /// Set by `SettingsModel::flush` to skip the rest of `WRITE_DEBOUNCE`
    /// without stopping the writer thread -- unlike `stop`, this is not the
    /// end of the model's life, just a request that whatever is owed go out
    /// now. Consumed (set back to `false`) by `write_loop` as soon as it
    /// breaks out of the debounce wait, so it never causes a *later* burst
    /// to skip its own debounce.
    flush: bool,
    /// The outcome of the most recently *finished* write attempt, kept so
    /// `flush` can report success or failure to its caller after waiting for
    /// `write` to clear -- `write` alone cannot tell the two apart, since
    /// `write_loop` clears it on both a successful save and a failed one
    /// (see the failure arm's comment). `None` until the first write
    /// attempt finishes.
    last_write_error: Option<String>,
}

/// The half of `SettingsModel` the writer thread also holds. Separate from
/// `SettingsModel` itself so the thread does not keep the model alive and
/// its `Drop` can actually run.
struct Shared {
    store: Arc<ConfigStore>,
    pending: Mutex<Pending>,
    /// Raised when there is something to write, or a stop to honour.
    work: Condvar,
    /// Raised when a write attempt has finished. Only tests wait on it, but
    /// the writer signals it unconditionally: a `notify_all` on a condvar
    /// nobody waits on is a few nanoseconds, and a `#[cfg(test)]` around it
    /// would mean the tested code path is not the shipped one.
    done: Condvar,
    /// Where a *write* failure goes, as opposed to a validation failure.
    ///
    /// Validation is synchronous and its error is returned straight from
    /// `edit`, so the window can toast it next to the row the user just
    /// touched. A write happens later on another thread, so its failure has
    /// to come back out of band. The window installs a sender for as long as
    /// it is open (see `watch_write_failures`) and drains it on the glib
    /// main thread; with no window open there is nobody to tell, which is
    /// why the writer also logs.
    failures: Mutex<Option<async_channel::Sender<String>>>,
    /// Completed `ConfigStore::save` calls. The debounce exists to keep this
    /// number far below the number of `edit`s, and that difference is
    /// invisible in the final config, so it has to be counted to be tested.
    writes: AtomicUsize,
}

/// Poison-tolerant, for the reason spelled out on `ConfigStore::stamp`: this
/// mutex is taken from GTK signal handlers, glib calls those through an
/// `extern "C"` frame, and a panic in one of those is a non-unwinding panic
/// that aborts the daemon outright rather than unwinding. Nothing under this
/// lock can leave a `Config` half-updated -- every write is a whole-value
/// replacement -- so reading through the poison is safe as well as
/// necessary. Same shape as `EngineHandle::snapshot`.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The window's view of the config, and the one path a change takes out of
/// it.
///
/// Writes do not happen on the caller's thread. `edit` validates, adopts and
/// returns; a writer thread owned by this model debounces the accepted
/// configs and calls `ConfigStore::save` off to one side. Two reasons, and
/// both of them bite in ordinary use:
///
/// - `save` holds the store's stamp across `Config::save_to`, a temp-write
///   plus rename in the user's home with no timeout anywhere on the path.
///   Its own doc comment says in as many words that it must not be called
///   from a thread that cannot block on disk. The GTK main thread is exactly
///   such a thread, and on a network, FUSE or encrypted home the whole
///   window would freeze for as long as the filesystem took.
/// - Holding a spin button's +/- auto-repeats; see [`WRITE_DEBOUNCE`].
pub struct SettingsModel {
    shared: Arc<Shared>,
    voices: Vec<String>,
    /// The writer thread, or `None` if it could not be started at all
    /// (finding 8) -- and, after `Drop` has taken it to join, `None` for
    /// that reason instead.
    ///
    /// Nothing drains `pending.write` without it, so `edit` and `flush` both
    /// refuse rather than pretend: an edit that cannot be written must not
    /// be reported as applied, and a shutdown flush must not wait out its
    /// whole timeout for a thread that was never there.
    writer: Option<std::thread::JoinHandle<()>>,
}

impl SettingsModel {
    pub fn new(store: Arc<ConfigStore>, models_dir: PathBuf, current: Config) -> Self {
        let shared = Arc::new(Shared {
            store,
            pending: Mutex::new(Pending {
                current,
                write: None,
                write_seed: None,
                stop: false,
                flush: false,
                last_write_error: None,
            }),
            work: Condvar::new(),
            done: Condvar::new(),
            failures: Mutex::new(None),
            writes: AtomicUsize::new(0),
        });
        let writer = {
            let shared = shared.clone();
            match std::thread::Builder::new()
                .name("settings-writer".into())
                .spawn(move || write_loop(&shared))
            {
                Ok(w) => Some(w),
                // Finding 8: swallowed with `.ok()`, this was the quietest
                // possible failure. Every `edit` would be accepted and shown
                // to the user as applied, `pending.write` would be set and
                // never drained, nothing would ever reach the disk, and the
                // shutdown `flush` would block its full timeout waiting for
                // a thread that does not exist. `edit` and `flush` now check
                // for it (see `writer`'s doc comment), but it still has to
                // be said out loud once, here, where the reason is known.
                Err(e) => {
                    eprintln!(
                        "warning: could not start the settings writer thread: {e}; \
                         settings changes cannot be saved this session"
                    );
                    None
                }
            }
        };
        SettingsModel {
            shared,
            voices: list_voices(&models_dir),
            writer,
        }
    }

    /// A model in the state finding 8 leaves behind: no writer thread.
    ///
    /// There is no way to make `Builder::spawn` fail on demand, so this
    /// starts the real thread and then stops and joins it, which reaches
    /// exactly the same state (`writer: None`, nothing draining
    /// `pending.write`) by a route a test can take.
    #[cfg(test)]
    fn without_writer(store: Arc<ConfigStore>, models_dir: PathBuf, current: Config) -> Self {
        let mut m = Self::new(store, models_dir, current);
        lock(&m.shared.pending).stop = true;
        m.shared.work.notify_all();
        if let Some(w) = m.writer.take() {
            let _ = w.join();
        }
        m
    }

    /// The dropdown's contents: sorted voice-pack names.
    pub fn voices(&self) -> &[String] {
        &self.voices
    }

    /// What the window should be showing right now. See `Pending::current`.
    pub fn current(&self) -> Config {
        lock(&self.shared.pending).current.clone()
    }

    /// Re-seed the display cache from the store, and hand back what it now
    /// holds.
    ///
    /// `current` is refreshed only by this model's own accepted `edit`s (see
    /// `Pending::current`), so a hand edit that `ConfigStore::reload` applied
    /// in between leaves it stale. The window calls this as it builds its
    /// rows and again whenever it is re-presented, so what it draws comes
    /// from what the file actually holds rather than from whatever this
    /// model last wrote itself.
    ///
    /// `store.current()` and not a fresh `Config::load`: it already reflects
    /// both directions -- our writes and the watcher's reloads -- without a
    /// read of the file, so this cannot race the debounce thread or see a
    /// half-written file (see `ConfigStore::current`'s doc comment).
    ///
    /// A write still owed to the disk wins over the file: it *is* the newer
    /// value, and the store has not been told about it yet.
    pub fn refresh(&self) -> Config {
        // Finding 6: routed around `ConfigStore::current` exactly the way
        // `edit` is, and for the same reason. `store.current()` takes the
        // store's stamp, and `ConfigStore::save` holds that stamp across a
        // temp-write and rename in the user's home with no timeout anywhere
        // on the path -- on a network, FUSE or encrypted home, for as long
        // as the filesystem takes. This runs on the glib main thread (the
        // window calls it as it builds its rows and whenever it is
        // re-presented), so taking the stamp unconditionally meant
        // presenting the window during a write froze the UI for that write.
        // A pending write is the newer value anyway, so when there is one
        // there is nothing the store could tell us that we do not already
        // have.
        //
        // Checked and released before touching the store: the writer thread
        // takes the stamp first and `pending` second, always, so nesting
        // them the other way round here would be a lock cycle. A write that
        // starts in the gap is harmless -- the same window `edit` has -- and
        // the re-check below is what keeps its value from being overwritten
        // by the staler one this read may then return.
        if let Some(pending) = lock(&self.shared.pending).write.clone() {
            return pending;
        }
        let latest = self.shared.store.current();
        let mut p = lock(&self.shared.pending);
        if p.write.is_none() {
            p.current = latest;
        }
        p.current.clone()
    }

    /// Apply one change: mutate a copy, validate it, adopt it, and hand it
    /// to the writer thread. Returns the config the window should now be
    /// showing.
    ///
    /// A rejected edit changes nothing at all, so the window never shows a
    /// value the file does not hold. An *accepted* one is adopted before it
    /// reaches the disk -- that is the point of the writer thread -- so for
    /// up to [`WRITE_DEBOUNCE`] the model shows a value the file does not
    /// have yet. That is a deliberate trade against freezing the UI on a
    /// slow filesystem; if the write then fails, the writer puts `current`
    /// back to the file's truth and tells the window, which redraws from it.
    ///
    /// The copy is seeded from the pending write if there is one, and only
    /// otherwise from `store.current()`. Never from `current`: that cache is
    /// refreshed only after this model's own writes, so it goes stale the
    /// moment something else changes the file -- a hand edit picked up by
    /// `ConfigStore::reload`, in particular. Seeding from it would mutate
    /// only the one field this edit touches and then write the *whole* stale
    /// copy back, silently reverting whatever the hand edit had changed
    /// while reporting success for a change the user never made.
    ///
    /// Preferring the pending write is not just about not losing the
    /// previous edit in a burst. It is also what keeps this call off the
    /// store's stamp while the writer is holding it across a disk write --
    /// the freeze this whole arrangement exists to avoid. The one call that
    /// does take the stamp is the first edit of a burst, when no write is in
    /// flight; the only thing that can be holding it then is the watcher's
    /// debounce thread inside `reload`, across a read and a parse of a small
    /// TOML file. That exposure is unchanged from before the writer thread
    /// existed.
    ///
    /// Deliberately not seeded from the engine's own config either: that
    /// also carries runtime-only changes from the tray and MPRIS
    /// (`SetVoice`/`SetSpeed`/`SetMuted`), which are intentionally never
    /// persisted. Seeding from there would let an unrelated slider move
    /// write a transient tray mute into the file permanently.
    pub fn edit(&self, f: impl FnOnce(&mut Config)) -> Result<Config, String> {
        // Finding 8: with no writer thread there is nobody to drain
        // `pending.write`, so adopting this edit would show the user a value
        // that is never going to reach the file -- the one thing this whole
        // module is arranged to avoid. Refusing sends it back through the
        // same toast a validation failure uses.
        if self.writer.is_none() {
            return Err(
                "the settings writer thread is not running; changes cannot be saved".to_string(),
            );
        }
        let pending_write = lock(&self.shared.pending).write.clone();
        let mut next = match pending_write {
            Some(pending) => pending,
            None => self.shared.store.current(),
        };
        // IMPORTANT 2: captured before `f` mutates `next`, so it is what
        // this edit started from. `write_loop`'s `save_merging` compares
        // this against whatever the burst's `write` has become by the time
        // it is finally written, to tell which fields the burst actually
        // touched from which ones are just carrying their stale seed value
        // -- see `Pending::write_seed` and `merge_untouched` in
        // `config_watch.rs`.
        let seed = next.clone();
        f(&mut next);
        validate(&mut next)?;

        let mut p = lock(&self.shared.pending);
        p.current = next.clone();
        // Set only at a burst's genuine start. `p.write` is `None` here
        // either because this is the first edit since the writer last
        // finished, or because it finished *during* this call, in the gap
        // between the read above and this lock -- `write_loop` is the only
        // other place `write` changes, and it only ever clears it, so that
        // is the sole way this check can disagree with the read above. Both
        // cases want the same thing: `seed` is this edit's own starting
        // point, and it equals exactly what the writer just finished
        // writing (or is about to), which is the correct comparison base
        // for a burst starting now either way.
        if p.write.is_none() {
            p.write_seed = Some(seed);
        }
        p.write = Some(next.clone());
        drop(p);
        self.shared.work.notify_one();
        Ok(next)
    }

    /// Ask to be told about write failures, for as long as the window is
    /// open.
    ///
    /// Returns a receiver the caller is expected to drain on its own event
    /// loop. Bounded and small on purpose: these are toasts, and a window
    /// that has fallen far enough behind to fill it has already been told.
    /// Installing a second sender drops the first, which is what closes the
    /// previous window's drain task.
    pub fn watch_write_failures(&self) -> async_channel::Receiver<String> {
        let (tx, rx) = async_channel::bounded(4);
        *lock(&self.shared.failures) = Some(tx);
        rx
    }

    /// Stop reporting write failures. Dropping the sender is what ends the
    /// window's drain task, and with it the last reference it holds to the
    /// window.
    pub fn stop_watching_write_failures(&self) {
        *lock(&self.shared.failures) = None;
    }

    /// How many times `ConfigStore::save` has actually run. The whole point
    /// of the debounce is that this stays far below the number of `edit`s,
    /// and a coalesced burst and a written-one-by-one burst leave identical
    /// files behind -- so nothing but a count can tell them apart.
    #[cfg(test)]
    fn writes(&self) -> usize {
        self.shared.writes.load(Ordering::Relaxed)
    }

    /// Force whatever edit is still owed to disk out right now, skipping the
    /// rest of `WRITE_DEBOUNCE`, and block up to `timeout` for that attempt
    /// to finish.
    ///
    /// For the daemon's shutdown path (`run_daemon` in `main.rs`), called
    /// there because nothing else ever would: this model lives in
    /// `settings`'s own `OnceLock` for the process's whole life, so its
    /// `Drop` -- which does exactly this same skip-the-debounce flush, see
    /// `stop`'s doc comment -- never runs in production, only in tests that
    /// build a standalone `SettingsModel` and let it go out of scope. A
    /// settings change made in the last `WRITE_DEBOUNCE` before SIGTERM/
    /// `Quit()`/`say quit` would otherwise still be sitting on the writer's
    /// queue, shown to the user as applied, when the process exits.
    ///
    /// A no-op, immediately, when there is nothing owed: `write.is_none()`
    /// is true both when no edit was ever made and when the debounce already
    /// finished writing on its own, and shutdown must not wait out a fake
    /// deadline for either.
    ///
    /// Returns the outcome of the forced write (or `Ok(())` for the no-op
    /// case) rather than just whether it finished in time, so the shutdown
    /// path can tell "nothing to do" and "wrote fine" apart from "a change
    /// the user was told was saved did not make it" and say so. The writer
    /// thread's own failure arm already puts the same message on stderr
    /// unconditionally (there may be no window open to hear about it any
    /// other way) -- this is for the caller that specifically needs to know
    /// whether *its* flush attempt succeeded, not just that some write,
    /// sometime, failed.
    pub fn flush(&self, timeout: Duration) -> Result<(), String> {
        let mut p = lock(&self.shared.pending);
        if p.write.is_none() {
            return Ok(());
        }
        // Finding 8: nothing will ever clear `write` without the writer
        // thread, so waiting for it would spend the whole timeout on the
        // shutdown path and then report a timeout for a write that was
        // never going to happen. Say what actually went wrong instead, at
        // once. (`edit` refuses in the same case, so reaching this needs the
        // thread to have died after an edit was accepted rather than failing
        // to start -- rare, but the wait would be just as pointless.)
        if self.writer.is_none() {
            return Err("the settings writer thread is not running".to_string());
        }
        p.flush = true;
        drop(p);
        self.shared.work.notify_one();

        let mut p = lock(&self.shared.pending);
        let deadline = std::time::Instant::now() + timeout;
        while p.write.is_some() {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(format!(
                    "the write did not finish within {:.1}s",
                    timeout.as_secs_f64()
                ));
            }
            p = match self.shared.done.wait_timeout(p, left) {
                Ok((g, _)) => g,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        p.last_write_error.clone().map_or(Ok(()), Err)
    }

    /// Block until nothing is owed to the disk. Test-only: production code
    /// on the glib thread must never wait on the writer.
    #[cfg(test)]
    fn settle(&self, timeout: Duration) -> bool {
        let mut p = lock(&self.shared.pending);
        let deadline = std::time::Instant::now() + timeout;
        while p.write.is_some() {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            p = match self.shared.done.wait_timeout(p, left) {
                Ok((g, _)) => g,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        true
    }
}

impl Drop for SettingsModel {
    /// Flush and join. The daemon never drops this -- it lives in a
    /// `OnceLock` for the life of the process -- so in practice this is what
    /// makes the writer thread deterministic for tests, which would
    /// otherwise race their own `tempdir` cleanup against a write.
    fn drop(&mut self) {
        lock(&self.shared.pending).stop = true;
        self.shared.work.notify_all();
        if let Some(w) = self.writer.take() {
            // A writer that panicked has already lost whatever it was
            // holding; there is nothing useful to do about it here, and
            // resuming the panic during a drop would abort.
            let _ = w.join();
        }
    }
}

/// The writer thread: debounce, write, report.
///
/// Everything blocking lives here -- `ConfigStore::save`'s temp-write and
/// rename, and the stamp it holds across them -- so that none of it is on
/// the thread that draws the window.
fn write_loop(shared: &Shared) {
    loop {
        let mut p = lock(&shared.pending);

        // Nothing to do: wait for an edit or for the drop.
        while p.write.is_none() && !p.stop {
            p = match shared.work.wait(p) {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
        if p.write.is_none() {
            return; // stopped with nothing owed
        }

        // Debounce: hold off while edits keep arriving, so a held spin
        // button is one write rather than one per repeat. A stop or an
        // explicit flush request (`SettingsModel::flush`) both skip the
        // wait entirely -- a stop because a change made just before the
        // window closed must not be dropped on the floor to save 250 ms, a
        // flush because its caller is deliberately waiting on this write and
        // has already decided the debounce should not apply to it.
        while !p.stop && !p.flush {
            let (guard, timed_out) = match shared.work.wait_timeout(p, WRITE_DEBOUNCE) {
                Ok((g, t)) => (g, t.timed_out()),
                Err(poisoned) => {
                    let (g, t) = poisoned.into_inner();
                    (g, t.timed_out())
                }
            };
            p = guard;
            if timed_out {
                break;
            }
        }
        // Consumed here, not left set: otherwise the *next* burst -- which
        // owes nothing to the caller that requested this flush -- would also
        // skip its own debounce.
        p.flush = false;

        // Cloned, not taken: while `write` is set, `edit` seeds from it and
        // so stays off the store's stamp -- which is precisely what `save`
        // is about to hold across a disk write.
        let Some(cfg) = p.write.clone() else { return };
        // IMPORTANT 2: the burst's own starting point, needed by
        // `save_merging` to tell which fields this burst actually changed
        // from which ones are just carrying their seed value -- see
        // `Pending::write_seed`. Falls back to `cfg` itself (a no-op merge:
        // every field then reads as "unchanged", so `save_merging` takes
        // it entirely from the fresh stamp) only for an invariant that
        // should not break -- `edit` sets `write_seed` in the same lock
        // acquisition it first sets `write` in.
        let seed = p.write_seed.clone().unwrap_or_else(|| cfg.clone());
        let stop = p.stop;
        drop(p);

        let outcome = shared.store.save_merging(&seed, &cfg);
        shared.writes.fetch_add(1, Ordering::Relaxed);

        match outcome {
            Ok(()) => {
                let mut p = lock(&shared.pending);
                // Only if nothing arrived while we were writing; if
                // something did, it is a newer config and still owed, and
                // `write_seed` must keep tracking the same burst it already
                // does -- that newer `edit` left it alone precisely because
                // `write` was still `Some` when it ran.
                if p.write.as_ref() == Some(&cfg) {
                    p.write = None;
                    p.write_seed = None;
                }
                // Recorded even when a newer write superseded this one: a
                // `flush` waiting on that newer write should see the newest
                // finished attempt's outcome, not this now-stale one.
                p.last_write_error = None;
            }
            Err(e) => {
                // The file still holds what it held before -- `save_to`
                // fails before its rename, and `save` restores the stamp it
                // displaced -- so the truth is whatever the store now says,
                // and the window has to be put back to it. Read outside the
                // `pending` lock: the writer takes the stamp first and
                // `pending` second, always, or the two are a cycle.
                let truth = shared.store.current();
                let mut p = lock(&shared.pending);
                p.write = None;
                p.write_seed = None;
                p.current = truth;
                p.last_write_error = Some(e.clone());
                drop(p);
                // Always logged, because there may be no window listening --
                // the debounce tail can outlive the one that made the edit.
                eprintln!("warning: {e}");
                if let Some(tx) = lock(&shared.failures).as_ref() {
                    let _ = tx.try_send(e);
                }
            }
        }

        shared.done.notify_all();
        if stop {
            return;
        }
    }
}

/// What `kokoro_synth::model_file_for` loads for a `model` string it does
/// not recognise -- its `_ => "model.onnx"` arm, which is fp32's file.
///
/// Taken from `MODELS[0]` rather than written out again so the two cannot
/// drift; `the_fallback_model_is_the_one_the_synthesizer_actually_loads`
/// pins it against both `model_file_for` and `Config::default`.
const FALLBACK_MODEL: &str = MODELS[0].0;

fn known_models() -> String {
    MODELS
        .iter()
        .map(|(v, _)| *v)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Clamp the fields that have a sensible nearest value, saying what was
/// changed and to what.
///
/// Shared by `validate` (the window) and `normalize` (the reload path) so
/// there is one set of range rules rather than two that can disagree about
/// what the engine will actually honour. The bounds themselves are
/// `SPEED_MIN`/`SPEED_MAX`, which exist to match `Engine`'s own clamp
/// exactly.
fn clamp_ranges(cfg: &mut Config) -> Vec<String> {
    let mut warnings = Vec::new();
    let speed = cfg.speed.clamp(SPEED_MIN, SPEED_MAX);
    if (speed - cfg.speed).abs() > f32::EPSILON {
        warnings.push(format!(
            "speed {} is outside {SPEED_MIN}-{SPEED_MAX}; running at {speed}",
            cfg.speed
        ));
        cfg.speed = speed;
    }
    if cfg.threads == 0 {
        warnings.push("threads = 0 is not a thread count; running with 1".to_string());
        cfg.threads = 1;
    }
    warnings
}

/// Clamp what has a sensible nearest value, reject what does not.
///
/// Speed and thread count have obvious clamps. A model string does not: an
/// unrecognised one would fall through `model_file_for` to fp32, so the
/// file would claim something other than what loads -- and this path is
/// about to *write* the value, so refusing is the only way to keep the file
/// honest. That is the deliberate asymmetry against `normalize` below,
/// which cannot refuse a file the user already wrote.
///
/// The clamps' own warnings are dropped rather than returned: an accepted
/// edit is not a failure, and the window already marks a row whose value
/// the engine had to clamp (see `window.rs`'s spin rows).
fn validate(cfg: &mut Config) -> Result<(), String> {
    let _ = clamp_ranges(cfg);
    if !MODELS.iter().any(|(v, _)| *v == cfg.model) {
        return Err(format!(
            "'{}' is not a model this build knows; expected one of {}",
            cfg.model,
            known_models()
        ));
    }
    Ok(())
}

/// The config as the daemon will actually run it, plus one warning per field
/// whose written value could not be honoured.
///
/// For configs that arrive from the *file* -- at startup and on every
/// reload -- rather than from the window. The difference from `validate` is
/// the whole point: the window may refuse a value because the user is about
/// to write it, but a file the user has already written cannot be refused.
/// Rejecting it would mean either ignoring the whole edit (so a typo in one
/// field discards the other eight) or wedging the daemon on a file it will
/// not run; both are worse than running the nearest thing and saying so.
///
/// So an unrecognised `model` becomes [`FALLBACK_MODEL`] here, because that
/// is what `model_file_for` was going to load anyway -- IMPORTANT 3: before
/// this, `model = "int4"` was applied verbatim, the daemon ran fp32 while
/// `say status` and the file both claimed int4, and nothing at all was
/// logged. Making the config say what is really running also un-wedges the
/// settings window, whose `edit` seeds from the store and would otherwise
/// have `validate` refuse every later edit over a field the user never
/// touched.
///
/// This deliberately does not write anything back to the file: spec §11
/// ("do not overwrite the user's file until they change something in the
/// UI") means the corrected value reaches the disk only when the user next
/// changes a setting, which is also the moment the file stops being theirs
/// alone.
pub fn normalize(cfg: &mut Config) -> Vec<String> {
    let mut warnings = clamp_ranges(cfg);
    if !MODELS.iter().any(|(v, _)| *v == cfg.model) {
        warnings.push(format!(
            "'{}' is not a model this build knows (expected one of {}); running {} instead",
            cfg.model,
            known_models(),
            FALLBACK_MODEL
        ));
        cfg.model = FALLBACK_MODEL.to_string();
    }
    warnings
}

/// Is `name` already on the notification allowlist?
///
/// Case-folded and trimmed, because that is how the list is *read*:
/// `notify::policy`'s `is_allowed` lowercases both sides before comparing,
/// and `compose` trims an `app_name` before speaking it. "Signal" and
/// " signal " are one entry to the layer that consumes this list, so they
/// must be one entry to the layer that edits it -- otherwise the window
/// would happily add a second row that changes nothing and cannot be told
/// apart from the first.
///
/// A name that is empty once trimmed is on no list: it is what an empty
/// entry field produces, and `compose` treats an `app_name` that trims to
/// nothing as no name at all.
///
/// Public because the window needs to ask the question -- whether to clear
/// the entry field after an add -- without knowing the rule.
pub fn allow_contains(cfg: &Config, name: &str) -> bool {
    let name = name.trim().to_lowercase();
    !name.is_empty()
        && cfg
            .notifications
            .allow
            .iter()
            .any(|a| a.trim().to_lowercase() == name)
}

/// Put `name` on the notification allowlist, or leave the config exactly as
/// it is.
///
/// An empty name and one already on the list are both no-ops, decided here
/// rather than in the window: the window is the one layer with no test
/// coverage, and "what happens when the user presses Add on an empty field"
/// is a rule like any other.
///
/// Takes `&mut Config` and mutates it in place so it can be handed straight
/// to `SettingsModel::edit`, whose copy is seeded from the file rather than
/// from anything the window has been holding. An add expressed as "write
/// back the list I drew" would silently revert an entry added by a hand edit
/// (or removed by one) since the window last redrew.
pub fn allow_add(cfg: &mut Config, name: &str) {
    let name = name.trim();
    if name.is_empty() || allow_contains(cfg, name) {
        return;
    }
    // Stored trimmed: the surrounding space would survive into the file, and
    // `is_allowed` would then match it only because it trims too. A file that
    // needs a reader's help to be read is a file that will eventually be
    // hand-edited wrong.
    cfg.notifications.allow.push(name.to_string());
}

/// Take `name` off the notification allowlist.
///
/// Every case-folded match goes, not just the first. A hand-edited file can
/// hold both `"Signal"` and `"signal"`; `is_allowed` matches either, so a
/// Remove button that took away only the row it was attached to would leave
/// the application still speaking with its row gone -- the worst of the
/// available outcomes.
pub fn allow_remove(cfg: &mut Config, name: &str) {
    let name = name.trim().to_lowercase();
    cfg.notifications
        .allow
        .retain(|a| a.trim().to_lowercase() != name);
}

/// Voice-pack names from `<models_dir>/voices/*.bin`, sorted.
///
/// A missing directory yields an empty list rather than an error: the
/// window must still open so the rest of the settings can be reached.
fn list_voices(models_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(models_dir.join("voices")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            // `KokoroSynthesizer::voice_exists` rejects anything that is not
            // a file at submit time, so a directory named e.g. `foo.bin/` --
            // a partially-written download, say -- would only ever be a dead
            // end once selected. Filtering it here keeps it out of the
            // dropdown in the first place.
            if path.extension()? != "bin" || !path.is_file() {
                return None;
            }
            Some(path.file_stem()?.to_string_lossy().into_owned())
        })
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use sayd_core::audio::VecSink;
    use sayd_core::handle::EngineHandle;
    use sayd_core::synth::StubSynthesizer;

    use crate::config_watch::ReloadOutcome;

    /// Long enough that a machine under load does not fail these, short
    /// enough that a genuinely stuck writer fails them rather than hanging
    /// the suite. Every use is "wait until it happened", never "sleep this
    /// long".
    const SETTLE: Duration = Duration::from_secs(5);

    /// Wait for the writer thread to have nothing left owed to the disk.
    fn settled(m: &SettingsModel) {
        assert!(
            m.settle(SETTLE),
            "the writer thread never finished its pending write"
        );
    }

    /// `starting` is `Config::default()` in every test here, matching the
    /// `Config::default()` the engine below is spawned with -- the store
    /// and the engine must agree at t=0 (see `ConfigStore::new`'s doc
    /// comment), and `Config::default()` for both is what keeps that true
    /// without duplicating a non-default config in two places.
    fn store_in(dir: &Path) -> (Arc<ConfigStore>, EngineHandle) {
        let engine = EngineHandle::spawn(
            Config::default(),
            Box::new(StubSynthesizer::new()),
            // Capacity large enough that nothing here (which never drives
            // real synthesis) could plausibly fill it -- same figure the
            // rest of the suite uses.
            Box::new(VecSink::new(24_000 * 10)),
        );
        let store = Arc::new(ConfigStore::new(
            dir.join("config.toml"),
            engine.clone(),
            Config::default(),
        ));
        (store, engine)
    }

    fn models_dir_with(voices: &[&str], dir: &Path) -> PathBuf {
        let v = dir.join("voices");
        std::fs::create_dir_all(&v).expect("voices dir");
        for name in voices {
            std::fs::write(v.join(format!("{name}.bin")), b"x").expect("voice pack");
        }
        dir.to_path_buf()
    }

    /// The dropdown's contents. Sorted so the list does not reshuffle
    /// between openings, and stripped of the `.bin` the daemon never shows.
    #[test]
    fn voices_are_listed_from_the_models_directory_sorted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["bm_george", "af_heart", "am_fenrir"], dir.path());
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::new(store, models, Config::default());
        assert_eq!(m.voices(), ["af_heart", "am_fenrir", "bm_george"]);
        engine.shutdown();
    }

    /// A directory named like a voice pack -- a partially-written download
    /// landing as `foo.bin/`, say -- must not appear in the dropdown.
    /// `KokoroSynthesizer::voice_exists` checks `is_file()` and rejects it at
    /// submit time regardless, so listing it here only gives the user a
    /// selectable dead end instead of no entry at all.
    #[test]
    fn a_directory_named_like_a_voice_pack_is_not_listed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart"], dir.path());
        std::fs::create_dir_all(models.join("voices").join("partial.bin"))
            .expect("partial voice directory");
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::new(store, models, Config::default());
        assert_eq!(m.voices(), ["af_heart"]);
        engine.shutdown();
    }

    /// A models directory that is missing or empty must produce an empty
    /// list, not a panic: the window still has to open so the user can see
    /// and fix everything else.
    #[test]
    fn a_missing_models_directory_yields_an_empty_voice_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::new(store, dir.path().join("nope"), Config::default());
        assert!(m.voices().is_empty());
        engine.shutdown();
    }

    /// An edit writes through to disk immediately -- the spec's "changes
    /// write through to the config file immediately and apply to the next
    /// utterance".
    #[test]
    fn an_edit_writes_through_to_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart", "am_fenrir"], dir.path());
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::new(store, models, Config::default());

        m.edit(|c| c.voice = "am_fenrir".into())
            .expect("edit succeeds");
        settled(&m);

        let (on_disk, err) = Config::load_from(&dir.path().join("config.toml"));
        assert_eq!(err, None);
        assert_eq!(on_disk.voice, "am_fenrir");
        assert_eq!(m.current().voice, "am_fenrir");
        engine.shutdown();
    }

    /// Regression: `edit` must not seed its copy from this model's own
    /// cache, which only this model's own writes ever refresh. Between an
    /// external edit and the model's next `edit`, that cache is stale --
    /// here, a hand edit changes `model` (picked up by `ConfigStore::reload`
    /// exactly as the watcher's debounce loop would pick it up), and then an
    /// unrelated field is changed through the model. Before the fix, `edit`
    /// seeded its copy from the model's stale cache (still the old model),
    /// mutated only the unrelated field, and wrote the whole stale copy
    /// back -- reverting the hand edit in both the file and the running
    /// engine while reporting success for a change the user never made. That
    /// is the exact scenario the spec's "Config is the single source of
    /// truth; the window is a view of it" rules out.
    #[test]
    fn an_edit_does_not_clobber_an_external_change_it_never_touched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart", "am_fenrir"], dir.path());
        let path = dir.path().join("config.toml");

        // Engine, store and model all start on the same non-default config
        // -- `store_in`'s helper hard-codes `Config::default()`, so it is
        // not reused here; see `ConfigStore::new`'s "engine and file agree
        // at t=0" doc comment for why they must match.
        let starting = Config {
            voice: "af_heart".into(),
            model: "fp32".into(),
            speed: 1.0,
            ..Config::default()
        };
        let engine = EngineHandle::spawn(
            starting.clone(),
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        let store = Arc::new(ConfigStore::new(
            path.clone(),
            engine.clone(),
            starting.clone(),
        ));
        let m = SettingsModel::new(store.clone(), models, starting.clone());

        // The hand edit: a field `edit` below never touches, written
        // straight to the file (as an editor or another tool would) and
        // picked up the way the watcher's debounce loop picks up a real
        // one -- `reload`.
        let hand_edited = Config {
            model: "q8".into(),
            ..starting.clone()
        };
        hand_edited.save_to(&path).expect("hand edit");
        assert_eq!(store.reload(), ReloadOutcome::Applied);

        // An unrelated edit through the model: the user moving the speed
        // slider, having never touched the model dropdown.
        m.edit(|c| c.speed = 1.5).expect("edit succeeds");
        settled(&m);

        let (on_disk, err) = Config::load_from(&path);
        assert_eq!(err, None);
        assert_eq!(
            on_disk.model, "q8",
            "an edit to an unrelated field must not revert the hand-edited model"
        );
        assert_eq!(on_disk.speed, 1.5, "the edit itself must still land");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut engine_cfg = engine.config();
        while engine_cfg.as_ref().map(|c| c.model.as_str()) != Some("q8")
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
            engine_cfg = engine.config();
        }
        assert_eq!(
            engine_cfg.expect("engine answers").model,
            "q8",
            "the running engine must not have been reverted to fp32 either"
        );
        engine.shutdown();
    }

    /// IMPORTANT 2, the reviewer's own reproduction. A field this model
    /// never edits at all (`muted` -- there is no mute row in the window)
    /// but that lands on disk *while* an unrelated edit is still inside its
    /// debounce must survive that edit's write when it finally lands.
    ///
    /// Before this, `write_loop` wrote the whole config the burst had been
    /// seeded with at its start -- `muted: false`, since nothing in this
    /// burst ever touches it -- and that write landed *after*
    /// `store.set_muted(true)` (the tray's own write, straight through the
    /// store, exactly as `say mute`/D-Bus/MPRIS do), clobbering it: the
    /// tray's checkbox would flip back off on its own about `WRITE_DEBOUNCE`
    /// after the user pressed Mute.
    #[test]
    fn a_pending_edit_does_not_clobber_a_mute_that_lands_during_its_debounce() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart"], dir.path());
        let path = dir.path().join("config.toml");
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::new(store.clone(), models, Config::default());

        // The window row nudged; the write is now pending, inside its
        // 250ms debounce.
        m.edit(|c| c.speed = 1.5).expect("edit succeeds");

        // The tray mute lands independently and immediately, straight
        // through the store -- exactly like `store.set_muted` inside
        // `persist_in_background`, never through the model.
        store.set_muted(true).expect("the mute write must succeed");

        // The window's debounced write lands.
        settled(&m);

        let (on_disk, err) = Config::load_from(&path);
        assert_eq!(err, None);
        assert_eq!(on_disk.speed, 1.5, "the edit itself must still land");
        assert!(
            on_disk.muted,
            "the mute must survive the window's pending write"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut engine_cfg = engine.config();
        while engine_cfg.as_ref().map(|c| c.muted) != Some(true)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
            engine_cfg = engine.config();
        }
        assert!(
            engine_cfg.expect("engine answers").muted,
            "the running engine must not have been unmuted by the window's write either"
        );
        engine.shutdown();
    }

    /// A window opening after a hand edit must draw the hand-edited value,
    /// not the one this model last wrote itself. `current` alone cannot
    /// deliver that -- only this model's own `edit`s refresh it -- which is
    /// exactly the display staleness the struct's doc comment describes and
    /// `refresh` (called from `window::build`) exists to close.
    #[test]
    fn refresh_picks_up_a_change_this_model_did_not_make() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart"], dir.path());
        let path = dir.path().join("config.toml");
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::new(store.clone(), models, Config::default());

        Config {
            model: "q8".into(),
            ..Config::default()
        }
        .save_to(&path)
        .expect("hand edit");
        assert_eq!(store.reload(), ReloadOutcome::Applied);

        assert_eq!(
            m.current().model,
            "fp32",
            "the cache is stale until something refreshes it -- that is the premise"
        );
        assert_eq!(m.refresh().model, "q8");
        assert_eq!(m.current().model, "q8", "and it stays refreshed");
        engine.shutdown();
    }

    /// Out-of-range values are clamped before they reach the file, so the
    /// file never contains a value the engine would silently reinterpret.
    #[test]
    fn speed_is_clamped_before_it_is_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart"], dir.path());
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::new(store, models, Config::default());

        m.edit(|c| c.speed = 5.0).expect("edit succeeds");
        assert!((m.current().speed - 2.0).abs() < f32::EPSILON);
        settled(&m);
        let (on_disk, _) = Config::load_from(&dir.path().join("config.toml"));
        assert!((on_disk.speed - 2.0).abs() < f32::EPSILON);
        engine.shutdown();
    }

    /// An unknown model string would silently fall back to fp32 inside the
    /// synthesizer. Rejecting it here means the file never holds a value
    /// that lies about what will be loaded.
    #[test]
    fn an_unknown_model_is_rejected_rather_than_silently_downgraded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart"], dir.path());
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::new(store, models, Config::default());

        let err = m
            .edit(|c| c.model = "int4".into())
            .expect_err("must be rejected");
        assert!(
            err.contains("int4"),
            "the rejected value must appear: {err}"
        );
        assert_eq!(m.current().model, "fp32", "a rejected edit must not stick");
        engine.shutdown();
    }

    /// A failed write must be reported rather than swallowed, and must not
    /// leave the model claiming a value the file does not have. This is the
    /// case M3's review flagged as needing "somewhere to surface a failed
    /// write" -- here it is.
    ///
    /// The report is asynchronous now that the write is: `edit` accepts the
    /// change (it is valid; nothing has touched the disk yet) and the writer
    /// thread discovers the failure afterwards. What has to survive that is
    /// the promise the synchronous version made -- that the model does not
    /// end up claiming a value the file never took -- plus somewhere for the
    /// window to hear about it.
    #[test]
    fn a_failed_write_is_reported_and_does_not_change_the_model() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart", "am_fenrir"], dir.path());
        let engine = EngineHandle::spawn(
            Config::default(),
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        // A path whose parent is a *file* cannot be created as a directory,
        // so `save_to` fails for a reason that needs no permission games.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("blocker");
        let store = Arc::new(ConfigStore::new(
            blocker.join("config.toml"),
            engine.clone(),
            Config::default(),
        ));
        let m = SettingsModel::new(store, models, Config::default());
        let failures = m.watch_write_failures();

        m.edit(|c| c.voice = "am_fenrir".into())
            .expect("valid, so it is accepted; the disk has not been touched yet");
        settled(&m);

        let err = failures
            .recv_blocking()
            .expect("the failure must reach the window");
        assert!(!err.is_empty());
        assert_eq!(
            m.current().voice,
            "af_heart",
            "a failed write must not leave the model out of step with the file"
        );
        engine.shutdown();
    }

    /// The reason the writer thread debounces at all.
    ///
    /// Holding a `GtkSpinButton`'s +/- auto-repeats, so dragging Threads
    /// from 1 to 32 is 31 `edit`s in well under a second. Written one by one
    /// that is 31 rewrites of `config.toml` and, because `threads`
    /// invalidates the ORT session, 31 teardowns and rebuilds of a ~1.27 GB
    /// session. A coalesced burst and a written-one-by-one burst leave the
    /// same file behind, so only the write count can tell them apart.
    #[test]
    fn a_burst_of_edits_becomes_a_single_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart"], dir.path());
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::new(store, models, Config::default());

        for threads in 2..=32usize {
            m.edit(|c| c.threads = threads).expect("edit succeeds");
        }
        settled(&m);

        assert_eq!(
            m.writes(),
            1,
            "31 spinner steps must not be 31 config writes and 31 session rebuilds"
        );
        let (on_disk, err) = Config::load_from(&dir.path().join("config.toml"));
        assert_eq!(err, None);
        assert_eq!(on_disk.threads, 32, "and the last value is the one kept");
        engine.shutdown();
    }

    /// Each edit in a burst must build on the one before it, not on the file
    /// (which is still several edits behind). Two *different* fields make
    /// that visible: seeding the second from the file would write it with
    /// the first field's old value and silently undo it.
    #[test]
    fn edits_within_one_burst_accumulate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart", "am_fenrir"], dir.path());
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::new(store, models, Config::default());

        m.edit(|c| c.voice = "am_fenrir".into())
            .expect("edit succeeds");
        m.edit(|c| c.threads = 7).expect("edit succeeds");
        settled(&m);

        let (on_disk, err) = Config::load_from(&dir.path().join("config.toml"));
        assert_eq!(err, None);
        assert_eq!(on_disk.voice, "am_fenrir");
        assert_eq!(on_disk.threads, 7);
        assert_eq!(m.writes(), 1);
        engine.shutdown();
    }

    /// A change made just before the window closed must still land. The
    /// daemon itself never drops this model, but a burst that ends inside
    /// the debounce window would otherwise be lost to whatever does.
    #[test]
    fn dropping_the_model_flushes_what_is_still_owed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart", "am_fenrir"], dir.path());
        let path = dir.path().join("config.toml");
        let (store, engine) = store_in(dir.path());

        {
            let m = SettingsModel::new(store, models, Config::default());
            m.edit(|c| c.voice = "am_fenrir".into())
                .expect("edit succeeds");
            // Deliberately no `settled`: the drop below happens inside the
            // debounce window, which is the case being tested.
        }

        let (on_disk, err) = Config::load_from(&path);
        assert_eq!(err, None);
        assert_eq!(on_disk.voice, "am_fenrir");
        engine.shutdown();
    }

    /// The shutdown-path counterpart to `dropping_the_model_flushes_what_is_
    /// still_owed`: in production this model is never dropped (it lives in
    /// `settings::HOST` for the daemon's whole life), so `flush` is the only
    /// thing that can ever land an edit made in the last `WRITE_DEBOUNCE`
    /// before the process exits. Flushing immediately after the edit, well
    /// inside the 250ms debounce window, is the point -- without `flush`
    /// skipping the debounce (or without `flush` existing at all), the file
    /// would not hold the edit yet at the moment this asserts.
    #[test]
    fn flush_lands_a_pending_edit_without_waiting_out_the_debounce() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart", "am_fenrir"], dir.path());
        let path = dir.path().join("config.toml");
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::new(store, models, Config::default());

        m.edit(|c| c.voice = "am_fenrir".into())
            .expect("edit succeeds");
        assert_eq!(
            m.flush(Duration::from_secs(5)),
            Ok(()),
            "flush must report success for a write that lands"
        );

        let (on_disk, err) = Config::load_from(&path);
        assert_eq!(err, None);
        assert_eq!(
            on_disk.voice, "am_fenrir",
            "flush must have written the edit through, not merely waited"
        );
        engine.shutdown();
    }

    /// `flush` with nothing owed must return immediately rather than wait
    /// out `timeout` -- called from the shutdown path on every exit,
    /// including the overwhelmingly common case where the settings window
    /// was never touched, this must never add a fixed delay to quitting.
    #[test]
    fn flush_with_nothing_pending_is_a_fast_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart"], dir.path());
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::new(store, models, Config::default());

        let start = std::time::Instant::now();
        assert_eq!(m.flush(Duration::from_secs(5)), Ok(()));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "a no-op flush must not wait out the timeout: took {:?}",
            start.elapsed()
        );
        engine.shutdown();
    }

    /// A flush that cannot reach disk must say so, not report success for a
    /// change that was in fact lost -- this is what lets the shutdown path
    /// (`settings::flush_pending` in `mod.rs`) decide whether to warn.
    #[test]
    fn flush_reports_a_write_failure_rather_than_claiming_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart", "am_fenrir"], dir.path());
        let engine = EngineHandle::spawn(
            Config::default(),
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        // Same trick as `a_failed_write_is_reported_and_does_not_change_the_
        // model`: a path whose parent is a file, not a directory, so
        // `save_to` fails for a reason that needs no permission games.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("blocker");
        let store = Arc::new(ConfigStore::new(
            blocker.join("config.toml"),
            engine.clone(),
            Config::default(),
        ));
        let m = SettingsModel::new(store, models, Config::default());

        m.edit(|c| c.voice = "am_fenrir".into())
            .expect("valid, so it is accepted; the disk has not been touched yet");
        let result = m.flush(Duration::from_secs(5));
        assert!(
            result.is_err(),
            "a write that cannot reach disk must not be reported as flushed"
        );
        engine.shutdown();
    }

    /// IMPORTANT 3: an unrecognised `model` is not something the reload
    /// path may refuse -- the user has already written it -- so it says
    /// what it will run instead. Before this it said nothing at all and the
    /// daemon ran fp32 while the file (and `say status`) claimed int4.
    #[test]
    fn normalize_names_an_unknown_model_and_what_will_run_instead() {
        let mut cfg = Config {
            model: "int4".into(),
            ..Config::default()
        };
        let warnings = normalize(&mut cfg);
        assert_eq!(cfg.model, FALLBACK_MODEL, "the config must say what runs");
        assert_eq!(warnings.len(), 1, "one field, one warning: {warnings:?}");
        assert!(
            warnings[0].contains("int4"),
            "the rejected value must be named: {warnings:?}"
        );
        assert!(
            warnings[0].contains(FALLBACK_MODEL),
            "and what will actually be used: {warnings:?}"
        );
    }

    /// Finding 9: the engine clamps `speed` on `ApplyConfig` but the file
    /// keeps its out-of-range value, so `say status` and MPRIS disagree with
    /// the file indefinitely. Nothing said so; now the same warning that
    /// covers the model covers the clamp.
    #[test]
    fn normalize_reports_the_speed_clamp_the_engine_would_apply_silently() {
        let mut cfg = Config {
            speed: 9.0,
            ..Config::default()
        };
        let warnings = normalize(&mut cfg);
        assert!((cfg.speed - SPEED_MAX).abs() < f32::EPSILON);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains('9'), "{warnings:?}");
        assert!(warnings[0].contains('2'), "{warnings:?}");

        let mut zero_threads = Config {
            threads: 0,
            ..Config::default()
        };
        let warnings = normalize(&mut zero_threads);
        assert_eq!(zero_threads.threads, 1);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    /// The overwhelmingly common case: a config the daemon can honour
    /// exactly must come through untouched and silent, or every reload
    /// would cry wolf.
    #[test]
    fn normalize_leaves_an_honourable_config_alone() {
        let mut cfg = Config {
            voice: "am_fenrir".into(),
            speed: 1.25,
            model: "q8".into(),
            threads: 4,
            ..Config::default()
        };
        let before = cfg.clone();
        assert!(normalize(&mut cfg).is_empty());
        assert_eq!(cfg, before);
    }

    /// The premise of normalising an unknown model to `FALLBACK_MODEL`: that
    /// is the file the synthesizer was going to load for it anyway. If
    /// `model_file_for`'s catch-all ever changed, normalising to fp32 would
    /// start writing a *different* lie into the running config.
    #[test]
    fn the_fallback_model_is_the_one_the_synthesizer_actually_loads() {
        use crate::kokoro_synth::model_file_for;
        assert_eq!(model_file_for("int4"), model_file_for(FALLBACK_MODEL));
        assert_eq!(
            FALLBACK_MODEL,
            Config::default().model,
            "the fallback must also be what a config that says nothing gets"
        );
    }

    /// The asymmetry between the two paths, stated as a test: the window
    /// refuses (it is about to write the value), the reload path does not
    /// (the user already wrote it).
    #[test]
    fn the_window_refuses_the_model_the_reload_path_normalizes() {
        let mut for_window = Config {
            model: "int4".into(),
            ..Config::default()
        };
        assert!(validate(&mut for_window).is_err());
        let mut from_file = Config {
            model: "int4".into(),
            ..Config::default()
        };
        assert!(!normalize(&mut from_file).is_empty());
        assert_eq!(from_file.model, FALLBACK_MODEL);
    }

    /// IMPORTANT 3, the part the user actually hits: a file the window
    /// would refuse used to lock every row of the window. `edit` seeds from
    /// the store, `validate` rejects the whole config, and touching *Speed*
    /// toasted "'int4' is not a model this build knows" -- nine settings
    /// unreachable over a field the user never touched. Normalising on the
    /// reload path means the store holds a config the window can build on.
    #[test]
    fn an_unrunnable_model_in_the_file_does_not_lock_the_window_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart"], dir.path());
        let path = dir.path().join("config.toml");
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::new(store.clone(), models, Config::default());

        // Hand-written, as a user would: `model_file_for` will fall through
        // to fp32 for this.
        // `speed` differs from the stamp so the reload is a genuine
        // `Applied` rather than an echo: normalising `int4` to fp32 makes
        // the rest of this file identical to the default the store was
        // seeded with.
        std::fs::write(&path, "model = \"int4\"\nspeed = 1.25\n").expect("hand edit");
        assert_eq!(store.reload(), ReloadOutcome::Applied);

        let after = m.edit(|c| c.speed = 1.5).expect(
            "an edit to an unrelated row must not be refused over the file's unrunnable model",
        );
        assert_eq!(after.model, FALLBACK_MODEL, "and it writes what is running");
        settled(&m);
        let (on_disk, err) = Config::load_from(&path);
        assert_eq!(err, None);
        assert_eq!(on_disk.speed, 1.5);
        engine.shutdown();
    }

    /// Finding 6: `refresh` runs on the glib main thread, and
    /// `ConfigStore::save` holds the store's stamp across a temp-write and
    /// rename with no timeout anywhere on the path -- so taking that stamp
    /// while a write is in flight freezes the UI for the length of a disk
    /// write. `edit` is routed around it; `refresh` was not. "Did not take
    /// the lock" is invisible in the value returned, so the store counts the
    /// reads (see `ConfigStore::stamp_reads`).
    #[test]
    fn refresh_does_not_touch_the_store_while_a_write_is_in_flight() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart"], dir.path());
        let (store, engine) = store_in(dir.path());
        // No writer thread, so the pending write below stays pending for
        // the whole test rather than racing the 250ms debounce.
        let m = SettingsModel::without_writer(store.clone(), models, Config::default());

        let owed = Config {
            speed: 1.5,
            ..Config::default()
        };
        lock(&m.shared.pending).write = Some(owed.clone());

        let before = store.stamp_reads();
        let shown = m.refresh();
        assert_eq!(
            store.stamp_reads(),
            before,
            "refresh must not take the store's stamp while a write is in flight"
        );
        assert_eq!(shown, owed, "and must show the newer, pending value");

        // With nothing owed it must still re-read, which is what it is for.
        lock(&m.shared.pending).write = None;
        let _ = m.refresh();
        assert_eq!(store.stamp_reads(), before + 1);
        engine.shutdown();
    }

    /// Finding 8: a writer thread that could not be started used to be
    /// swallowed by `.ok()`. Every edit was then accepted and shown to the
    /// user as applied, `pending.write` was set and never drained, and
    /// nothing was ever written.
    #[test]
    fn an_edit_is_refused_when_there_is_no_writer_thread() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart", "am_fenrir"], dir.path());
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::without_writer(store, models, Config::default());

        let err = m
            .edit(|c| c.voice = "am_fenrir".into())
            .expect_err("an edit that can never be written must not be reported as applied");
        assert!(!err.is_empty());
        assert_eq!(
            m.current().voice,
            "af_heart",
            "and must not change what the window shows"
        );
        engine.shutdown();
    }

    /// The other half of finding 8: the shutdown flush must not spend its
    /// whole timeout waiting for a thread that is not running.
    #[test]
    fn flush_does_not_wait_for_a_writer_thread_that_is_not_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart"], dir.path());
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::without_writer(store, models, Config::default());
        lock(&m.shared.pending).write = Some(Config::default());

        let start = std::time::Instant::now();
        assert!(
            m.flush(Duration::from_secs(5)).is_err(),
            "a write that cannot happen must not be reported as flushed"
        );
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "and must not wait out the timeout: took {:?}",
            start.elapsed()
        );
        engine.shutdown();
    }

    /// What the window's Add button does, for every name worth pressing it
    /// with. All of it is decided here because the window cannot be tested.
    #[test]
    fn adding_to_the_allowlist_trims_and_ignores_empty_and_duplicate_names() {
        // (what the list holds, what the user typed, what the list must hold)
        let cases: [(&[&str], &str, &[&str]); 8] = [
            (&[], "Signal", &["Signal"]),
            (&["Signal"], "Fractal", &["Signal", "Fractal"]),
            // Nothing typed: the button is a no-op rather than an entry the
            // policy layer would then read as "no name at all".
            (&[], "", &[]),
            (&[], "   ", &[]),
            // Already there, in every casing `is_allowed` would match.
            (&["Signal"], "Signal", &["Signal"]),
            (&["Signal"], "signal", &["Signal"]),
            (&["Signal"], "  SIGNAL  ", &["Signal"]),
            // Trimmed on the way in, so the file holds what a human would
            // have written.
            (&["Signal"], "  Fractal  ", &["Signal", "Fractal"]),
        ];
        for (before, typed, after) in cases {
            let mut cfg = Config::default();
            cfg.notifications.allow = before.iter().map(|s| (*s).to_string()).collect();
            allow_add(&mut cfg, typed);
            assert_eq!(
                cfg.notifications.allow, after,
                "adding {typed:?} to {before:?}"
            );
        }
    }

    /// What each row's Remove button does.
    #[test]
    fn removing_from_the_allowlist_takes_every_entry_that_matched() {
        let cases: [(&[&str], &str, &[&str]); 5] = [
            (&["Signal", "Fractal"], "Signal", &["Fractal"]),
            (&["Signal", "Fractal"], "signal", &["Fractal"]),
            // Both, or the application keeps speaking with no row left to
            // stop it: `is_allowed` matches either spelling.
            (&["Signal", "signal"], "Signal", &[]),
            // A name that is not on the list changes nothing.
            (&["Signal"], "Fractal", &["Signal"]),
            (&["Signal"], "", &["Signal"]),
        ];
        for (before, removed, after) in cases {
            let mut cfg = Config::default();
            cfg.notifications.allow = before.iter().map(|s| (*s).to_string()).collect();
            allow_remove(&mut cfg, removed);
            assert_eq!(
                cfg.notifications.allow, after,
                "removing {removed:?} from {before:?}"
            );
        }
    }

    /// The window's duplicate rule and the policy layer's match rule have to
    /// be the same rule. If they drift, the window either refuses a name the
    /// daemon would never have matched or accepts a second entry that does
    /// nothing -- so this pins `allow_add` against the code that actually
    /// reads the list rather than against a second copy of its wording.
    #[test]
    fn an_entry_the_model_adds_is_one_the_policy_layer_matches() {
        use crate::notify::policy::{Decision, Limiter};
        use crate::notify::Notification;

        let mut cfg = Config::default();
        cfg.notifications.enabled = true;
        allow_add(&mut cfg, "  Signal  ");

        let notification = Notification {
            app_name: "signal".into(),
            summary: "Ada: dinner?".into(),
            body: String::new(),
        };
        let mut limiter = Limiter::new();
        assert_ne!(
            limiter.decide(&notification, &cfg.notifications, std::time::Instant::now()),
            Decision::NotAllowed,
            "a name the window added must be one the daemon speaks for"
        );

        // And so the other casing is a duplicate rather than a second entry
        // that would change nothing.
        allow_add(&mut cfg, "signal");
        assert_eq!(cfg.notifications.allow, ["Signal"]);
    }

    /// The allowlist is the one field the window edits as a *list*, so it is
    /// the one with an obvious wrong implementation: hold the list the rows
    /// were drawn from and write it back with one entry more. `edit` seeds
    /// from the file, and `allow_add` mutates that seed, so an entry added by
    /// hand while the window sat open survives the next Add.
    #[test]
    fn an_allowlist_add_does_not_drop_an_entry_added_outside_the_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart"], dir.path());
        let path = dir.path().join("config.toml");
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::new(store.clone(), models, Config::default());

        let mut hand_edited = Config::default();
        hand_edited.notifications.enabled = true;
        hand_edited.notifications.allow = vec!["Fractal".into()];
        hand_edited.save_to(&path).expect("hand edit");
        assert_eq!(store.reload(), ReloadOutcome::Applied);

        m.edit(|c| allow_add(c, "Signal")).expect("edit succeeds");
        settled(&m);

        let (on_disk, err) = Config::load_from(&path);
        assert_eq!(err, None);
        assert_eq!(
            on_disk.notifications.allow,
            ["Fractal", "Signal"],
            "the hand-added entry must survive the window's add"
        );
        engine.shutdown();
    }

    /// The cooldown row must be able to show the value a fresh config has,
    /// or the very first window opened on a default config would report its
    /// own default as out of the range it offers (see `Spin::show`).
    #[test]
    fn the_cooldown_row_can_express_the_default_cooldown() {
        let cooldown = Config::default().notifications.cooldown_secs as f64;
        assert!(
            (COOLDOWN_MIN..=COOLDOWN_MAX).contains(&cooldown),
            "the default cooldown {cooldown} is outside {COOLDOWN_MIN}-{COOLDOWN_MAX}"
        );
        assert_eq!(
            COOLDOWN_MIN, 0.0,
            "0 is a setting -- no rate limiting at all -- not a floor to be raised"
        );
    }

    /// The Model row's inline text is spec'd verbatim; the window renders
    /// whatever this table says.
    #[test]
    fn the_model_table_carries_the_measured_tradeoffs() {
        let joined: String = MODELS.iter().map(|(v, d)| format!("{v}{d}")).collect();
        assert!(joined.contains("fp32") && joined.contains("4.78"));
        assert!(joined.contains("fp16") && joined.contains("4.66"));
        assert!(joined.contains("q8") && joined.contains("1.40"));
    }
}
