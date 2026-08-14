//! The config file as a live, two-way surface.
//!
//! Two directions meet here and must not fight:
//!
//! - **Write-through.** The settings window changes a value; it lands in
//!   `config.toml` and in the running engine.
//! - **Reload.** Someone edits `config.toml` by hand; it lands in the
//!   running engine.
//!
//! Both go through `Command::ApplyConfig`, so there is one place where a
//! config becomes behaviour. The hazard is the loop between them: our own
//! atomic write fires the same inotify event a hand edit does. `save`
//! records the exact config it wrote and `reload` drops any load that
//! matches it -- comparing content rather than timestamps, because the
//! temp+rename write arrives as a create/rename on the destination and
//! because an editor may write identical bytes back.
//!
//! Applying a config is not free -- `ApplyConfig` can drop and rebuild the
//! ~1.27 GB ORT session -- so events are debounced rather than acted on one
//! by one; see [`DEBOUNCE`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::event::{EventKind, ModifyKind};
use notify::{Event, RecursiveMode, Watcher};
use sayd_core::config::Config;
use sayd_core::engine::Command;
use sayd_core::handle::EngineHandle;

/// What one `reload` attempt did, so callers (and tests) can tell an
/// applied edit from a suppressed echo without reading logs.
#[derive(Debug, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// An external edit was parsed and sent to the engine.
    Applied,
    /// The file matches what we last wrote: our own echo.
    OwnWrite,
    /// The file is gone. Nothing to apply.
    Missing,
    /// The file exists but does not parse. The running config is kept.
    Failed(String),
}

pub struct ConfigStore {
    path: PathBuf,
    engine: EngineHandle,
    last_written: Mutex<Config>,
    applied_reloads: AtomicUsize,
}

