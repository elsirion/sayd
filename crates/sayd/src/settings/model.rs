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
    /// `None` only after `Drop` has taken it to join.
    writer: Option<std::thread::JoinHandle<()>>,
}

impl SettingsModel {
    pub fn new(store: Arc<ConfigStore>, models_dir: PathBuf, current: Config) -> Self {
        let shared = Arc::new(Shared {
            store,
            pending: Mutex::new(Pending {
                current,
                write: None,
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
            std::thread::Builder::new()
                .name("settings-writer".into())
                .spawn(move || write_loop(&shared))
                .ok()
        };
        SettingsModel {
            shared,
            voices: list_voices(&models_dir),
            writer,
        }
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
        // Not under `pending`: `store.current()` takes the store's stamp,
        // and the writer thread takes that stamp and then `pending`. Taking
        // them in that order here too is what keeps the two from being a
        // lock cycle.
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
        let seed = lock(&self.shared.pending).write.clone();
        let mut next = match seed {
            Some(pending) => pending,
            None => self.shared.store.current(),
        };
        f(&mut next);
        validate(&mut next)?;

        let mut p = lock(&self.shared.pending);
        p.current = next.clone();
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
        let stop = p.stop;
        drop(p);

        let outcome = shared.store.save(&cfg);
        shared.writes.fetch_add(1, Ordering::Relaxed);

        match outcome {
            Ok(()) => {
                let mut p = lock(&shared.pending);
                // Only if nothing arrived while we were writing; if
                // something did, it is a newer config and still owed.
                if p.write.as_ref() == Some(&cfg) {
                    p.write = None;
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
