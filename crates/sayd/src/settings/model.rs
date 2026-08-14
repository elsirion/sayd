//! The settings model: what the window is allowed to show and to write.
//!
//! Nothing here draws anything. `SettingsModel` owns the one path a change
//! takes -- mutate a copy, validate it, write it through the `ConfigStore`
//! from Task 2, and only then let the window see it -- so the window layer
//! (`window.rs`, filled in by Task 5) can be nothing but widgets that read
//! `current()`/`voices()`/`MODELS` and call `edit`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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

/// `current` is a display cache, not the source of truth: it is seeded once
/// at construction and refreshed only after this model's own successful
/// `edit`s. Nothing here subscribes to `ConfigStore::reload`, so if an
/// external edit lands while the window is open, `current()` -- and so
/// whatever the window last drew from it -- can show a value the file no
/// longer holds. That is display staleness only: `edit` itself no longer
/// builds its write on top of this cache (see `edit`'s doc comment), so the
/// stale display cannot be written back over the external edit, only look
/// wrong until the next `edit` or window reopen refreshes it.
///
/// Task 5 closed the half of that gap that matters: `refresh` re-seeds from
/// the store, and `window::build` calls it as the window opens, so a freshly
/// drawn set of widgets never shows a value the file does not hold. The
/// remaining half -- a hand edit landing while the window is *already* open
/// -- is left deliberately: it would need a change signal this model does
/// not expose, and the spec asks for a view of the config, not for
/// live-updating widgets.
pub struct SettingsModel {
    store: Arc<ConfigStore>,
    voices: Vec<String>,
    current: Mutex<Config>,
}

impl SettingsModel {
    pub fn new(store: Arc<ConfigStore>, models_dir: PathBuf, current: Config) -> Self {
        SettingsModel {
            store,
            voices: list_voices(&models_dir),
            current: Mutex::new(current),
        }
    }

    /// The dropdown's contents: sorted voice-pack names.
    pub fn voices(&self) -> &[String] {
        &self.voices
    }

    pub fn current(&self) -> Config {
        self.current.lock().expect("settings mutex").clone()
    }

    /// Re-seed the display cache from the store, and hand back what it now
    /// holds.
    ///
    /// `current` is refreshed only by this model's own successful `edit`s
    /// (see the struct's doc comment), so a hand edit that
    /// `ConfigStore::reload` applied in between leaves it stale. The window
    /// calls this as it builds its rows, so what it draws comes from what
    /// the file actually holds rather than from whatever this model last
    /// wrote itself.
    ///
    /// `store.current()` and not a fresh `Config::load`: it already reflects
    /// both directions -- our writes and the watcher's reloads -- without a
    /// read of the file, so this cannot race the debounce thread or see a
    /// half-written file (see `ConfigStore::current`'s doc comment).
    pub fn refresh(&self) -> Config {
        let latest = self.store.current();
        *self.current.lock().expect("settings mutex") = latest.clone();
        latest
    }

    /// Apply one change: mutate a copy, validate it, write it through, and
    /// only then adopt it. A rejected or unwritable edit leaves the model
    /// exactly as it was, so the window never shows a value the file does
    /// not hold.
    ///
    /// The copy is seeded from `store.current()`, not from `self.current`.
    /// This model's own cache is refreshed only after its own writes, so it
    /// goes stale the moment something else changes the file -- a hand edit
    /// picked up by `ConfigStore::reload`, in particular. Seeding from it
    /// would mutate only the one field this edit touches and then write the
    /// *whole* stale copy back through `store.save`, silently reverting
    /// whatever the hand edit had changed while reporting success for a
    /// change the user never made. `store.current()` reflects both
    /// directions (see its doc comment) and needs no re-read of the file, so
    /// this stays free of the TOCTOU and the debounce-thread race a fresh
    /// `Config::load` here would reopen.
    ///
    /// Deliberately not seeded from the engine's own config either: that
    /// also carries runtime-only changes from the tray and MPRIS
    /// (`SetVoice`/`SetSpeed`/`SetMuted`), which are intentionally never
    /// persisted. Seeding from there would let an unrelated slider move
    /// write a transient tray mute into the file permanently.
    pub fn edit(&self, f: impl FnOnce(&mut Config)) -> Result<(), String> {
        let mut next = self.store.current();
        f(&mut next);
        validate(&mut next)?;
        self.store.save(&next)?;
        *self.current.lock().expect("settings mutex") = next;
        Ok(())
    }
}

/// Clamp what has a sensible nearest value, reject what does not.
///
/// Speed and thread count have obvious clamps. A model string does not: an
/// unrecognised one would fall through `model_file_for` to fp32, so the
/// file would claim something other than what loads.
fn validate(cfg: &mut Config) -> Result<(), String> {
    cfg.speed = cfg.speed.clamp(SPEED_MIN, SPEED_MAX);
    cfg.threads = cfg.threads.max(1);
    if !MODELS.iter().any(|(v, _)| *v == cfg.model) {
        return Err(format!(
            "'{}' is not a model this build knows; expected one of {}",
            cfg.model,
            MODELS
                .iter()
                .map(|(v, _)| *v)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
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

        let err = m
            .edit(|c| c.voice = "am_fenrir".into())
            .expect_err("write must fail");
        assert!(!err.is_empty());
        assert_eq!(
            m.current().voice,
            "af_heart",
            "a failed write must not leave the model out of step with the file"
        );
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
