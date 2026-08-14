//! The config file as a live, two-way surface.
//!
//! Three directions meet here and must not fight:
//!
//! - **Write-through.** The settings window changes a value; it lands in
//!   `config.toml` and in the running engine.
//! - **Reload.** Someone edits `config.toml` by hand; it lands in the
//!   running engine.
//! - **Runtime control.** The tray, `say`, D-Bus or MPRIS changes mute or
//!   speed; it lands in both, through [`ConfigStore::update`]. It used to
//!   land in the engine alone, which meant the first change from either of
//!   the other two directions silently undid it -- see that method.
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
use std::sync::{Arc, Mutex, MutexGuard};
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

/// The standing complaint about the config *file*, for the tray to show.
///
/// Spec §11 asks for a malformed config to be surfaced in the tray, and
/// there was nowhere to put it: `main` printed the parse error to stderr at
/// startup and `debounce_loop` printed it on every failed reload, which on a
/// desktop means nobody ever sees it. The engine's own `error`/`State::Error`
/// is deliberately *not* that place -- it means "synthesis is broken" and
/// makes `Engine::submit` reject everything while it holds, so a typo in
/// `config.toml` would stop the daemon speaking at all, which is the
/// opposite of §11's "fall back to defaults, keep running".
///
/// So: a daemon-side slot, written by whatever last had an opinion about the
/// file (startup load, reload, a persisted mute that could not be written)
/// and read by `tray::SaydTray::menu`. One string, not a list: these are
/// alternatives, not accumulations -- the newest verdict on the file
/// replaces the previous one, and a good load clears it.
#[derive(Default)]
pub struct ConfigStatus {
    problem: Mutex<Option<String>>,
}