impl ConfigStore {
    /// `running` is the config the engine was spawned with -- i.e. what the
    /// file said at startup. It is the stamp's first value because the
    /// engine and the file already agree at that point: without it the
    /// very first write of identical bytes (which is what most editor saves
    /// are) looks like an external change and is applied as one.
    pub fn new(path: PathBuf, engine: EngineHandle, running: Config) -> Self {
        ConfigStore {
            path,
            engine,
            last_written: Mutex::new(running),
            applied_reloads: AtomicUsize::new(0),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The config the file most recently held, as far as the daemon knows.
    ///
    /// `save` stamps this *before* every write and `reload` re-stamps it
    /// whenever it applies an external edit, so this is the freshest content
    /// either direction has produced -- ours or a hand edit -- without a
    /// re-read of the file. That makes it the right seed for a caller that
    /// wants to build on top of "whatever the file currently says" (the
    /// settings model's `edit`, in particular): no re-read means no TOCTOU
    /// against the write it is about to do, and no race with the debounce
    /// thread's own read.
    ///
    /// There is no "we do not know" state to represent: the stamp is seeded
    /// at construction with the config the engine was spawned with, and a
    /// failed `save` restores what it displaced rather than clearing it (see
    /// `save`). A `None` here would be worse than useless to the caller that
    /// needs it most -- seeding an edit from `Config::default()` after a
    /// failed write would turn the next successful write into a reset of
    /// every setting the user has.
    pub fn current(&self) -> Config {
        self.stamp().clone()
    }

    /// The stamp, taken poison-tolerantly.
    ///
    /// Every caller goes through here rather than `.lock().expect(...)`, and
    /// the reason is not tidiness. A panic anywhere under this lock -- the
    /// `save_to` or `load_str` calls below are the candidates -- poisons the
    /// mutex permanently, and every later `expect` then panics too. Before
    /// there was a settings window that cost a dead debounce thread and a
    /// silently stopped watch. It now costs the whole daemon: `save` and
    /// `current` are reached from GTK signal handlers, and glib invokes
    /// those through an `extern "C"` frame, so a panic there is a
    /// *non-unwinding* panic -- measured, it aborts the process outright
    /// ("panic in a function that cannot unwind ... aborting"), with no
    /// shutdown, no exit code, and immune to a `catch_unwind` in the frame
    /// below. Mid-utterance, on a click on any settings row.
    ///
    /// A poisoned stamp is also not meaningfully corrupt: it is one `Config`
    /// replaced wholesale under the lock, so the worst an observer can see
    /// is the value from before the panicking write -- exactly what the
    /// failure path restores anyway. Same reasoning, and the same
    /// `into_inner()` shape, as `EngineHandle::snapshot`.
    fn stamp(&self) -> std::sync::MutexGuard<'_, Config> {
        match self.last_written.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Write `cfg` to disk and apply it to the engine.
    ///
    /// The stamp is taken *before* the write: the watcher thread can observe
    /// the file the instant `save_to` renames it, and a stamp written
    /// afterwards would lose that race and let our own write bounce back as
    /// an external change.
    ///
    /// The stamp's lock is what makes a save and a reload one at a time --
    /// see `reload`.
    pub fn save(&self, cfg: &Config) -> Result<(), String> {
        let mut stamp = self.stamp();
        let displaced = std::mem::replace(&mut *stamp, cfg.clone());
        if let Err(e) = cfg.save_to(&self.path) {
            // `save_to` writes a temp file and renames it, and every failure
            // path fails before the rename -- so the destination still holds
            // exactly what it held before, which is what `displaced`
            // describes. Restoring it keeps own-write suppression correct for
            // the write *before* this one, and keeps `current` returning the
            // file's real content rather than a config nobody chose.
            *stamp = displaced;
            return Err(format!("could not write {}: {e}", self.path.display()));
        }
        self.engine.send(Command::ApplyConfig(cfg.clone()));
        Ok(())
    }

    /// Read the file and apply it unless it is our own echo.
    ///
    /// The stamp's lock is held across the whole of read, compare and
    /// stamp -- and across `save`'s whole write -- so that the two cannot
    /// interleave. Taking it only for the compare left this window: a
    /// reload reads config A, a save writes B and sends `ApplyConfig(B)`,
    /// and the reload then finds A different from the stamp and sends
    /// `ApplyConfig(A)` *after* it. The engine ends up running a config the
    /// disk no longer holds, and stays there until some later event happens
    /// to arrive -- with a model change in play, each of those steps costs a
    /// session unload. A settings window saving once per slider tick hits
    /// exactly this. Holding the lock across both makes the order commands
    /// reach the engine the order the file actually changed; the sends
    /// underneath it are non-blocking channel pushes, so the lock is never
    /// held waiting on the engine -- but it *is* held across the file write
    /// itself: `save`'s `save_to` call is disk I/O with no bound this
    /// module puts on it. `save` is what the settings window's writes end up
    /// in, so it must not be called from a thread that cannot block on disk
    /// (a UI event-loop thread, say) -- doing so would stall every reload
    /// for as long as that write takes, and freeze the UI with it. That is
    /// why `SettingsModel` owns a writer thread and never calls this from
    /// the glib main thread; see `SettingsModel::edit`.
    ///
    /// A panic inside `save_to` or `load_str` while this lock is held still
    /// poisons the mutex, but no longer propagates: every taker goes through
    /// `stamp`, which reads through the poison. See its doc comment for why
    /// that is both safe here and now mandatory.
    ///
    /// Reads the file exactly once, deciding everything from those bytes
    /// rather than from a separate `exists()` check: checking existence and
    /// then loading is two syscalls with a gap between them, and a delete
    /// landing in that gap used to make `load_from`'s own `NotFound`
    /// fallback -- `(Config::default(), None)`, indistinguishable from an
    /// empty file -- read as a real load rather than as `Missing`. An empty
    /// or whitespace-only file is also treated as `Missing` rather than
    /// parsed; see the comment on [`DEBOUNCE`] for why that is safe and
    /// where its limit is.
    pub fn reload(&self) -> ReloadOutcome {
        let mut stamp = self.stamp();
        let txt = match std::fs::read_to_string(&self.path) {
            Ok(txt) => txt,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ReloadOutcome::Missing,
            Err(e) => return ReloadOutcome::Failed(format!("{}: {e}", self.path.display())),
        };
        if txt.trim().is_empty() {
            return ReloadOutcome::Missing;
        }
        let (cfg, err) = Config::load_str(&txt);
        if let Some(reason) = err {
            // Deliberately not applying `cfg` here: `load_str` returns
            // defaults alongside the error, and applying those would reset
            // every setting the user has because of one typo.
            return ReloadOutcome::Failed(format!("{}: {reason}", self.path.display()));
        }
        if *stamp == cfg {
            return ReloadOutcome::OwnWrite;
        }
        *stamp = cfg.clone();
        self.applied_reloads.fetch_add(1, Ordering::Relaxed);
        self.engine.send(Command::ApplyConfig(cfg));
        ReloadOutcome::Applied
    }

    /// How many reloads have reached the engine.
    ///
    /// Test-only, and not a nicety: "one edit became one apply" is invisible
    /// in the engine's final state, and the cost the debounce exists to
    /// avoid -- a session teardown and a rebuild of over a second -- is paid
    /// per apply, not per final state.
    #[cfg(test)]
    fn applied_reloads(&self) -> usize {
        self.applied_reloads.load(Ordering::Relaxed)
    }
}

/// Does this event mean the file's *contents* may have changed?
///
/// Filtering on the kind matters far more than it looks. notify's inotify
/// backend puts `IN_OPEN` in its watch mask, and the first thing a reload
/// does is open the file -- so a watcher that reloads on any event feeds
/// itself its own event, as fast as the machine can read a small TOML file
/// (measured: ~101k events/s, ~91% of a core, indefinitely, after a single
/// edit). It is invisible by hand because every self-triggered reload is
/// suppressed as an `OwnWrite` and logs nothing.
///
/// `Modify(Metadata)` (`IN_ATTRIB`, from `touch`/`chmod`) is dropped for a
/// related reason: `SetVoice`/`SetSpeed`/`SetMuted` change the engine
/// without touching the file, so *any* reload re-applies the file over
/// them. Muting from the tray and then merely reading -- or touching --
/// `config.toml` would unmute the daemon and let it speak.
///
/// The atomic temp+rename write still arrives: notify maps `IN_MOVED_TO`
/// to `Modify(Name(To))` and, paired with the temp file's `IN_MOVED_FROM`,
/// also emits `Modify(Name(Both))`; both carry the destination path, so
/// own-write suppression still sees the write it has to suppress.
fn is_content_change(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_))
    )
}

