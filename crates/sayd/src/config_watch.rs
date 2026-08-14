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

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
}

impl ConfigStore {
    pub fn new(path: PathBuf, engine: EngineHandle) -> Self {
        ConfigStore {
            path,
            engine,
            last_written: Mutex::new(None),
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
    pub fn save(&self, cfg: &Config) -> Result<(), String> {
        *self.last_written.lock().expect("last_written mutex") = Some(cfg.clone());
        if let Err(e) = cfg.save_to(&self.path) {
            // The stamp now describes a file that does not exist. Clear it
            // so a later genuine edit is not mistaken for our echo.
            *self.last_written.lock().expect("last_written mutex") = None;
            return Err(format!("could not write {}: {e}", self.path.display()));
        }
        self.engine.send(Command::ApplyConfig(cfg.clone()));
        Ok(())
    }

    /// Read the file and apply it unless it is our own echo.
    pub fn reload(&self) -> ReloadOutcome {
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
        if self.last_written.lock().expect("last_written mutex").as_ref() == Some(&cfg) {
            return ReloadOutcome::OwnWrite;
        }
        *self.last_written.lock().expect("last_written mutex") = Some(cfg.clone());
        self.engine.send(Command::ApplyConfig(cfg));
        ReloadOutcome::Applied
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

/// Watch the config file's directory and reload on change.
///
/// The *directory* is watched, not the file: an atomic temp+rename replaces
/// the inode, so a watch on the file itself stops seeing events after the
/// first write, and a config that does not exist yet cannot be watched at
/// all.
///
/// The returned watcher must be kept alive for the watch to stay active --
/// dropping it silently stops the reload.
pub fn spawn(store: Arc<ConfigStore>) -> Result<notify::RecommendedWatcher, String> {
    let dir = store
        .path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(format!("could not create {}: {e}", dir.display()));
    }

    let watched = store.path().to_path_buf();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        let Ok(event) = res else { return };
        if !is_content_change(&event.kind) || !event.paths.contains(&watched) {
            return;
        }
        match store.reload() {
            ReloadOutcome::Applied => {
                eprintln!("sayd: reloaded {}", watched.display());
            }
            ReloadOutcome::Failed(reason) => {
                eprintln!("warning: {reason}; keeping the running settings");
            }
            ReloadOutcome::OwnWrite | ReloadOutcome::Missing => {}
        }
    })
    .map_err(|e| format!("could not create a config watcher: {e}"))?;

    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .map_err(|e| format!("could not watch {}: {e}", dir.display()))?;
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sayd_core::audio::VecSink;
    use sayd_core::synth::StubSynthesizer;

    fn engine() -> EngineHandle {
        EngineHandle::spawn(
            Config::default(),
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
        std::thread::sleep(std::time::Duration::from_millis(600));
    }

    /// The write-through path: what the settings window calls. The file on
    /// disk and the running engine must agree afterwards.
    #[test]
    fn save_writes_the_file_and_reaches_the_engine() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let engine = engine();
        let store = ConfigStore::new(path.clone(), engine.clone());

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
        let store = ConfigStore::new(path.clone(), engine.clone());

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

    /// A genuine hand edit must reach the engine.
    #[test]
    fn an_external_edit_is_applied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let engine = engine();
        let store = ConfigStore::new(path.clone(), engine.clone());

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
        let store = ConfigStore::new(path.clone(), engine.clone());

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
        let store = Arc::new(ConfigStore::new(path.clone(), engine.clone()));
        let _watcher = spawn(store).expect("watcher starts");

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
        let store = Arc::new(ConfigStore::new(path.clone(), engine.clone()));
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
        let store = ConfigStore::new(path.clone(), engine.clone());
        assert_eq!(store.reload(), ReloadOutcome::Missing);
        engine.shutdown();
    }
}