impl ConfigStatus {
    /// Poison-tolerant for the same reason `ConfigStore::stamp` is: this is
    /// read from `Tray::menu`, and a panic while the lock was held would
    /// otherwise turn every later menu render into a panic of its own.
    fn slot(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        match self.problem.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn set(&self, problem: Option<String>) {
        *self.slot() = problem;
    }

    pub fn get(&self) -> Option<String> {
        self.slot().clone()
    }
}

pub struct ConfigStore {
    path: PathBuf,
    engine: EngineHandle,
    last_written: Mutex<Config>,
    status: Arc<ConfigStatus>,
    applied_reloads: AtomicUsize,
    /// Test-only: how many times `current` took the stamp.
    ///
    /// Finding 6 is about a caller on the glib main thread taking this lock
    /// while a write holds it, and "did not take a lock" is invisible in any
    /// value the caller returns -- only in how long it took, which is not
    /// something to build a test on. Counting the reads makes it assertable.
    #[cfg(test)]
    stamp_reads: AtomicUsize,
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
            status: Arc::new(ConfigStatus::default()),
            applied_reloads: AtomicUsize::new(0),
            #[cfg(test)]
            stamp_reads: AtomicUsize::new(0),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The slot the tray renders the file's standing problem from.
    ///
    /// Owned here rather than passed in so `ConfigStore::new` keeps its
    /// signature: startup hands its own parse error straight to
    /// `store.status().set(..)` after construction, and the tray is given a
    /// clone of the same `Arc`.
    pub fn status(&self) -> Arc<ConfigStatus> {
        self.status.clone()
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
    /// What the stamp holds is the file's content *as the daemon runs it* --
    /// after `settings::model::normalize` (see `reload`), not the raw bytes.
    /// The two differ only for a file the daemon cannot honour literally
    /// (`model = "int4"`, `speed = 9.0`), and for those this is the more
    /// useful answer to both of the questions asked of it: an edit seeded
    /// from here builds on what is actually running and is not refused by
    /// `validate` over a field the user never touched, and a `ApplyConfig`
    /// sent from here says what the engine is really doing. This does not
    /// cost own-write suppression what an earlier version of this comment
    /// claimed: `reload` normalises *before* it compares against the stamp
    /// (see its body), so a byte-identical rewrite of such a file
    /// normalises to the same thing every time and is still recognised as
    /// our own -- one apply for a real edit, none for an echo, exactly like
    /// any other file. The one real residual was that a *suppressed* reload
    /// of such a file re-printed its warnings on every occurrence rather
    /// than only the one that changed anything; `reload` now logs them only
    /// on the path that actually enters the stamp, next to the equality
    /// check that decides it.
    ///
    /// There is no "we do not know" state to represent: the stamp is seeded
    /// at construction with the config the engine was spawned with, and a
    /// failed `save` restores what it displaced rather than clearing it (see
    /// `save`). A `None` here would be worse than useless to the caller that
    /// needs it most -- seeding an edit from `Config::default()` after a
    /// failed write would turn the next successful write into a reset of
    /// every setting the user has.
    pub fn current(&self) -> Config {
        #[cfg(test)]
        self.stamp_reads.fetch_add(1, Ordering::Relaxed);
        self.stamp().clone()
    }

    /// How many times `current` has taken the stamp. Test-only; see the
    /// field's doc comment.
    #[cfg(test)]
    pub(crate) fn stamp_reads(&self) -> usize {
        self.stamp_reads.load(Ordering::Relaxed)
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

    /// Write `cfg` to disk verbatim and apply it to the engine.
    ///
    /// The stamp is taken *before* the write: the watcher thread can observe
    /// the file the instant `save_to` renames it, and a stamp written
    /// afterwards would lose that race and let our own write bounce back as
    /// an external change.
    ///
    /// The stamp's lock is what makes a save and a reload one at a time --
    /// see `reload`.
    ///
    /// Test-only now, and deliberately not the primitive either production
    /// writer builds on: `update` (mute, speed) always seeds `cfg` from the
    /// stamp's *current* value, so nothing about it needs merging, and
    /// `SettingsModel`'s writer thread needs `save_merging` instead --
    /// IMPORTANT 2, on that method. What is left for `save` to do in
    /// production is nothing; what is left for it to do in tests is be the
    /// simplest possible way to put an exact, known config on disk and have
    /// it reach the engine, which most of this module's tests want and none
    /// of them want entangled with burst/seed bookkeeping that has nothing
    /// to do with what they are testing.
    #[cfg(test)]
    fn save(&self, cfg: &Config) -> Result<(), String> {
        let mut stamp = self.stamp();
        self.write_locked(&mut stamp, cfg.clone())
    }

    /// Write `next` to disk and apply it to the engine, except for whatever
    /// fields a debounced burst of edits did not itself change: those come
    /// from the stamp's *current* value rather than from whatever it held
    /// when the burst started. See `merge_untouched`.
    ///
    /// IMPORTANT 2: `SettingsModel`'s writer thread seeds a burst once, at
    /// its start, from the stamp then -- and only writes once the debounce
    /// settles, up to `WRITE_DEBOUNCE` later. Anything else that wrote to
    /// the file in between (a tray mute, an MPRIS rate change, a hand edit
    /// picked up by `reload`) is already in the stamp and on disk by the
    /// time this runs, and writing the burst's whole seeded copy verbatim
    /// would silently overwrite it -- the reviewer's repro was exactly
    /// this: nudge Speed in the window, mute from the tray before the
    /// window's debounce lands, and the mute reappeared unmuted ~250ms
    /// later. Taking the stamp once, here, and merging against it under the
    /// same lock the write itself uses closes the gap a separate
    /// `current()` call followed by a plain write would leave open (another
    /// writer landing between the read and the write, to be silently
    /// re-clobbered by this one).
    pub fn save_merging(&self, seed: &Config, next: &Config) -> Result<(), String> {
        let mut stamp = self.stamp();
        let merged = merge_untouched(seed, next, &stamp);
        self.write_locked(&mut stamp, merged)
    }

    /// The write both `save` and `save_merging` do, once the config to write
    /// has been decided and the stamp is already held.
    fn write_locked(&self, stamp: &mut MutexGuard<'_, Config>, cfg: Config) -> Result<(), String> {
        let displaced = std::mem::replace(&mut **stamp, cfg.clone());
        if let Err(e) = cfg.save_to(&self.path) {
            // `save_to` writes a temp file and renames it, and every failure
            // path fails before the rename -- so the destination still holds
            // exactly what it held before, which is what `displaced`
            // describes. Restoring it keeps own-write suppression correct for
            // the write *before* this one, and keeps `current` returning the
            // file's real content rather than a config nobody chose.
            **stamp = displaced;
            // MINOR 6: a write failure here used to reach only the caller
            // (and, for `update`'s callers, the log via `persist_in_
            // background`) -- never the tray. The window's own toast covers
            // the common case, but this is also reached from the writer
            // thread's debounce tail, which can finish after the window
            // that made the edit has closed; stderr is then the only
            // surface. `update`'s failure already went here; `save`'s now
            // does too.
            //
            // MINOR 4: without the path, for the same reason the parse-error
            // path lost its path in `e0a0fb2` -- the line is already
            // labelled `Config:`, there is exactly one config file, and an
            // 80-character menu label has no room to spare for one. The
            // `Err` returned below keeps the full path, for the log.
            self.status.set(Some(format!("could not write: {e}")));
            return Err(format!("could not write {}: {e}", self.path.display()));
        }
        // Whatever the file's standing complaint was, it is about a file
        // that no longer exists: this write went through `validate`, so the
        // model is one this build knows and the ranges are the engine's own.
        self.status.set(None);
        self.engine.send(Command::ApplyConfig(cfg));
        Ok(())
    }

    /// Persist a runtime control change -- mute, speed -- and apply it.
    ///
    /// CRITICAL 1 / IMPORTANT 2: three writers can change what the engine is
    /// running (this store, the file watcher, and the runtime commands from
    /// the tray, `say`, D-Bus and MPRIS), and only two of them used to end
    /// up in the file. `Command::SetMuted`/`SetSpeed` changed `cfg` inside
    /// the engine and nowhere else, so the *next* `ApplyConfig` from either
    /// of the other two writers -- a settings-window save, a hand edit, a
    /// dotfile manager rewriting `~/.config/sayd` -- replaced the whole
    /// `Config` and silently undid them. Measured: mute from the tray, then
    /// edit the config's *voice*, and the daemon unmutes, the tray checkbox
    /// flips back on its own and the next submission is audible. Spec §6 is
    /// explicit that mute "is sticky across utterances and persists to
    /// config", and a restart lost it too.
    ///
    /// So these go through the file like every other setting, and reach the
    /// engine as `ApplyConfig` -- the one place a config becomes behaviour.
    /// For mute that also needs `ApplyConfig` to keep doing what
    /// `SetMuted(true)` does to the transport, which is IMPORTANT 5, fixed
    /// in `sayd-core`'s engine.
    ///
    /// Seeded from the stamp, not from the engine: the engine's `Config` is
    /// the *running* one, and seeding an edit from there is what the
    /// settings model deliberately avoids (see `SettingsModel::edit`).
    ///
    /// Blocking, like `save_merging` and for the same reason -- it is
    /// `write_locked`'s write, the same disk I/O both go through -- so
    /// callers on a thread that must not block on disk go through
    /// [`persist_in_background`].
    ///
    /// `fallback` is sent unconditionally, *before* anything here touches
    /// disk -- MINOR 8: `save_to` sits between "not yet applied" and either
    /// outcome for as long as the write takes, unbounded on a hung NFS or
    /// FUSE home, and a fallback that only fired from the error arm could
    /// not help while the write was still in flight -- "shut up now" would
    /// wait on a stuck filesystem. Sending it first means it never does. The
    /// later `ApplyConfig` carries the same value once the write is decided
    /// either way, so for a mute this makes it a no-op by the time it lands:
    /// `Engine::handle`'s transition test (`cfg.muted && !self.cfg.muted`)
    /// is already false, because `fallback` already made it so.
    ///
    /// The stamp is held across the send on both the success and the
    /// failure path, exactly like `save`/`save_merging` and `reload` -- and
    /// for exactly `reload`'s reason (see its doc comment) -- because this
    /// goes through the same `write_locked` they do. Releasing it first was
    /// this method's own bug, back when it had its own copy of this write
    /// instead: a mute's `update` could release the stamp, be preempted
    /// before its `ApplyConfig(cfg)` send, let a `save` or `reload` run its
    /// whole read-write-send cycle under the lock this one had just given
    /// up, and only then send its own now-stale `ApplyConfig` last --
    /// landing the engine on a config the stamp (and the file) had already
    /// moved past, with own-write suppression then swallowing every later
    /// reload that might have corrected it. That is this module's original
    /// failure shape -- the engine and the file disagreeing about mute,
    /// durably -- re-entering through the one writer that was not holding
    /// the lock across its send.
    ///
    /// A failed write still applies the change (via `fallback`, above): a
    /// mute the daemon cannot write down must still shut it up. The caller
    /// gets the error back and the tray gets the standing complaint, but the
    /// room goes quiet either way.
    fn update(&self, change: impl FnOnce(&mut Config), fallback: Command) -> Result<(), String> {
        self.engine.send(fallback);

        let mut stamp = self.stamp();
        let mut cfg = stamp.clone();
        change(&mut cfg);
        self.write_locked(&mut stamp, cfg)
    }

    /// Mute or unmute, persistently. See [`ConfigStore::update`].
    pub fn set_muted(&self, muted: bool) -> Result<(), String> {
        self.update(|cfg| cfg.muted = muted, Command::SetMuted(muted))
    }

    /// Set the speed, persistently, clamped exactly as the engine and the
    /// settings window clamp it -- MPRIS advertises `MinimumRate`/
    /// `MaximumRate` over the same bounds, but a client is free to ignore
    /// them, and an out-of-range value must not reach the file when the
    /// engine would only clamp it again. See [`ConfigStore::update`].
    pub fn set_speed(&self, speed: f32) -> Result<(), String> {
        let speed = speed.clamp(
            crate::settings::model::SPEED_MIN,
            crate::settings::model::SPEED_MAX,
        );
        self.update(|cfg| cfg.speed = speed, Command::SetSpeed(speed))
    }

    /// Read the file and apply it unless it is our own echo.
    ///
    /// The stamp's lock is held across the whole of read, compare and
    /// stamp -- and across `write_locked`'s whole write -- so that the two
    /// cannot interleave. Taking it only for the compare left this window: a
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
    /// itself: `write_locked`'s `save_to` call is disk I/O with no bound
    /// this module puts on it. `save_merging` is what the settings window's
    /// writes end up in, so it (like `update`) must not be called from a
    /// thread that cannot block on disk (a UI event-loop thread, say) --
    /// doing so would stall every reload for as long as that write takes,
    /// and freeze the UI with it. That is why `SettingsModel` owns a writer
    /// thread and never calls this from the glib main thread; see
    /// `SettingsModel::edit`.
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
            // MINOR 5: a deleted file is not a standing complaint about one
            // -- deleting `config.toml` to start over is a plausible thing
            // to do, and the tray must not go on showing a parse error (or
            // a stale clamp warning) for a file that is no longer there.
            // `NotFound` used to leave `status` untouched, so whatever it
            // said before the delete just sat there.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.status.set(None);
                return ReloadOutcome::Missing;
            }
            // Some other I/O problem (permissions, a symlink loop) -- itself
            // a standing complaint about the file, same as a parse failure
            // below, and must replace whatever `status` held before rather
            // than leave it stale next to it.
            Err(e) => {
                self.status.set(Some(e.to_string()));
                return ReloadOutcome::Failed(format!("{}: {e}", self.path.display()));
            }
        };
        if txt.trim().is_empty() {
            // Same reasoning as `NotFound` just above: treated as `Missing`
            // because nobody means an empty file (see the comment on
            // `DEBOUNCE`), so treated the same for the standing complaint.
            self.status.set(None);
            return ReloadOutcome::Missing;
        }
        let (mut cfg, err) = Config::load_str(&txt);
        if let Some(reason) = err {
            // Deliberately not applying `cfg` here: `load_str` returns
            // defaults alongside the error, and applying those would reset
            // every setting the user has because of one typo.
            // IMPORTANT 4: and into the tray, per spec §11 -- see
            // `ConfigStatus`. stderr alone means a desktop user learns
            // nothing at all; measured, `sh.sayd.Sayd1.Error` was `""` and
            // `State` was `idle` after a malformed file landed.
            //
            // The tray gets the reason without the path: the line is
            // already labelled `Config:`, the daemon has exactly one config
            // file, and a menu label is truncated -- measured, a path long
            // enough to fill it left the user with nothing but the path.
            // The log below keeps the full thing.
            self.status.set(Some(reason.clone()));
            return ReloadOutcome::Failed(format!("{}: {reason}", self.path.display()));
        }
        // IMPORTANT 3: nothing used to stand between `load_str` and the
        // engine, so a file the daemon could not honour literally was
        // applied verbatim and in silence -- `model = "int4"` left the
        // daemon running fp32 (`model_file_for`'s fallback) while the file
        // and `say status` both claimed int4, durably and invisibly, and
        // `speed = 9.0` left the file disagreeing with the engine's clamp
        // indefinitely. `normalize` is the settings window's own rules, in
        // the one shape a reload can use: it cannot refuse the user's file,
        // so it says what it had to change and carries on. Nothing is
        // written back (spec §11).
        let warnings = crate::settings::model::normalize(&mut cfg);
        self.status.set(if warnings.is_empty() {
            None
        } else {
            Some(warnings.join("; "))
        });
        if *stamp == cfg {
            return ReloadOutcome::OwnWrite;
        }
        // Logged only here, on the path that actually enters the stamp --
        // not above, next to `status.set`. A file the daemon cannot honour
        // literally normalises to the same warned-about config on every
        // reload, so without this a periodic rewrite of such a file (or
        // anything else that fires a reload without changing what it says)
        // would re-print the same warnings forever, even though `status`
        // already carries them and nothing about the running config just
        // changed.
        for w in &warnings {
            eprintln!("warning: {}: {w}", self.path.display());
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

/// Build the config `save_merging` should actually write: `next` for a field
/// that differs from `seed`, `fresh` (the stamp's value at write time) for
/// one that does not.
///
/// `seed` is what the writing burst started from and `next` is what it has
/// become after every edit in it; a field where the two disagree is a field
/// this burst genuinely set, and that value must win no matter what else has
/// happened to the file meanwhile -- it is the whole reason the burst is
/// being written at all. A field where they agree was never touched by this
/// burst, and `next`'s value for it is nothing but a stale copy of whatever
/// the stamp held when the burst was seeded; `fresh` is what the stamp holds
/// *now*, which is the freshest thing anyone has told this store about that
/// field, ours or another writer's (`update`, `reload`) that landed while
/// this burst was debouncing.
///
/// The comparison is against `seed`, deliberately not against `fresh`:
/// comparing `next` to `fresh` would call a field "touched" merely because
/// someone else changed it, and write back this burst's stale copy of it --
/// exactly the clobber this function exists to avoid.
///
/// The one case this does not distinguish is a field the burst set back to
/// its own seed value within itself (nudge Speed up, then down to exactly
/// where it started, before the debounce fires): that is indistinguishable
/// from never having touched it and is treated the same way, which can only
/// ever lose a no-op. Whole nested structs (`cleanup`, `chunking`) are
/// compared and taken as a unit, matching how the window itself changes
/// them -- one row's handler sets one field of `cfg.cleanup`, never `cleanup`
/// piecemeal from two different writers.
fn merge_untouched(seed: &Config, next: &Config, fresh: &Config) -> Config {
    fn pick<T: Clone + PartialEq>(seed: &T, next: &T, fresh: &T) -> T {
        if next != seed {
            next.clone()
        } else {
            fresh.clone()
        }
    }
    Config {
        voice: pick(&seed.voice, &next.voice, &fresh.voice),
        speed: pick(&seed.speed, &next.speed, &fresh.speed),
        model: pick(&seed.model, &next.model, &fresh.model),
        threads: pick(&seed.threads, &next.threads, &fresh.threads),
        idle_unload_secs: pick(
            &seed.idle_unload_secs,
            &next.idle_unload_secs,
            &fresh.idle_unload_secs,
        ),
        muted: pick(&seed.muted, &next.muted, &fresh.muted),
        max_chars: pick(&seed.max_chars, &next.max_chars, &fresh.max_chars),
        cleanup: pick(&seed.cleanup, &next.cleanup, &fresh.cleanup),
        chunking: pick(&seed.chunking, &next.chunking, &fresh.chunking),
    }
}

/// Run a persisting change off the calling thread, reporting a failure.
///
/// Every caller of [`ConfigStore::set_muted`]/[`ConfigStore::set_speed`] in
/// the daemon is somewhere that must not block on disk: a zbus method
/// handler (`dbus.rs`), a ksni menu callback (`tray.rs`) or an MPRIS
/// property setter (`mpris.rs`). All three run on tokio -- ksni's own
/// service task is `tokio::spawn`ed, which is what makes a runtime
/// guaranteed to be in scope for its callbacks too (see
/// `tray::SaydTray::speak`) -- so `spawn_blocking` is available to all of
/// them and is the one place that knows this, rather than three copies of
/// the same three lines.
///
/// Fire and forget, like `EngineHandle::send`: a mute is a transport
/// command whose caller has already been answered, and the change reaches
/// the engine whether or not the write succeeds (see `update`). The failure
/// is logged here and shown in the tray by `update` itself.
pub fn persist_in_background(
    store: Arc<ConfigStore>,
    change: impl FnOnce(&ConfigStore) -> Result<(), String> + Send + 'static,
) {
    tokio::task::spawn_blocking(move || {
        if let Err(e) = change(&store) {
            eprintln!("warning: {e}");
        }
    });
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
            // Also in the tray from here on, per spec §11 -- `reload` puts
            // it in the `ConfigStatus` slot; this line is for the log.
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

    /// Poll the published snapshot until `f` holds. The engine runs on its
    /// own thread, so every command is fire-and-forget from here.
    fn wait_for(
        engine: &EngineHandle,
        label: &str,
        f: impl Fn(&sayd_core::engine::Snapshot) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if f(&engine.snapshot()) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!(
            "timed out waiting for {label}; snapshot = {:?}",
            engine.snapshot()
        );
    }

    /// The same, for the engine's live `Config` -- which, unlike the
    /// snapshot, carries the fields (`model`, `threads`) a config apply is
    /// mostly about.
    fn wait_for_config(engine: &EngineHandle, label: &str, f: impl Fn(&Config) -> bool) -> Config {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Some(c) = engine.config() {
                if f(&c) {
                    return c;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!(
            "timed out waiting for {label}; config = {:?}",
            engine.config()
        );
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

    /// MINOR 6: `save`'s failure used to reach only the caller -- `update`'s
    /// already went to the tray, `save`'s did not. The window's own toast
    /// covers the common case, but this is also what `save_merging` shares
    /// (`SettingsModel`'s writer thread calls it well after the click that
    /// triggered it, and the debounce tail can outlive the window that made
    /// the edit), so stderr used to be the only surface for a write that
    /// failed after the window that caused it had already closed.
    #[test]
    fn a_failed_save_reaches_the_tray_the_same_way_a_failed_update_does() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("blocker");
        let engine = engine();
        let store = ConfigStore::new(
            blocker.join("config.toml"),
            engine.clone(),
            Config::default(),
        );

        let cfg = Config {
            voice: "am_fenrir".into(),
            ..Config::default()
        };
        assert!(store.save(&cfg).is_err(), "the write must fail");
        assert_eq!(
            store
                .status()
                .get()
                .as_deref()
                .map(|s| s.contains("could not write")),
            Some(true),
            "a failed settings-window write must reach the tray, exactly like a failed update"
        );
        engine.shutdown();
    }

    /// MINOR 8: `update` used to send its transport fallback only from the
    /// error arm of a *finished* write, so a write that is neither `Ok` nor
    /// `Err` yet -- stuck, on a hung NFS or FUSE mount -- left "shut up now"
    /// waiting on it too. A named pipe at the write's temp-file path
    /// reproduces "stuck" precisely: `Config::save_to` writes to
    /// `<path>.tmp` with a plain `std::fs::write`, and opening a FIFO for
    /// writing blocks until something reads it -- nothing here ever does,
    /// so the write never returns for the life of this test.
    #[test]
    fn a_mute_takes_effect_even_while_the_write_is_stuck() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let tmp = path.with_extension("toml.tmp");
        let tmp_c = std::ffi::CString::new(tmp.to_str().expect("utf8 path")).expect("no NUL");
        assert_eq!(
            unsafe { libc::mkfifo(tmp_c.as_ptr(), 0o600) },
            0,
            "mkfifo failed: {}",
            std::io::Error::last_os_error()
        );

        let engine = engine();
        let store = Arc::new(ConfigStore::new(
            path.clone(),
            engine.clone(),
            Config::default(),
        ));

        // `set_muted` blocks inside `save_to`'s `std::fs::write` to the
        // FIFO -- that is the point, it is a synchronous call -- so it runs
        // on its own thread, deliberately never joined: nothing ever reads
        // the FIFO, so this call does not return before the test process
        // does.
        let writer = store.clone();
        std::thread::spawn(move || {
            let _ = writer.set_muted(true);
        });

        wait_for(&engine, "the engine to mute despite the stuck write", |s| {
            s.muted
        });
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

    /// CRITICAL 1 at the store level: mute goes to the file as well as the
    /// engine, so the next `ApplyConfig` from any of the other writers
    /// cannot undo it and a restart does not lose it (spec §6).
    #[test]
    fn a_persisted_mute_reaches_both_the_file_and_the_engine() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let engine = engine();
        let store = ConfigStore::new(path.clone(), engine.clone(), Config::default());

        store.set_muted(true).expect("the write must succeed");
        let (on_disk, err) = Config::load_from(&path);
        assert_eq!(err, None);
        assert!(on_disk.muted, "spec §6: mute persists to config");
        wait_for(&engine, "the engine to mute", |s| s.muted);

        // And the reverse, so the file cannot get stuck muted.
        store.set_muted(false).expect("the write must succeed");
        let (on_disk, _) = Config::load_from(&path);
        assert!(!on_disk.muted);
        wait_for(&engine, "the engine to unmute", |s| !s.muted);
        engine.shutdown();
    }

    /// The measured failure: mute, then edit an unrelated field of the file,
    /// and the daemon used to unmute itself.
    #[test]
    fn a_persisted_mute_survives_a_reload_of_an_unrelated_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let engine = engine();
        let store = ConfigStore::new(path.clone(), engine.clone(), Config::default());

        store.set_muted(true).expect("the write must succeed");
        wait_for(&engine, "the engine to mute", |s| s.muted);

        let (mut edited, _) = Config::load_from(&path);
        edited.voice = "bm_george".into();
        edited.save_to(&path).expect("hand edit");
        assert_eq!(store.reload(), ReloadOutcome::Applied);

        wait_for_voice(&engine, "bm_george");
        assert!(
            engine.snapshot().muted,
            "a config change that never mentioned mute must not unmute"
        );
        engine.shutdown();
    }

    /// A mute the daemon cannot write down must still shut it up: the
    /// transport behaviour is the urgent half, and the caller (and the tray)
    /// still learn the file did not get it.
    #[test]
    fn a_mute_that_cannot_be_written_still_mutes() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A path whose parent is a *file* cannot be created as a directory,
        // so `save_to` fails for a reason that needs no permission games.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("blocker");
        let engine = engine();
        let store = ConfigStore::new(
            blocker.join("config.toml"),
            engine.clone(),
            Config::default(),
        );

        let err = store.set_muted(true).expect_err("the write cannot succeed");
        assert!(!err.is_empty());
        wait_for(&engine, "the engine to mute anyway", |s| s.muted);
        assert_eq!(
            store
                .status()
                .get()
                .as_deref()
                .map(|s| s.contains("could not write")),
            Some(true),
            "and the tray is told the file did not get it"
        );
        assert_eq!(
            store.current(),
            Config::default(),
            "a failed write must leave the stamp on what the file really holds"
        );
        engine.shutdown();
    }

    /// IMPORTANT 1: `update` (what `set_muted`/`set_speed` call) used to
    /// release the stamp before sending `ApplyConfig`, the only one of the
    /// three writers that did. That let a concurrent `save` run its whole
    /// stamp-write-send cycle inside the gap, so `update`'s own send --
    /// built from an older config -- could land at the engine *after* it:
    /// the engine ends up on a config the stamp (and the file) have already
    /// moved past, and since the stamp really does say the newer thing,
    /// `reload` treats every later attempt to correct it as an echo and
    /// swallows it. That is this module's original failure shape -- the
    /// engine and the file disagreeing about mute, durably and silently --
    /// re-entering through the one writer that did not hold the lock across
    /// its send.
    ///
    /// Same technique as `a_reload_racing_saves_cannot_leave_the_engine_on_
    /// the_older_config` above: race the two for long enough and check only
    /// the final state, since the losing interleaving is narrow and only
    /// the last loser leaves a mark that survives to the assertion. With
    /// the stamp held across every writer's send, the total order of stamp
    /// writes and the total order of engine sends are the same order, so
    /// the engine and the stamp cannot disagree once both race loops stop.
    #[test]
    fn a_mute_racing_a_save_cannot_leave_the_engine_disagreeing_with_the_stamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let engine = engine();
        let store = Arc::new(ConfigStore::new(
            path.clone(),
            engine.clone(),
            Config::default(),
        ));

        let unrelated_a = Config {
            voice: "am_fenrir".into(),
            ..Config::default()
        };
        let unrelated_b = Config {
            voice: "bm_george".into(),
            ..Config::default()
        };

        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let savers: Vec<_> = [unrelated_a, unrelated_b]
            .into_iter()
            .map(|cfg| {
                let store = store.clone();
                let done = done.clone();
                std::thread::spawn(move || {
                    while !done.load(Ordering::Relaxed) {
                        let _ = store.save(&cfg);
                    }
                })
            })
            .collect();
        for i in 0..4_000 {
            store.set_muted(i % 2 == 0).expect("the write must succeed");
        }
        done.store(true, Ordering::Relaxed);
        for saver in savers {
            saver.join().expect("saver thread");
        }

        // Deliberately no reload here, for the same reason the racing-saves
        // test has none: a reload would re-read the file and could paper
        // over a stuck engine by correcting it, rather than exposing that
        // it was ever stuck.
        let want = store.current();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while engine.config().as_ref() != Some(&want) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            engine.config().expect("engine answers"),
            want,
            "the engine must not be stuck on a config older than the stamp (and the file)"
        );
        engine.shutdown();
    }