/// How quiet the file must be before an edit is applied.
///
/// A write is not one event. `cat > config.toml` truncates on the first
/// keystroke and grows a line at a time; an editor unlinks and recreates;
/// `save_to` renames over the file. Debouncing coalesces the burst of
/// events one edit produces into a single reload of whatever the writer
/// finished with, instead of one reload per intermediate state.
///
/// The window alone is not a complete fix: a human typing into `cat >`, or
/// a script that truncates and then computes before writing the real
/// content, can pause longer than any window this module could pick
/// without a normal edit starting to feel laggy. `reload` closes the gap
/// from the read side too, by treating an empty or whitespace-only file as
/// `Missing` rather than parsing it. An earlier version of this comment
/// argued that no emptiness test could tell a mid-truncate file from a user
/// who genuinely wants defaults -- true of a test on the *parsed* result
/// (`== Config::default()`: a config with every field left at its default
/// is indistinguishable from one that says nothing), but not of a test on
/// the *raw bytes* before parsing. Nobody means an empty file: a user who
/// wants defaults either deletes the config (already `Missing`) or writes
/// explicit default values. The honest limit is the partial-but-valid
/// prefix -- `cat >` after the first line has landed but before the rest
/// has -- which is not empty, still parses, and still gets applied,
/// resetting whichever fields the writer has not reached yet to their
/// defaults. That is a much smaller blast radius than the full-default
/// reset a bare truncate used to produce (every field, not just the
/// untyped ones), but it is not zero; the debounce window above is what
/// keeps it rare rather than routine.
///
/// The cost of getting either of these wrong is not a blip. `ApplyConfig`
/// reconfigures the synthesizer and drops the ~1.27 GB ORT session when
/// `model` moves, so an edit that never mentioned `model` would pay for a
/// teardown and a rebuild of over a second, twice -- and on a machine that
/// has only the `q8` model installed, the rebuild for the *default* `fp32`
/// fails into a sticky synth error that rejects every later submission.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// What the notify callback tells the reload thread.
enum Tick {
    /// The watched file may have changed.
    Changed,
    /// The watched directory went away, taking the watch with it.
    Rewatch,
    /// notify reported a failure rather than an event.
    Failed(String),
    /// The [`ConfigWatcher`] was dropped; wind up.
    Stop,
}

/// A running config watch. Dropping it stops the watch.
///
/// The `notify` watcher itself lives on the reload thread rather than in
/// here, so this handle asks that thread to finish instead of dropping the
/// watcher directly.
pub struct ConfigWatcher {
    tx: Sender<Tick>,
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        // Nothing to do if the thread is already gone: the watch died with
        // it.
        let _ = self.tx.send(Tick::Stop);
    }
}

/// Watch the config file's directory and reload on change.
///
/// The *directory* is watched, not the file: an atomic temp+rename replaces
/// the inode, so a watch on the file itself stops seeing events after the
/// first write, and a config that does not exist yet cannot be watched at
/// all.
///
/// The returned handle must be kept alive for the watch to stay active --
/// dropping it silently stops the reload.
pub fn spawn(store: Arc<ConfigStore>) -> Result<ConfigWatcher, String> {
    let dir = store
        .path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(format!("could not create {}: {e}", dir.display()));
    }

    let (tx, rx) = mpsc::channel();
    let watched = store.path().to_path_buf();
    let watched_dir = dir.clone();
    let events = tx.clone();
    // Deliberately no work in here beyond classifying the event: this runs
    // on notify's own event-loop thread, which must stay free to drain the
    // inotify queue -- and which cannot call `watch` on its own watcher
    // without deadlocking on itself.
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        let event = match res {
            Ok(event) => event,
            // Not only per-event problems arrive here: notify reports
            // watch-level failures through the same channel. Swallowing
            // them left a dead watch indistinguishable from a quiet one.
            Err(e) => {
                let _ = events.send(Tick::Failed(e.to_string()));
                return;
            }
        };
        // The watch is on the directory, so losing the directory loses the
        // watch: inotify's `IN_DELETE_SELF`/`IN_MOVE_SELF` arrive as a
        // remove or a rename *of the directory itself*, and after them the
        // old inode's watch reports nothing, ever. A dotfile manager
        // replacing `~/.config/sayd` does exactly this, and every hand edit
        // after it was ignored silently.
        if event.paths.contains(&watched_dir)
            && matches!(
                event.kind,
                EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
            )
        {
            let _ = events.send(Tick::Rewatch);
            return;
        }
        // The kernel dropped events we will never see (inotify's queue
        // overflowed). We cannot know whether the file was among them, so
        // assume it was.
        if event.need_rescan() {
            let _ = events.send(Tick::Changed);
            return;
        }
        if !is_content_change(&event.kind) || !event.paths.contains(&watched) {
            return;
        }
        let _ = events.send(Tick::Changed);
    })
    .map_err(|e| format!("could not create a config watcher: {e}"))?;

    // Started before the thread so that a watch we cannot establish is
    // reported to the caller as an error rather than dying silently in a
    // thread it cannot see.
    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .map_err(|e| format!("could not watch {}: {e}", dir.display()))?;

    std::thread::Builder::new()
        .name("sayd-config-watch".into())
        .spawn(move || {
            // The watcher lives here rather than in the returned handle:
            // re-arming a lost watch means calling `watch` from somewhere
            // that is not notify's event-loop thread, and this is the only
            // such place that outlives the call to `spawn`.
            let mut watcher = watcher;
            debounce_loop(&store, &rx, &mut watcher, &dir);
        })
        .map_err(|e| format!("could not start the config watch thread: {e}"))?;

    Ok(ConfigWatcher { tx })
}

