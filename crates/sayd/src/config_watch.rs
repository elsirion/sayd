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
use std::time::Duration;

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
    last_written: Mutex<Option<Config>>,
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
            last_written: Mutex::new(Some(running)),
            applied_reloads: AtomicUsize::new(0),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
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
        let mut stamp = self.last_written.lock().expect("last_written mutex");
        *stamp = Some(cfg.clone());
        if let Err(e) = cfg.save_to(&self.path) {
            // The stamp now describes a file that does not exist. Clear it
            // so a later genuine edit is not mistaken for our echo.
            *stamp = None;
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
    /// held waiting on the engine.
    pub fn reload(&self) -> ReloadOutcome {
        let mut stamp = self.last_written.lock().expect("last_written mutex");
        if !self.path.exists() {
            return ReloadOutcome::Missing;
        }
        let (cfg, err) = Config::load_from(&self.path);
        if let Some(reason) = err {
            // Deliberately not applying `cfg` here: `load_from` returns
            // defaults alongside the error, and applying those would reset
            // every setting the user has because of one typo.
            return ReloadOutcome::Failed(reason);
        }
        if stamp.as_ref() == Some(&cfg) {
            return ReloadOutcome::OwnWrite;
        }
        *stamp = Some(cfg.clone());
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
/// `save_to` renames over the file. Every prefix of a TOML file is itself
/// valid TOML and every `Config` field has a serde default, so the empty
/// file in the middle of a `>` redirect parses cleanly into a *complete
/// default config* -- there is no malformed-input error to hide behind, and
/// no emptiness or "equals `Config::default()`" test can tell it from a
/// user who genuinely wants defaults. Only time can: wait for the writer to
/// stop.
///
/// The cost of getting this wrong is not a blip. `ApplyConfig` reconfigures
/// the synthesizer and drops the ~1.27 GB ORT session when `model` moves,
/// so an edit that never mentioned `model` would pay for a teardown and a
/// rebuild of over a second, twice -- and on a machine that has only the
/// `q8` model installed, the rebuild for the *default* `fp32` fails into a
/// sticky synth error that rejects every later submission.
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
        match rx.recv() {
            Ok(tick) => match handle(tick, watcher, dir, &mut errors) {
                Next::Reload => {}
                Next::Wait => continue,
                Next::Stop => return,
            },
            Err(_) => return,
        }
        loop {
            match rx.recv_timeout(DEBOUNCE) {
                // Still being written. Restart the quiet period.
                Ok(tick) => match handle(tick, watcher, dir, &mut errors) {
                    Next::Reload | Next::Wait => continue,
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

        assert_eq!(
            store.reload(),
            ReloadOutcome::OwnWrite,
            "the write we just made must be recognised as ours"
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

        std::fs::write(&path, "voice = [this is not toml").expect("write");
        match store.reload() {
            ReloadOutcome::Failed(reason) => assert!(!reason.is_empty()),
            other => panic!("expected a parse failure, got {other:?}"),
        }

        std::thread::sleep(std::time::Duration::from_millis(100));
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
        // to lose shows up in the final state. This can only fail on code
        // that has the bug, never on code that does not.
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
        for _ in 0..400 {
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
        assert_eq!(store.reload(), ReloadOutcome::Missing);
        engine.shutdown();
    }
}