    /// IMPORTANT 3: a file the daemon cannot honour literally is applied as
    /// what it will actually run, and says so. Before this, `model = "int4"`
    /// went to the engine verbatim, `model_file_for` fell through to fp32,
    /// and nothing anywhere said the daemon was running a different model
    /// from the one the file (and `say status`) claimed.
    #[test]
    fn an_unrunnable_model_is_normalized_and_reported_rather_than_applied_verbatim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let engine = engine();
        let store = ConfigStore::new(path.clone(), engine.clone(), Config::default());

        std::fs::write(&path, "model = \"int4\"\nthreads = 0\nspeed = 9.0\n").expect("write");
        assert_eq!(store.reload(), ReloadOutcome::Applied);

        let applied = wait_for_config(&engine, "the normalized config", |c| c.model == "fp32");
        assert_eq!(applied.threads, 1, "threads = 0 is not a thread count");
        assert!(
            (applied.speed - 2.0).abs() < f32::EPSILON,
            "speed is clamped"
        );
        assert_eq!(
            store.current(),
            applied,
            "the stamp must hold what is running, so the window builds on it"
        );

        let problem = store.status().get().expect("the tray must be told");
        assert!(problem.contains("int4"), "{problem}");
        assert!(problem.contains("fp32"), "{problem}");
        assert!(problem.contains("9"), "the clamp too: {problem}");
        assert!(
            !std::fs::read_to_string(&path)
                .expect("read")
                .contains("fp32"),
            "spec §11: the user's file is not corrected behind their back"
        );
        engine.shutdown();
    }

    /// IMPORTANT 4: spec §11 says a malformed config is surfaced *in the
    /// tray*. Measured before this: after `voice = [this is not toml`
    /// landed, `sh.sayd.Sayd1.Error` was `""` and `State` was `idle` -- the
    /// daemon knew and no surface said so. It must also not go into the
    /// engine's error, which would reject every submission.
    #[test]
    fn a_malformed_config_is_surfaced_for_the_tray_without_erroring_the_engine() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let engine = engine();
        let store = ConfigStore::new(path.clone(), engine.clone(), Config::default());

        std::fs::write(&path, "voice = [this is not toml").expect("write");
        assert!(matches!(store.reload(), ReloadOutcome::Failed(_)));

        let problem = store.status().get().expect("the tray must be told");
        assert!(
            problem.contains("expected"),
            "the parse reason, not just the path a truncated menu label would eat: {problem}"
        );
        let s = engine.snapshot();
        assert_eq!(
            s.error, None,
            "a typo in config.toml must not look like broken synthesis"
        );
        assert_ne!(s.state, sayd_core::engine::State::Error);
        assert!(
            engine
                .submit("Still speaking.".into(), Default::default())
                .is_ok(),
            "and must not stop the daemon speaking"
        );

        // And it clears once the file parses again.
        Config::default().save_to(&path).expect("repair");
        let _ = store.reload();
        assert_eq!(store.status().get(), None);
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

    /// MINOR 5: a deleted config file is not a standing complaint about one.
    /// Measured: after `rm ~/.config/sayd/config.toml` the tray kept
    /// showing a parse error for a file that no longer existed --
    /// `reload`'s `NotFound` arm returned `Missing` without ever touching
    /// `status`, so whatever it said before the delete just sat there.
    /// Deleting the config to start fresh is a plausible thing for a user
    /// to do, and the tray line has to clear when they do it.
    #[test]
    fn a_deleted_config_clears_a_stale_tray_complaint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let engine = engine();
        let store = ConfigStore::new(path.clone(), engine.clone(), Config::default());

        std::fs::write(&path, "voice = [this is not toml").expect("write");
        assert!(matches!(store.reload(), ReloadOutcome::Failed(_)));
        assert!(
            store.status().get().is_some(),
            "sanity: the file's complaint must be standing before the delete"
        );

        std::fs::remove_file(&path).expect("delete the config");
        assert_eq!(store.reload(), ReloadOutcome::Missing);
        assert_eq!(
            store.status().get(),
            None,
            "a deleted file cannot still be malformed"
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