/// Coalesce ticks and reload once the file has been quiet for [`DEBOUNCE`].
///
/// "Quiet" is tracked as an explicit deadline, not as a `recv_timeout`
/// restarted on every message. Only `Changed`/`Rewatch` -- an actual sign
/// the file may be mid-edit -- push the deadline out; `Failed` is drained
/// but never does. A naive "restart the timeout on any `Ok`" version
/// starves indefinitely under a persistent error stream: notify's inotify
/// loop re-reads after a failed non-`WouldBlock` read without pausing, so
/// such a stream can arrive faster than DEBOUNCE for as long as the
/// underlying problem lasts, and a pending edit would then never see its
/// quiet window expire -- silently, since the log cap means only the first
/// two of those errors are ever printed.
fn debounce_loop(
    store: &ConfigStore,
    rx: &mpsc::Receiver<Tick>,
    watcher: &mut notify::RecommendedWatcher,
    dir: &Path,
) {
    // The sender is held by the callback, which this thread owns through
    // the watcher, so the channel never disconnects on its own: `Stop` from
    // the dropped handle is the only way out.
    let mut errors = 0usize;
    loop {
        // Block for a tick that starts a pending edit. A lone `Failed`
        // tick (nothing pending yet) loops back here rather than starting
        // a quiet window over nothing.
        let mut deadline = loop {
            match rx.recv() {
                Ok(tick) => match handle(tick, watcher, dir, &mut errors) {
                    Next::Reload => break Instant::now() + DEBOUNCE,
                    Next::Wait => continue,
                    Next::Stop => return,
                },
                Err(_) => return,
            }
        };
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match rx.recv_timeout(deadline - now) {
                // Still being written -- but only `Changed`/`Rewatch` push
                // the deadline out; a `Failed` tick is drained in place so
                // it cannot hold the window open forever.
                Ok(tick) => match handle(tick, watcher, dir, &mut errors) {
                    Next::Reload => deadline = Instant::now() + DEBOUNCE,
                    Next::Wait => {}
                    Next::Stop => return,
                },
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        match store.reload() {
            ReloadOutcome::Applied => eprintln!("sayd: reloaded {}", store.path().display()),
            ReloadOutcome::Failed(reason) => {
                eprintln!("warning: {reason}; keeping the running settings");
            }
            // `Missing` is not worth a word: an editor that unlinks and
            // recreates passes through it on the way to a real edit.
            ReloadOutcome::OwnWrite | ReloadOutcome::Missing => {}
        }
    }
}

/// What the debounce loop should do next.
enum Next {
    /// This tick wants the file read.
    Reload,
    /// Dealt with; nothing to read.
    Wait,
    /// Wind up the thread, and the watch with it.
    Stop,
}

/// Act on one tick. `errors` counts the failures reported so far.
fn handle(
    tick: Tick,
    watcher: &mut notify::RecommendedWatcher,
    dir: &Path,
    errors: &mut usize,
) -> Next {
    match tick {
        Tick::Changed => Next::Reload,
        // The file may well have changed while we were blind, so read it
        // once the watch is back.
        Tick::Rewatch => {
            rearm(watcher, dir);
            Next::Reload
        }
        // Said once, not once per failure: notify's inotify loop re-reads
        // after a failed read without pausing, so a persistent one (a
        // buffer too small for a very long name, say) arrives as fast as
        // the thread can produce it, and stderr is a log file on a real
        // machine.
        Tick::Failed(reason) => {
            *errors += 1;
            match *errors {
                1 => eprintln!("warning: config watch error: {reason}"),
                2 => eprintln!("warning: further config watch errors will not be repeated"),
                _ => {}
            }
            Next::Wait
        }
        Tick::Stop => Next::Stop,
    }
}

/// Put the watch back after the directory it was on was removed or renamed.
///
/// This always arms a *new* kernel watch; it never assumes the old one is
/// gone. For a delete (`IN_DELETE_SELF`) and for the `MOVED_FROM` half of a
/// rename, notify's inotify backend has already retired the old watch
/// descriptor itself (`remove_watch_by_event` covers exactly those two). A
/// bare rename of the watched directory -- `IN_MOVE_SELF`, with no paired
/// `MOVED_FROM` because nothing else in the tree moved -- is neither, so
/// the old descriptor stays live under the now-stale path and this call
/// arms a second one under the new path. The result is one leaked inotify
/// watch and a spurious extra wakeup per future event on the old path --
/// not a wedge, since `reload` always reads the real, current path no
/// matter which watch fired. Left as-is rather than engineered around:
/// closing it needs tracking watch descriptors ourselves, which is more
/// machinery than a harmless leak justifies.
///
/// The `create_dir_all` below also means the watch survives its directory
/// being deleted out from under it in a way that is easy to misread as
/// robustness: `rm -rf ~/.config/sayd` does not stay gone. The next tick
/// recreates it as an empty directory (config included -- it was in there)
/// and the watch quietly resumes on nothing.
fn rearm(watcher: &mut notify::RecommendedWatcher, dir: &Path) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!(
            "warning: could not recreate {}: {e}; config changes will need a restart",
            dir.display()
        );
        return;
    }
    if let Err(e) = watcher.watch(dir, RecursiveMode::NonRecursive) {
        eprintln!(
            "warning: could not watch {} again: {e}; config changes will need a restart",
            dir.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sayd_core::audio::VecSink;
    use sayd_core::synth::StubSynthesizer;

    fn engine() -> EngineHandle {
        engine_with(Config::default())
    }

    fn engine_with(cfg: Config) -> EngineHandle {
        EngineHandle::spawn(
            cfg,
            Box::new(StubSynthesizer::new()),
            // Capacity large enough that nothing in this module's tests
            // (which only exercise config save/reload, never real
            // synthesis) could plausibly fill it.
            Box::new(VecSink::new(24_000 * 10)),
        )
    }

    /// The engine runs on its own thread, and the watcher adds an inotify
    /// round trip on top; wait for the value rather than asserting on a
    /// race, and give up at a deadline so a failure says what the voice
    /// actually was.
    fn wait_for_voice(engine: &EngineHandle, want: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while engine.snapshot().voice != want && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(engine.snapshot().voice, want);
    }

    /// Long enough that anything the watcher was going to do has happened,
    /// for the tests that assert nothing happens.
    fn settle() {
        std::thread::sleep(DEBOUNCE * 3);
    }

    /// A failed write must not cost the caller its seed for the next one.
    ///
    /// `save` stamps before it writes, so a failure has to put back what it
    /// displaced. Clearing the stamp instead looks harmless -- the file is
    /// unknown, so claim nothing -- but the settings model seeds every edit
    /// from `current`, so the next edit after a failed one would build on
    /// `Config::default()` and its (successful) write would silently reset
    /// every setting the user has.
    #[test]
    fn a_failed_write_leaves_the_stamp_describing_the_file_not_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = engine();
        // A path whose parent is a regular file cannot be created as a
        // directory, so `save_to` fails without any permission games.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("blocker");
        let running = Config {
            voice: "am_fenrir".into(),
            model: "q8".into(),
            ..Config::default()
        };
        let store = ConfigStore::new(blocker.join("config.toml"), engine.clone(), running.clone());

        let doomed = Config {
            speed: 1.75,
            ..running.clone()
        };
        assert!(store.save(&doomed).is_err(), "the write must fail");
        assert_eq!(
            store.current(),
            running,
            "a failed save must leave the previous config as the seed"
        );
        engine.shutdown();
    }

    /// The write-through path: what the settings window calls. The file on
    /// disk and the running engine must agree afterwards.
    #[test]
    fn save_writes_the_file_and_reaches_the_engine() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let engine = engine();
        let store = ConfigStore::new(path.clone(), engine.clone(), Config::default());

        let cfg = Config {
            voice: "am_fenrir".into(),
            ..Config::default()
        };
        store.save(&cfg).expect("save succeeds");

        let (from_disk, err) = Config::load_from(&path);
        assert_eq!(err, None);
        assert_eq!(from_disk.voice, "am_fenrir");

        // The engine runs on its own thread; give the command a moment to
        // land rather than asserting on a race.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while engine.snapshot().voice != "am_fenrir" && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(engine.snapshot().voice, "am_fenrir");
        engine.shutdown();
    }

    /// A panic under the stamp's lock must not turn every later settings
    /// click into a dead daemon.
    ///
    /// `save` and `current` are reached from GTK signal handlers, which glib
    /// calls through an `extern "C"` frame: a panic there is a
    /// *non-unwinding* panic and aborts the process outright -- measured, no
    /// shutdown, no exit code, and unstoppable by `catch_unwind`. So a
    /// poisoned stamp (from a panic inside `save_to`/`load_str`, the only
    /// non-trivial code this lock is held across) must be read through
    /// rather than propagated, the way `EngineHandle::snapshot` already
    /// does.
    #[test]
    fn a_poisoned_stamp_is_read_through_rather_than_propagated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let engine = engine();
        let store = std::sync::Arc::new(ConfigStore::new(
            path.clone(),
            engine.clone(),
            Config::default(),
        ));

        // Poison it the only way a mutex can be poisoned: panic while
        // holding it. `save_to`/`load_str` panicking under `save`/`reload`
        // is the real-world route to the same state.
        let poisoner = store.clone();
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let _ = std::thread::spawn(move || {
            let _held = poisoner.last_written.lock().expect("lock");
            panic!("deliberate: poisoning the stamp");
        })
        .join();
        std::panic::set_hook(hook);
        assert!(
            store.last_written.is_poisoned(),
            "sanity: the test must actually have poisoned it"
        );

        // Every path that takes the stamp must still work.
        assert_eq!(store.current(), Config::default());
        let cfg = Config {
            voice: "am_fenrir".into(),
            ..Config::default()
        };
        store
            .save(&cfg)
            .expect("save must survive a poisoned stamp");
        assert_eq!(store.current(), cfg);
        assert_eq!(
            store.reload(),
            ReloadOutcome::OwnWrite,
            "own-write suppression must survive it too"
        );
        engine.shutdown();
    }

    /// The suppression this exists for: `save` must not bounce back through
    /// the watcher as an external change.
    #[test]
    fn a_config_we_just_wrote_is_not_treated_as_an_external_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let engine = engine();
        let store = ConfigStore::new(path.clone(), engine.clone(), Config::default());

        let cfg = Config {
            voice: "am_fenrir".into(),
            ..Config::default()
        };
        store.save(&cfg).expect("save succeeds");
        wait_for_voice(&engine, "am_fenrir");

        // Something the file has never said, so that a bounced-back write
        // shows up as the engine losing it. Asserting only on the returned
        // `OwnWrite` would pass just as happily on an implementation that
        // reported the echo and re-applied it anyway.
        engine.send(Command::SetVoice("bm_george".into()));
        wait_for_voice(&engine, "bm_george");

        assert_eq!(
            store.reload(),
            ReloadOutcome::OwnWrite,
            "the write we just made must be recognised as ours"
        );
        settle();
        assert_eq!(
            engine.snapshot().voice,
            "bm_george",
            "our own write must not come back as an edit and undo runtime state"
        );
        engine.shutdown();
    }

    /// At startup the engine is already running what the file says, so an
    /// editor that writes the identical bytes back -- the common case for a
    /// save with no change in it -- is not an edit and must not be applied
    /// as one. That only holds if the store knows what the engine started
    /// with.
    #[test]
    fn the_config_the_engine_started_with_is_not_an_external_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let cfg = Config {
            voice: "am_fenrir".into(),
            ..Config::default()
        };
        cfg.save_to(&path).expect("write");

        let engine = engine_with(cfg.clone());
        let store = ConfigStore::new(path.clone(), engine.clone(), cfg.clone());

        // The bytes an editor would write back unchanged.
        cfg.save_to(&path).expect("rewrite");
        assert_eq!(
            store.reload(),
            ReloadOutcome::OwnWrite,
            "the config the engine started on is not a change to apply"
        );
        engine.shutdown();
    }

    /// A genuine hand edit must reach the engine.
    #[test]
    fn an_external_edit_is_applied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let engine = engine();
        let store = ConfigStore::new(path.clone(), engine.clone(), Config::default());

        std::fs::write(&path, "voice = \"bm_george\"\nspeed = 1.5\n").expect("write");
        assert_eq!(store.reload(), ReloadOutcome::Applied);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while engine.snapshot().voice != "bm_george" && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(engine.snapshot().voice, "bm_george");
        engine.shutdown();
    }

    /// A half-saved or typo'd file must not blow the running settings away.
    /// `Config::load_from` returns defaults plus a reason on a parse error;
    /// applying those defaults would silently reset every setting the user
    /// has, which is far worse than ignoring the edit until it parses.
    #[test]
    fn a_malformed_edit_is_reported_and_the_running_config_is_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let engine = engine();
        let store = ConfigStore::new(path.clone(), engine.clone(), Config::default());

        let cfg = Config {
            voice: "am_fenrir".into(),
            ..Config::default()
        };
        store.save(&cfg).expect("save succeeds");
        // Wait for the engine to actually be on this config before breaking
        // the file: a fixed sleep afterwards would report a slow engine
        // thread as "the malformed edit reset the config".
        wait_for_voice(&engine, "am_fenrir");

        std::fs::write(&path, "voice = [this is not toml").expect("write");
        match store.reload() {
            ReloadOutcome::Failed(reason) => assert!(!reason.is_empty()),
            other => panic!("expected a parse failure, got {other:?}"),
        }

        settle();
        assert_eq!(
            engine.snapshot().voice,
            "am_fenrir",
            "a malformed file must not reset the running config to defaults"
        );
        engine.shutdown();
    }

    /// The inotify half, end to end. Every other test in this module drives
    /// `reload` directly, which leaves the watch itself -- directory versus
    /// file, the path filter, the event kinds, the watcher's lifetime --
    /// entirely uncovered. That gap is what let a self-triggering reload
    /// loop ship.
    #[test]
    fn an_edit_through_the_watcher_reaches_the_engine() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let engine = engine();
        let store = Arc::new(ConfigStore::new(
            path.clone(),
            engine.clone(),
            Config::default(),
        ));
        let _watcher = spawn(store).expect("watcher starts");

        std::fs::write(&path, "voice = \"bm_george\"\n").expect("write");
        wait_for_voice(&engine, "bm_george");
        engine.shutdown();
    }

    /// Saves and reloads race by construction: a settings window emits one
    /// save per slider tick while the watcher reads the file underneath it.
    /// Whatever order they interleave in, the engine must end up on the
    /// config the disk holds -- never on an older one that a reload read
    /// before the newer save and applied after it.
    #[test]
    fn a_reload_racing_saves_cannot_leave_the_engine_on_the_older_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let engine = engine();
        let store = Arc::new(ConfigStore::new(
            path.clone(),
            engine.clone(),
            Config::default(),
        ));

        let older = Config::default();
        let newer = Config {
            voice: "am_fenrir".into(),
            speed: 1.5,
            ..Config::default()
        };

        // Several readers and a long run because the losing interleaving is
        // narrow -- the window is one file read -- and only the *last* one
        // to lose shows up in the final state. A pass here is evidence, not
        // proof: on code with the bug reintroduced, this test has been
        // observed to catch it on roughly half its runs (2 of 4 in one
        // measurement), not every run, because only the very last reload to
        // lose the race leaves a mark that survives to the final
        // assertion -- earlier losers get overwritten by a later winner.
        // 4000 iterations rather than 400 buys more chances for a losing
        // interleaving to be the last one (about half a second here,
        // instead of the ~0.05s the smaller count ran in); it narrows the
        // odds of a false pass, it does not close them to zero.
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let store = store.clone();
                let done = done.clone();
                std::thread::spawn(move || {
                    while !done.load(Ordering::Relaxed) {
                        store.reload();
                    }
                })
            })
            .collect();
        for _ in 0..4_000 {
            store.save(&older).expect("save succeeds");
            store.save(&newer).expect("save succeeds");
        }
        done.store(true, Ordering::Relaxed);
        for reader in readers {
            reader.join().expect("reader thread");
        }

        // Deliberately no reload here: one would paper over the bug by
        // re-reading the file that is already correct.
        wait_for_voice(&engine, "am_fenrir");
        assert_eq!(engine.config().expect("engine answers"), newer);
        engine.shutdown();
    }

    /// A shell redirect is a truncate followed by writes, and every prefix
    /// of a TOML file parses -- into a full default config, since every
    /// field has a serde default. Applying those intermediate states is not
    /// cosmetic: each one can drop and rebuild the ORT session, and the
    /// default `model` may not even be installed. The whole sequence must
    /// land as exactly one apply, of the content the writer finished with.
    #[test]
    fn a_truncate_and_rewrite_is_applied_once_as_its_final_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "voice = \"bm_george\"\nspeed = 1.5\n").expect("write");

        let engine = engine();
        let store = Arc::new(ConfigStore::new(
            path.clone(),
            engine.clone(),
            Config::default(),
        ));
        let _watcher = spawn(store.clone()).expect("watcher starts");

        // What `cat > config.toml` looks like from the outside: empty, then
        // one valid prefix at a time. Spaced out, because that is the case
        // that matters -- an interactive writer takes far longer between
        // lines than a burst of `write` calls, long enough that each state
        // is comfortably observable.
        let between = DEBOUNCE / 5;
        std::fs::write(&path, "").expect("truncate");
        std::thread::sleep(between);
        std::fs::write(&path, "voice = \"am_fenrir\"\n").expect("write");
        std::thread::sleep(between);
        std::fs::write(&path, "voice = \"am_fenrir\"\nspeed = 1.5\n").expect("write");

        wait_for_voice(&engine, "am_fenrir");
        settle();
        assert_eq!(
            engine.config().expect("engine answers").speed,
            1.5,
            "the content the writer finished with must be what is applied"
        );
        assert_eq!(
            store.applied_reloads(),
            1,
            "the truncate and its partial prefixes must not each be applied"
        );
        engine.shutdown();
    }

    /// The daemon holds the handle for the life of the process because
    /// dropping it stops the watch. That is only true if the thread the
    /// watcher now lives on actually winds up with it.
    #[test]
    fn dropping_the_handle_stops_the_watch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let engine = engine();
        let store = Arc::new(ConfigStore::new(
            path.clone(),
            engine.clone(),
            Config::default(),
        ));
        let watcher = spawn(store).expect("watcher starts");
        drop(watcher);

        std::fs::write(&path, "voice = \"bm_george\"\n").expect("write");
        settle();
        assert_eq!(
            engine.snapshot().voice,
            Config::default().voice,
            "a dropped watcher must not still be reloading"
        );
        engine.shutdown();
    }

    /// A dotfile manager that replaces `~/.config/sayd` wholesale takes the
    /// watched directory's inode with it, and inotify has nothing more to
    /// say about the old one. Without re-arming, this is permanent and
    /// silent: every hand edit for the rest of the process is ignored.
    #[test]
    fn a_replaced_config_directory_is_watched_again() {
        let home = tempfile::tempdir().expect("tempdir");
        let dir = home.path().join("sayd");
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).expect("mkdir");

        let engine = engine();
        let store = Arc::new(ConfigStore::new(
            path.clone(),
            engine.clone(),
            Config::default(),
        ));
        let _watcher = spawn(store).expect("watcher starts");

        std::fs::remove_dir_all(&dir).expect("remove the directory");
        std::fs::create_dir_all(&dir).expect("and put a fresh one back");
        // Let the re-watch land before editing: this test is about the
        // watch surviving, not about racing it.
        settle();

        std::fs::write(&path, "voice = \"bm_george\"\n").expect("write");
        wait_for_voice(&engine, "bm_george");
        engine.shutdown();
    }

    /// Regression for the reload loop, and for what it costs.
    ///
    /// `reload` opens the file, and notify's inotify mask includes
    /// `IN_OPEN`, so an unfiltered watcher re-enters itself on its own read
    /// and never stops. Reading the file from outside is the cheapest probe
    /// for that: it produces exactly the event the reload's own
    /// `read_to_string` produces. The damage is asserted rather than the
    /// event count, because that is what a user sees -- `SetVoice` never
    /// touches the file, so a reload triggered by a pure read drags the
    /// engine back to whatever the file says.
    #[test]
    fn merely_reading_the_config_does_not_revert_runtime_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        // Written before the watch starts: no event, so the store has never
        // seen this content and cannot mistake it for its own write.
        std::fs::write(&path, "voice = \"bm_george\"\n").expect("write");

        let engine = engine();
        let store = Arc::new(ConfigStore::new(
            path.clone(),
            engine.clone(),
            Config::default(),
        ));
        let _watcher = spawn(store).expect("watcher starts");

        engine.send(Command::SetVoice("am_fenrir".into()));
        wait_for_voice(&engine, "am_fenrir");

        std::fs::read_to_string(&path).expect("read");
        settle();
        assert_eq!(
            engine.snapshot().voice,
            "am_fenrir",
            "reading the config file must not re-apply it over runtime state"
        );
        engine.shutdown();
    }

    /// A deleted config file is not an edit to apply. Some editors unlink
    /// and recreate; resetting to defaults in the gap would be visible as a
    /// voice change and then a change back.
    #[test]
    fn a_missing_file_is_ignored_rather_than_applied_as_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let engine = engine();
        let store = ConfigStore::new(path.clone(), engine.clone(), Config::default());

        // A config the engine is running that defaults would visibly undo.
        // Reloading a *fresh* store is where "not applied as defaults" is
        // unobservable: it is already on defaults, so nothing could show.
        let cfg = Config {
            voice: "am_fenrir".into(),
            ..Config::default()
        };
        store.save(&cfg).expect("save succeeds");
        wait_for_voice(&engine, "am_fenrir");

        std::fs::remove_file(&path).expect("delete the config");
        assert_eq!(store.reload(), ReloadOutcome::Missing);

        settle();
        assert_eq!(
            engine.snapshot().voice,
            "am_fenrir",
            "a deleted config must leave the running settings alone"
        );
        engine.shutdown();
    }

    /// The Important finding this module was re-reviewed for: with a
    /// `cat >`-shaped rewrite of a *non-default* config, the engine must
    /// never observe a complete default config at any point, not merely
    /// converge to the right value eventually. Before the fix, `reload`
    /// read the file whole (via `Config::load_from`'s `exists()`-then-load)
    /// with no way to tell an empty file, mid-truncate, from one that
    /// genuinely means defaults, so the truncate step parsed cleanly into
    /// `Config::default()` and got applied: a `muted = true` daemon started
    /// speaking, and `model` moved `q8` -> `fp32` -> `q8` -- two ORT
    /// session teardowns and a rebuild of over a second for an edit that
    /// never touched `model` -- with a `q8`-only install landing in the
    /// sticky `Synth` wedge (`engine.rs:357-373`) for however long the
    /// `fp32` rebuild is live.
    ///
    /// Polled continuously from a background thread rather than checked
    /// only before and after: the whole point is that a transient default
    /// that lands and clears between two polls of the writer thread alone
    /// would otherwise hide.
    #[test]
    fn a_truncate_never_makes_the_engine_observe_a_default_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        // The reviewer's own repro setup: voice, model and muted all
        // non-default, so a full-default apply is unmistakable in any one
        // of three different ways.
        let starting = Config {
            voice: "bm_george".into(),
            model: "q8".into(),
            muted: true,
            ..Config::default()
        };
        starting.save_to(&path).expect("write");

        let engine = engine_with(starting.clone());
        let store = Arc::new(ConfigStore::new(
            path.clone(),
            engine.clone(),
            starting.clone(),
        ));
        let _watcher = spawn(store.clone()).expect("watcher starts");

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saw_default = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher_thread = {
            let engine = engine.clone();
            let stop = stop.clone();
            let saw_default = saw_default.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Some(cfg) = engine.config() {
                        if cfg == Config::default() {
                            saw_default.store(true, Ordering::Relaxed);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            })
        };

        // What `cat > config.toml` looks like from the outside: truncate,
        // then one valid prefix at a time. Spaced *longer* than DEBOUNCE --
        // the reviewer's own repro used ~500ms writes against a 250ms
        // window -- so each state gets its own reload instead of being
        // coalesced into one; a burst faster than DEBOUNCE would hide
        // exactly the bug this test is for.
        let between = DEBOUNCE + Duration::from_millis(50);
        std::fs::write(&path, "").expect("truncate");
        std::thread::sleep(between);
        std::fs::write(&path, "voice = \"bm_george\"\n").expect("write");
        std::thread::sleep(between);
        std::fs::write(&path, "voice = \"bm_george\"\nmodel = \"q8\"\n").expect("write");
        std::thread::sleep(between);
        std::fs::write(
            &path,
            "voice = \"bm_george\"\nmodel = \"q8\"\nmuted = true\n",
        )
        .expect("write");
        std::thread::sleep(between);

        stop.store(true, Ordering::Relaxed);
        watcher_thread.join().expect("watcher thread");

        assert!(
            !saw_default.load(Ordering::Relaxed),
            "the engine must never observe a complete default config mid-edit"
        );
        assert_eq!(
            engine.config().expect("engine answers"),
            starting,
            "the edit must settle on the content the writer finished with"
        );
        engine.shutdown();
    }

    /// Regression for the Minor 2 finding: a persistent stream of watch
    /// errors must not starve a pending edit forever. `debounce_loop` is
    /// driven directly with hand-fed ticks -- a real inotify error takes
    /// real broken input to produce, but the loop cannot tell a synthetic
    /// `Tick::Failed` from a real one, so this exercises exactly the code
    /// path notify's re-read-without-pausing behaviour hits.
    #[test]
    fn a_persistent_error_stream_does_not_starve_a_pending_reload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        // Already on disk before the loop starts: this test drives ticks
        // by hand rather than through real inotify events, so `reload`
        // just needs something new to read once it decides to read.
        let cfg = Config {
            voice: "am_fenrir".into(),
            ..Config::default()
        };
        cfg.save_to(&path).expect("write");

        let engine = engine();
        let store = Arc::new(ConfigStore::new(
            path.clone(),
            engine.clone(),
            Config::default(),
        ));

        let (tx, rx) = mpsc::channel();
        let mut watcher =
            notify::recommended_watcher(|_res: notify::Result<Event>| {}).expect("watcher");
        let watch_dir = dir.path().to_path_buf();
        let loop_store = store.clone();
        let loop_thread = std::thread::spawn(move || {
            debounce_loop(&loop_store, &rx, &mut watcher, &watch_dir);
        });

        tx.send(Tick::Changed).expect("send");
        // Faster than DEBOUNCE, and for well over DEBOUNCE in total: the
        // arrival rate the finding describes for a persistent, non-
        // `WouldBlock` inotify read failure. On the naive "restart
        // recv_timeout(DEBOUNCE) on any Ok" version this fix replaced,
        // ticks this close together keep the quiet window from ever
        // expiring for as long as they keep arriving.
        for _ in 0..40 {
            tx.send(Tick::Failed("simulated".into())).expect("send");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            store.applied_reloads(),
            1,
            "the deadline set by Changed must expire on its own, even while errors keep arriving"
        );

        drop(tx);
        loop_thread.join().expect("loop thread");
        engine.shutdown();
    }
}
