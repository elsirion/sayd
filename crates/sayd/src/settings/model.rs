//! The settings model: what the window is allowed to show and to write.
//!
//! Nothing here draws anything. `SettingsModel` owns the one path a change
//! takes -- mutate a copy, validate it, write it through the `ConfigStore`
//! from Task 2, and only then let the window see it -- so the window layer
//! (`window.rs`, filled in by Task 5) can be nothing but widgets that read
//! `current()`/`voices()`/`MODELS` and call `edit`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use sayd_core::config::{Config, RewordConfig};

use crate::config_watch::ConfigStore;
use crate::notify::seen::{self, SeenApp};
use crate::notify::{truncate_chars, MAX_APP_NAME_LEN};
use crate::reword::{RewordError, Rewriter};

/// The model values, with the measured trade-off shown inline in the
/// window. Numbers are from the benchmark recorded in the design doc; do
/// not adjust them without re-measuring.
pub const MODELS: [(&str, &str); 3] = [
    ("fp32", "best quality, RTF 4.78"),
    ("fp16", "RTF 4.66"),
    ("q8", "fastest, RTF 1.40, some quality loss"),
];

/// The `speed_mode` values, with the measured trade-off shown inline the same
/// way [`MODELS`] is. Numbers are from the leading-word investigation; do not
/// adjust them without re-measuring. `"model"` is first so it stays
/// [`FALLBACK_SPEED_MODE`], matching `Config::default`.
pub const SPEED_MODES: [(&str, &str); 2] = [
    (
        "model",
        "Kokoro's speed input; ~10 dB leading-word dropout near speed 1.3",
    ),
    (
        "stretch",
        "resynthesize at 1.0 and time-stretch; no dropout, own artifacts",
    ),
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
///
/// `0` and then `NOTIFY_COOLDOWN_MIN_SECS`, with nothing in between: a typed
/// `1` or `2` is raised by `clamp_ranges` before it is written, so the row
/// snaps rather than saving a value the next load would silently change.
pub const COOLDOWN_MIN: f64 = 0.0;
/// An hour, the same ceiling `IDLE_UNLOAD_MAX` uses: past it the spinner is
/// no longer a control anyone drives to the end, and a longer window is a
/// hand edit (which, as with every other row here, is left alone rather than
/// clamped -- see the doc comment above).
pub const COOLDOWN_MAX: f64 = 3600.0;
pub const COOLDOWN_STEP: f64 = 5.0;
/// What the Reword group's spin rows offer, per spec §6. `timeout_ms`'s
/// ceiling is the load-bearing one: `sayd-cli` bounds every D-Bus
/// interaction at 3 s and `say --reword` waits for the rewrite inline, so a
/// budget past `REWORD_TIMEOUT_MAX_MS` would turn a slow provider into a CLI
/// error instead of a spoken sentence.
///
/// The numbers themselves live in `sayd_core::config`, because
/// `Config::load_str` applies the same clamp to a hand-edited file that
/// never reaches a spin row: two spellings of one window is exactly the
/// drift this pair exists to prevent.
pub const REWORD_TIMEOUT_MIN: f64 = sayd_core::config::REWORD_TIMEOUT_MIN_MS as f64;
pub const REWORD_TIMEOUT_MAX: f64 = sayd_core::config::REWORD_TIMEOUT_MAX_MS as f64;
pub const REWORD_TIMEOUT_STEP: f64 = 100.0;
/// The floor is not `0`: there is no magic zero here. `enabled` is the off
/// switch, and `--reword` on an over-long submission is a no-op rather than
/// an error.
pub const REWORD_MAX_CHARS_MIN: f64 = 32.0;
pub const REWORD_MAX_CHARS_MAX: f64 = 2000.0;
pub const REWORD_MAX_CHARS_STEP: f64 = 32.0;

/// What the Test row starts with: the example this whole feature exists
/// for, so pressing Test once without typing anything is already a
/// meaningful test. A working setup answers with a recognisably better
/// sentence, and a user who has never thought about what rewording *is*
/// gets shown it.
// `#[allow(dead_code)]`: the Test row's widgets are a later task in this
// milestone and are its only consumer. Produced here because §6's rule is
// that every string the window shows is decided in this module.
#[allow(dead_code)]
pub const REWORD_TEST_DEFAULT: &str = "Alice: where do you want to go for dinner";

/// The six rows of §6's endpoint table, offered by the Endpoint row's
/// preset menu. UI strings rather than config values -- a preset that goes
/// stale costs nothing and adding one is a line -- but they live here, not
/// in `window.rs`, for the reason [`MODELS`] and [`SPEED_MODES`] do: the
/// window is the one layer with no test coverage.
///
/// The third field is the table's Key column, collapsed to a bool:
/// `false` for the row's "ignored", `true` for "as configured" or `sk-…`.
/// [`reword_key_row_applies`] reads it back out to decide whether the API
/// key row is offered for a *loopback* preset -- vLLM is the one local
/// server in this table whose Key column is not "ignored" (`vllm serve
/// --api-key …` is a real invocation), so it cannot be folded into "this
/// machine, therefore no credential" the way Ollama, llama.cpp `server` and
/// LM Studio can. PPQ and OpenAI are `true` here too, for completeness with
/// the table, but their row is already shown by the loopback check alone.
pub const ENDPOINT_PRESETS: [(&str, &str, bool); 6] = [
    ("Ollama", "http://localhost:11434/v1", false),
    ("llama.cpp server", "http://localhost:8080/v1", false),
    ("LM Studio", "http://localhost:1234/v1", false),
    ("vLLM", "http://localhost:8000/v1", true),
    ("PPQ", "https://api.ppq.ai/v1", true),
    ("OpenAI", "https://api.openai.com/v1", true),
];

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

/// How the Test row builds its client. Injectable so every row of §6's Test
/// table can be driven from a struct holding one canned answer and no
/// network at all -- the same seam, and the same reason, as
/// `crate::reword::Rewriter` itself.
type RewriterFn = dyn Fn(&RewordConfig) -> Result<Arc<dyn Rewriter>, RewordError> + Send + Sync;
type RewriterFactory = Arc<RewriterFn>;

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
    /// How the Test row builds a client: the real one in the daemon, a
    /// canned answer in this module's own tests.
    rewriter_factory: RewriterFactory,
    /// The daemon's tokio runtime, so `test_reword` can put its blocking
    /// request on the blocking pool exactly as §2's path does.
    ///
    /// `Option` because `new` is also called from unit tests with no
    /// runtime in scope. In the daemon it is always `Some`: `new` is called
    /// from `run_daemon`, which runs on the runtime.
    runtime: Option<tokio::runtime::Handle>,
}

impl SettingsModel {
    pub fn new(store: Arc<ConfigStore>, models_dir: PathBuf, current: Config) -> Self {
        Self::new_with_rewriter(
            store,
            models_dir,
            current,
            Arc::new(crate::reword::build_rewriter),
        )
    }

    /// [`SettingsModel::new`] with the Test row's client injected, so every
    /// row of §6's Test table can be driven with no network.
    pub fn new_with_rewriter(
        store: Arc<ConfigStore>,
        models_dir: PathBuf,
        current: Config,
        rewriter_factory: RewriterFactory,
    ) -> Self {
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
            rewriter_factory,
            runtime: tokio::runtime::Handle::try_current().ok(),
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

    /// Suggestions for the notification allowlist editor's "add an
    /// application" picker: applications `sayd` has actually watched notify
    /// this run (most recent first, `seen: true`), then [`CURATED`]
    /// (`seen: false`) -- filtered to drop anything already on the
    /// allowlist, and deduplicated so an application that is both recently
    /// seen and in the curated table is offered once, as the seen one.
    ///
    /// Seen entries sort first: the application a user opens this picker to
    /// add is almost always the one that just interrupted them, and a
    /// curated entry is a guess by construction (see `CURATED`'s doc
    /// comment) -- an educated one, but still second-best to what the
    /// application actually just sent.
    ///
    /// Both the allow-filter and the dedupe fold case, matching
    /// `allow_contains` (in turn matching `notify::policy::is_allowed`):
    /// "signal" already on the allowlist must hide a suggestion spelled
    /// "Signal", or the picker offers to add something that changes
    /// nothing.
    ///
    /// A seen entry keeps the icon the application actually sent rather
    /// than `CURATED`'s guess -- the whole reason `notify::seen` records an
    /// icon at all is to show the real one once one has been observed.
    pub fn suggestions(&self) -> Vec<Suggestion> {
        let cfg = self.current();
        // Case-folded app names already placed in `out`: what makes "seen
        // wins over curated" true, and also guards a source duplicating
        // itself (`notify::seen` already dedupes internally, but nothing
        // stops two `CURATED` rows from clashing after case-folding, and
        // `the_curated_table_is_well_formed` only checks that -- this is
        // the belt to that test's suspenders).
        let mut included: HashSet<String> = HashSet::new();
        let mut out = Vec::new();

        for app in seen::snapshot() {
            if allow_contains(&cfg, &app.app_name) {
                continue;
            }
            if !included.insert(app.app_name.trim().to_lowercase()) {
                continue;
            }
            out.push(Suggestion {
                icons: icon_candidates(&app),
                app_name: app.app_name,
                kind: SuggestionKind::Seen,
            });
        }

        for (name, icon) in CURATED {
            if allow_contains(&cfg, name) {
                continue;
            }
            if !included.insert(name.trim().to_lowercase()) {
                continue;
            }
            out.push(Suggestion {
                app_name: (*name).to_string(),
                icons: {
                    let mut icons = Vec::new();
                    push_candidate(&mut icons, icon);
                    icons
                },
                kind: SuggestionKind::Curated,
            });
        }

        out
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

    /// The Reword group's description, against the real environment and
    /// this model's own idea of the current config.
    ///
    /// Called once, at build time, when there is no redraw-supplied `Config`
    /// yet to describe. The redraw closure itself calls
    /// [`Self::reword_description_for`] with the `Config` it was handed
    /// instead -- see that method for why the two must not collapse into
    /// one.
    pub fn reword_description_now(&self) -> String {
        self.reword_description_for(&self.current())
    }

    /// The Reword group's description for a specific config, against the
    /// real environment.
    ///
    /// Every call site today passes `self.current()` -- either directly, via
    /// [`Self::reword_description_now`], or as the `cfg` a row redraw is
    /// handed, which is always the model's current config too (see
    /// [`Ui::redraw`] in `window.rs`). But the redraw closure is handed that
    /// `Config` for a reason: a description built by re-reading `current()`
    /// instead would happen to agree with every call site today and still
    /// describe the wrong config the day one doesn't, showing a destination
    /// or a key sentence that does not match the rows next to it. Taking
    /// `cfg` as a parameter is what makes that impossible rather than
    /// merely untested.
    pub fn reword_description_for(&self, cfg: &Config) -> String {
        let cfg = cfg.reword.clone();
        // The two halves of `resolve_api_key_with`'s environment rule, kept
        // in step with it deliberately: an empty `api_key_env` names no
        // variable and is never looked up (here), and a variable that is set
        // but empty is not a key (in `reword_description`, which has to make
        // that call anyway for the value a test hands it).
        let from_env = if cfg.api_key_env.is_empty() {
            None
        } else {
            std::env::var(&cfg.api_key_env).ok()
        };
        reword_description(&cfg, from_env.as_deref())
    }

    /// Run one rewrite against the *pending* config and report what
    /// happened.
    ///
    /// Returns immediately with a receiver. The caller -- the glib main
    /// thread -- awaits it with `glib::spawn_future_local`, so nothing
    /// blocks the UI; the receiver being dropped when the window closes
    /// mid-flight simply discards the delivery, and the blocking job ends on
    /// its own at `REWORD_HTTP_CEILING` at the latest.
    ///
    /// The *pending* config, not the last-written file: otherwise a user who
    /// types a key and immediately presses Test is told their old key is
    /// rejected, which is the single most confusing thing this row could do.
    pub fn test_reword(&self, text: String) -> async_channel::Receiver<TestOutcome> {
        // Bounded at one: there is exactly one outcome, and the button is
        // disabled until it arrives.
        let (tx, rx) = async_channel::bounded(1);
        let cfg = *self.current().reword;
        let factory = self.rewriter_factory.clone();
        let run = move || {
            let _ = tx.try_send(run_reword_test(&cfg, text, factory.as_ref()));
        };
        match &self.runtime {
            Some(handle) => {
                handle.spawn_blocking(run);
            }
            // Unit tests only; the daemon always has a runtime. A named
            // thread rather than an unnamed one so a stuck provider is
            // identifiable in a backtrace.
            None => {
                let _ = std::thread::Builder::new()
                    .name("reword-test".into())
                    .spawn(run);
            }
        }
        rx
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

/// What `speed_mode` runs as when it is left unset or holds a value this
/// build does not know -- `"model"`, today's behaviour, matching
/// `Config::default`. Taken from `SPEED_MODES[0]` for the same
/// cannot-drift reason as [`FALLBACK_MODEL`].
const FALLBACK_SPEED_MODE: &str = SPEED_MODES[0].0;

fn known_models() -> String {
    MODELS
        .iter()
        .map(|(v, _)| *v)
        .collect::<Vec<_>>()
        .join(", ")
}

fn known_speed_modes() -> String {
    SPEED_MODES
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
    let timeout = cfg
        .reword
        .timeout_ms
        .clamp(REWORD_TIMEOUT_MIN as u64, REWORD_TIMEOUT_MAX as u64);
    if timeout != cfg.reword.timeout_ms {
        warnings.push(format!(
            "reword.timeout_ms {} is outside {}-{}; using {timeout}",
            cfg.reword.timeout_ms, REWORD_TIMEOUT_MIN as u64, REWORD_TIMEOUT_MAX as u64
        ));
        cfg.reword.timeout_ms = timeout;
    }
    // The one range in this table that is not about taste, and the one that
    // was enforced in `Config::load_str` alone. Without it here the settings
    // window could write a `cooldown_secs = 2` that silently became 3 the
    // next time the file was read: a file that disagrees with the running
    // config, which is the exact drift the `REWORD_TIMEOUT_MIN/MAX` pair
    // above exists to prevent, in the same function. `0` is exempt because
    // it means something else entirely -- `Limiter::decide`'s
    // `cooldown_secs == 0` arm switches rate limiting off, so no coalescing
    // window ever opens and the ordering the floor protects does not exist.
    // See `sayd_core::config::NOTIFY_COOLDOWN_MIN_SECS` for what it protects.
    let floor = sayd_core::config::NOTIFY_COOLDOWN_MIN_SECS;
    if cfg.notifications.cooldown_secs != 0 && cfg.notifications.cooldown_secs < floor {
        warnings.push(format!(
            "notifications.cooldown_secs {} is shorter than a rewrite may take; using {floor}",
            cfg.notifications.cooldown_secs
        ));
        cfg.notifications.cooldown_secs = floor;
    }
    let max_chars = cfg
        .reword
        .max_chars
        .clamp(REWORD_MAX_CHARS_MIN as usize, REWORD_MAX_CHARS_MAX as usize);
    if max_chars != cfg.reword.max_chars {
        warnings.push(format!(
            "reword.max_chars {} is outside {}-{}; using {max_chars}",
            cfg.reword.max_chars, REWORD_MAX_CHARS_MIN as usize, REWORD_MAX_CHARS_MAX as usize
        ));
        cfg.reword.max_chars = max_chars;
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
    if !SPEED_MODES.iter().any(|(v, _)| *v == cfg.speed_mode) {
        return Err(format!(
            "'{}' is not a speed mode this build knows; expected one of {}",
            cfg.speed_mode,
            known_speed_modes()
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
    if !SPEED_MODES.iter().any(|(v, _)| *v == cfg.speed_mode) {
        warnings.push(format!(
            "'{}' is not a speed mode this build knows (expected one of {}); running {} instead",
            cfg.speed_mode,
            known_speed_modes(),
            FALLBACK_SPEED_MODE
        ));
        cfg.speed_mode = FALLBACK_SPEED_MODE.to_string();
    }
    warnings
}

/// One row of §6's Test table.
///
/// Every variant carries data, never a formatted sentence: [`TestOutcome::title`]
/// and [`TestOutcome::subtitle`] are where the wording lives, and they are
/// here rather than in `window.rs` because this is the layer with tests. §8's
/// table governs what the *daemon* does; this governs what the *user* is
/// told, and the two are not the same thing -- every row of §8 degrades to
/// "speak the original", which is correct and indistinguishable from the
/// feature being switched off. A typo in the endpoint, a stale key and a
/// model the provider does not have all produce a daemon that behaves
/// exactly as it did before, with nothing in the window to look at. These
/// distinctions *are* the row: a rejected key, an unreachable host and a
/// timeout have three different fixes.
#[derive(Debug, Clone)]
pub enum TestOutcome {
    /// Answered, passed the guard, inside the configured deadline.
    Rewritten {
        text: String,
        elapsed: Duration,
        deadline: Duration,
        /// The first request against this endpoint this run, so the number
        /// includes DNS and a TLS handshake.
        first: bool,
    },
    /// Answered and passed the guard, but slower than the deadline -- so a
    /// real notification would have been spoken as written.
    Slower {
        text: String,
        elapsed: Duration,
        deadline: Duration,
        /// As [`TestOutcome::Rewritten`]'s, and it matters more here: a
        /// first request to a remote endpoint pays for DNS and a TLS
        /// handshake, so without this the row would tell a user their
        /// provider is too slow on the strength of a number that will not
        /// happen again.
        first: bool,
    },
    /// Answered, and §3's guard threw it away. The answer is still shown:
    /// that is how a user discovers their model likes to explain itself.
    ///
    /// `answer` is the candidate exactly as it came off the wire -- untrimmed
    /// and unbounded. [`TestOutcome::title`] is what makes it fit in a row --
    /// see `shown_answer` -- and note that nothing but the guard's own
    /// `Oversized` check bounds what can be in here.
    Rejected {
        answer: String,
        reason: sayd_core::reword::Rejection,
    },
    AuthRejected {
        status: u16,
        host: String,
        env_var: String,
        message: Option<String>,
    },
    /// Nothing answered: DNS, the connection or the transport failed.
    Unreachable {
        detail: String,
        endpoint: String,
    },
    /// Something answered, and it was not a chat completion this client can
    /// read -- no `choices[0].message.content` in the body.
    ///
    /// Its own variant rather than a second spelling of
    /// [`TestOutcome::Unreachable`], because the two send a user to different
    /// places. "Could not reach the provider" is an instruction to check that
    /// the server is up and that no firewall is in the way; a reverse proxy
    /// answering its own error page, a health endpoint, or Ollama's native
    /// `/api/chat` on a URL that should have been `/v1` is a server that is
    /// up, reachable, and answering -- and every minute spent on the first
    /// investigation is a minute not spent looking at the URL. The subtitle
    /// is the same shape as `Unreachable`'s, because the detail and the
    /// endpoint are what both rows have to show; it is the *title* that must
    /// not claim something the request disproved.
    Unusable {
        detail: String,
        endpoint: String,
    },
    NoSuchModel {
        status: u16,
        model: String,
        message: Option<String>,
    },
    RateLimited {
        retry_after: Option<Duration>,
    },
    /// The client's own ceiling was hit.
    NoAnswer {
        ceiling: Duration,
        deadline: Duration,
    },
    /// `base_url` is empty or unusable.
    NotConfigured {
        reason: String,
    },
    /// Built without the `reword` feature.
    Unavailable,
    /// Both permits were in use. Rare, and only reachable by hammering the
    /// button while notifications are being rewritten.
    Busy,
}

/// `840 ms`, `2.4 s`. The crossover is at exactly one second: below it a
/// whole number of milliseconds is the readable form, above it a decimal
/// second is.
fn human_elapsed(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        format!("{} ms", d.as_millis())
    } else {
        format!("{:.1} s", d.as_secs_f64())
    }
}

/// `1.5 s`. Always seconds, because a deadline is a setting the user chose
/// in a spin row and reads as a number of seconds even when it is 200 ms.
fn human_deadline(d: Duration) -> String {
    format!("{:.1} s", d.as_secs_f64())
}

/// What a first-ever request against an endpoint owes the number beside it.
///
/// Appended to both timing rows rather than only the fast one: connection
/// setup is exactly what makes a first request slow, so the row that says a
/// provider missed its deadline is the one that most needs to say the
/// measurement included a handshake.
fn first_note(first: bool) -> &'static str {
    if first {
        " (first request — includes connection setup)"
    } else {
        ""
    }
}

/// The longest a provider's own answer may be when it is put in front of a
/// user as a row title.
///
/// Generous enough to show a chatty model's whole preamble -- which is the
/// thing the [`TestOutcome::Rejected`] row exists to let a user recognise --
/// and short enough that the row stays a row. Characters and not bytes,
/// because it bounds what is *read*.
///
/// Measured, not arbitrary: a 201-character title -- one character past this
/// cap -- occupies 3 lines and 47 px in a row that is otherwise 92 px tall,
/// which is exactly what "the row stays a row" cashes out to.
///
/// [`TestOutcome::Rewritten`] and [`TestOutcome::Slower`] carry a title (the
/// rewrite itself) with no cap of their own, and that is deliberate rather
/// than an oversight: a rewrite is bounded by what the user typed into the
/// Test field, not by a provider's own unbounded free text the way a
/// rejected answer is, so it cannot run further than its own input already
/// did. Which also means the Test field is the escape hatch this cap does
/// not close: a user who pastes 5,000 characters in and gets a `Rewritten`
/// row back has produced a very tall row by their own hand, not by anything
/// a provider volunteered.
const ANSWER_DISPLAY_MAX_CHARS: usize = 200;

/// The result row's title while a request is outstanding.
///
/// Named here rather than in `window.rs`: it is user-facing result-row
/// content exactly like every [`TestOutcome`] title and subtitle, and this
/// module's rule is that none of that is authored in the window -- there is
/// no reason "no outcome has arrived yet" should be the one exception.
pub const TEST_IN_PROGRESS_TITLE: &str = "Testing…";

/// The result row's title when the receiver was dropped without an answer.
///
/// That happens when the model's own thread died mid-request; there is
/// nothing to diagnose from the window, only to say plainly.
pub const TEST_INCOMPLETE_TITLE: &str = "The test did not complete";

/// A provider's answer, made fit to be a row title.
///
/// Two things, both of which the window is forbidden to decide for itself:
///
/// - **Trimmed.** [`TestOutcome::Rejected`] carries the candidate as it
///   arrived, before `sayd_core::reword::check` trims it -- which is the
///   right thing to carry, since the leading blank lines are part of what the
///   user is being shown. But a model answering `"\n\nSure!\nHere:"` would
///   otherwise put two empty lines at the top of the row and push the text
///   the row is *about* out of sight.
/// - **Bounded.** Nothing between the guard and here caps this. The guard's
///   `Oversized` check rejects a candidate past four bytes per character of
///   its ceiling -- for the default test text that is about 372 bytes -- and
///   the HTTP client caps its read at 64 KiB, so anything in between reaches
///   this function verbatim. A local model with a runaway generation against
///   a server that ignores `max_tokens` is the ordinary way to get there, and
///   that user is exactly who this row is for: they need to see that the
///   model is rambling, not to have the rambling pasted into their window.
///
/// The ellipsis is a single `…` and is added only when something was
/// actually cut, so a 200-character answer does not read as a truncated one.
fn shown_answer(answer: &str) -> String {
    let answer = answer.trim();
    match answer.char_indices().nth(ANSWER_DISPLAY_MAX_CHARS) {
        // `char_indices` gives a boundary by construction, so this cannot
        // split a multi-byte character.
        Some((end, _)) => format!("{}…", &answer[..end]),
        None => answer.to_string(),
    }
}

// The Reword group's result row is the only caller of these two. They are
// here, with a test each, because `window.rs` is the one layer with no test
// coverage and so must not decide anything -- it maps a variant onto two
// labels and a visibility and holds no rule of its own. That includes the
// truncation in `title()`: a window that decided for itself how much of a
// provider's answer to show would be deciding it untested.
impl TestOutcome {
    /// The result row's title: the rewritten text where there is one,
    /// because the rewritten text is the point.
    pub fn title(&self) -> String {
        match self {
            TestOutcome::Rewritten { text, .. } | TestOutcome::Slower { text, .. } => text.clone(),
            // Trimmed and bounded here rather than in the window: see
            // `shown_answer`, and note that this is a row title in a GTK
            // label, fed by a string a provider chose.
            TestOutcome::Rejected { answer, .. } => shown_answer(answer),
            TestOutcome::AuthRejected { .. } => "The provider rejected the API key".into(),
            TestOutcome::Unreachable { .. } => "Could not reach the provider".into(),
            // Not "could not reach": it was reached, and it answered. See
            // [`TestOutcome::Unusable`].
            TestOutcome::Unusable { .. } => "The endpoint answered something unusable".into(),
            TestOutcome::NoSuchModel { .. } => "The provider does not have that model".into(),
            TestOutcome::RateLimited { .. } => "The provider is rate limiting".into(),
            TestOutcome::NoAnswer { ceiling, .. } => {
                format!("No answer after {}", human_elapsed(*ceiling))
            }
            TestOutcome::NotConfigured { .. } => "The endpoint is not usable".into(),
            TestOutcome::Unavailable => "This build has no rewording client".into(),
            TestOutcome::Busy => "Another rewrite is still running".into(),
        }
    }

    /// The result row's subtitle: the number, and what it means.
    pub fn subtitle(&self) -> String {
        match self {
            // The deadline is named on the *good* row too, and not only on
            // `Slower`. "840 ms" alone does not answer the question the
            // number is for -- a user who has never seen this before does
            // not know what it is being measured against, and the row is the
            // only place they could find out.
            TestOutcome::Rewritten {
                elapsed,
                deadline,
                first,
                ..
            } => format!(
                "Rewritten in {}, inside the {} deadline{}",
                human_elapsed(*elapsed),
                human_deadline(*deadline),
                first_note(*first)
            ),
            // The one sentence this whole row exists for. Nothing in this
            // project has measured end-to-end provider latency and nothing
            // else will: this is the only route by which anyone learns what
            // their own provider costs on their own hardware, and the
            // comparison against the configured deadline is the only thing
            // that answers the question the number is for -- is my deadline
            // long enough?
            TestOutcome::Slower {
                elapsed,
                deadline,
                first,
                ..
            } => format!(
                "Rewritten in {} — longer than the {} deadline, so a real \
                 notification would have been spoken as written{}",
                human_elapsed(*elapsed),
                human_deadline(*deadline),
                first_note(*first)
            ),
            TestOutcome::Rejected { reason, .. } => {
                format!("Rejected: {} — spoken as written", reason.phrase())
            }
            TestOutcome::AuthRejected {
                status,
                host,
                env_var,
                message,
            } => {
                format!(
                    "HTTP {status} from {host}{} — check the key, or {env_var} if it is set",
                    parenthesised(message)
                )
            }
            // One shape for both: what went wrong, and the endpoint it went
            // wrong against. The titles are what tell them apart.
            TestOutcome::Unreachable { detail, endpoint }
            | TestOutcome::Unusable { detail, endpoint } => format!("{detail} — {endpoint}"),
            TestOutcome::NoSuchModel {
                status,
                model,
                message,
            } => {
                format!(
                    "HTTP {status}{} — sent as ‘{model}’",
                    parenthesised(message)
                )
            }
            TestOutcome::RateLimited { retry_after } => match retry_after {
                Some(d) => format!("HTTP 429 — Retry-After: {} s", d.as_secs()),
                None => "HTTP 429".to_string(),
            },
            TestOutcome::NoAnswer { deadline, .. } => format!(
                "The deadline is {}, so this provider is not usable for notifications",
                human_deadline(*deadline)
            ),
            TestOutcome::NotConfigured { reason } => reason.clone(),
            TestOutcome::Unavailable => "Rebuild with --features reword to use this".to_string(),
            TestOutcome::Busy => "Both rewrite slots are in use; try again in a moment".to_string(),
        }
    }

    /// The text worth speaking aloud, if this outcome has one.
    ///
    /// `None` for every row whose title is a status sentence about the
    /// transport or the button rather than something a provider wrote --
    /// `window.rs` maps that onto Speak being hidden, because a click on
    /// "Both rewrite slots are in use" would load a ~1.27 GB ORT session to
    /// say a sentence about the button, not about any text a provider
    /// produced.
    ///
    /// [`TestOutcome::Rejected`] is included on purpose, and not run through
    /// [`shown_answer`]'s 200-character cap the way its own title is:
    /// hearing what the model actually wrote, in full, is that row's whole
    /// point, and the cap on the title exists only to keep a *row* a row --
    /// it says nothing about how much of the answer is worth playing back.
    /// It is still trimmed, for the same reason the title is: a model that
    /// opens with blank lines should not be heard pausing before it speaks.
    pub fn speech(&self) -> Option<String> {
        match self {
            TestOutcome::Rewritten { text, .. } | TestOutcome::Slower { text, .. } => {
                Some(text.clone())
            }
            TestOutcome::Rejected { answer, .. } => Some(answer.trim().to_string()),
            TestOutcome::AuthRejected { .. }
            | TestOutcome::Unreachable { .. }
            | TestOutcome::Unusable { .. }
            | TestOutcome::NoSuchModel { .. }
            | TestOutcome::RateLimited { .. }
            | TestOutcome::NoAnswer { .. }
            | TestOutcome::NotConfigured { .. }
            | TestOutcome::Unavailable
            | TestOutcome::Busy => None,
        }
    }
}

/// A provider's own message, in brackets, or nothing at all.
///
/// It has already been cut and de-fanged on the way out of `http.rs`, which
/// is what makes it safe to put in front of a user at all.
fn parenthesised(message: &Option<String>) -> String {
    message
        .as_deref()
        .map(|m| format!(" ({m})"))
        .unwrap_or_default()
}

/// The Reword group's description: where text goes, and where the key comes
/// from.
///
/// `env_value` is passed in rather than read here so this can be tested
/// without touching process-global environment state. It names the variable
/// even when it is unset, because a user who exports `SAYD_REWORD_API_KEY`
/// and then sees an empty password field would otherwise conclude the
/// feature is unconfigured.
///
/// It also says, plainly, that Test is a network call: §7 requires the one
/// send that happens with `enabled = false` to be named where the button is.
fn reword_description(cfg: &RewordConfig, env_value: Option<&str>) -> String {
    let destination = match sayd_core::reword::parse_base_url(&cfg.base_url) {
        Ok(endpoint) => endpoint.host,
        // Said here because §8 makes it a silent degradation everywhere
        // else: an unparseable `base_url` speaks the text as written and
        // logs one line the user is not reading.
        Err(_) => {
            return format!(
                "‘{}’ is not a usable endpoint, so text is spoken as written. \
                 Pressing Test below is itself a network call.",
                cfg.base_url
            )
        }
    };
    let key = match (env_value.filter(|v| !v.is_empty()), cfg.api_key.is_empty()) {
        (Some(_), _) => format!(
            "The API key comes from {}, not from the field below.",
            cfg.api_key_env
        ),
        (None, false) => "The API key comes from this config file.".to_string(),
        (None, true) => "No API key is set; local servers ignore it.".to_string(),
    };
    format!(
        "Sends the text about to be spoken to {destination}. {key} \
         Pressing Test below is itself a network call."
    )
}

/// Whether the API key row has anything to offer for this endpoint.
///
/// The rule, not the widget: a credential field in front of a server that
/// takes no credential is an invitation to put a secret on disk for nothing,
/// and `config.toml` is a file the settings window rewrites wholesale (which
/// is why `api_key_env` is the documented way to hold one at all). A
/// loopback endpoint is *usually* the case where that is certain --
/// `is_loopback` is the same name-based test `Config::load_str` uses to
/// decide whether plain HTTP earns a warning, so the two cannot disagree
/// about what "this machine" means -- but "on this machine" and "takes no
/// credential" are not the same claim, and §6's own endpoint table says so:
/// vLLM's row is a loopback `base_url` with Key **"as configured"**, because
/// `vllm serve --api-key sk-…` (and a local LiteLLM or llama.cpp built with
/// auth) are real invocations. [`ENDPOINT_PRESETS`]' third field carries
/// that column, and this function checks it before falling back to the
/// loopback test -- so vLLM's preset keeps the row while Ollama, llama.cpp
/// `server` and LM Studio still lose it, exactly as their "ignored" cells
/// say.
///
/// Three things it deliberately does *not* do:
///
/// - **It never hides a key that is already stored.** A non-empty `api_key`
///   keeps the row visible whatever the endpoint is, because the row is the
///   only way to read or clear it; hiding a secret the file still holds
///   would be worse than showing a field nobody needs.
/// - **It errs towards visible.** An unparseable `base_url` is not a
///   loopback claim, so the row stays: the user is mid-repair, and the
///   description already says the endpoint is unusable.
/// - **It does not try to recognise "a vLLM-shaped URL" in general.** The
///   exemption is an exact match against [`ENDPOINT_PRESETS`]' key-taking
///   rows -- what the preset button actually writes into `base_url` -- not
///   a guess at every host:port a user might run vLLM on. A user who points
///   `base_url` at vLLM by hand rather than through the preset and wants the
///   row back has the same escape the row exists to avoid needing: type the
///   key in once the row is visible (from the preset, or after fixing the
///   URL to match it), and the "already stored" exception above keeps it
///   visible from then on.
///
/// The environment is not consulted. `api_key_env` supplies the request's
/// key without this field being involved at all, and [`reword_description`]
/// is where that is said -- one sentence in the group description, rather
/// than a row that vanishes for a reason the user cannot see.
pub fn reword_key_row_applies(cfg: &RewordConfig) -> bool {
    if !cfg.api_key.is_empty() {
        return true;
    }
    let is_key_taking_preset = ENDPOINT_PRESETS
        .iter()
        .any(|(_, url, takes_key)| *takes_key && *url == cfg.base_url);
    if is_key_taking_preset {
        return true;
    }
    match sayd_core::reword::parse_base_url(&cfg.base_url) {
        Ok(endpoint) => !sayd_core::reword::is_loopback(&endpoint.host),
        Err(_) => true,
    }
}

/// One deliberate probe of the configured endpoint.
///
/// Four rules, all of them §6's:
///
/// - It bypasses §4's eligibility floor and ceiling. The user typed this
///   text deliberately; refusing to test `Ping` because it is under twelve
///   characters would be baffling.
/// - It applies §3's guard, and reports a rejection as its own outcome with
///   the answer that was rejected still shown -- that is how a user
///   discovers their model likes to explain itself.
/// - It bypasses the configured deadline, waiting the client's own
///   [`crate::reword::REWORD_TEST_CEILING`] and reporting the real elapsed
///   time. A test that gave up at `timeout_ms` could only ever say "too
///   slow" and never *how much*, which is the number needed to choose a
///   better deadline.
/// - It **ignores and does not update the circuit breakers**, so a user who
///   has just fixed a rejected key gets a real request rather than a cached
///   verdict. Structurally: nothing here calls [`crate::reword::RewordState::allow`]
///   or `record`. A *successful* test does call `clear_auth_latch`, which is
///   the only way to recover from a key supplied through the environment:
///   editing that does not change the config at all, so nothing else would
///   ever clear it.
///
/// It does still take a permit from the same pool of two, so a user
/// hammering the button cannot outrun the bound §2 establishes.
fn run_reword_test(cfg: &RewordConfig, text: String, factory: &RewriterFn) -> TestOutcome {
    let state = crate::reword::state();
    let Some(_permit) = state.try_permit() else {
        return TestOutcome::Busy;
    };
    let rewriter = match factory(cfg) {
        Ok(r) => r,
        Err(e) => return outcome_for_error(e, cfg),
    };
    // §7's privacy line, in front of the send and behind the permit, exactly
    // where `crate::reword::attempt` puts it: a deliberate button press is a
    // send like any other and is logged like one, and a run in which nothing
    // left the machine must not contain the line. Its return value is what
    // says whether this request pays for DNS and a handshake, taken from the
    // same call rather than from a separate `endpoint_seen` -- two calls
    // could disagree with a notification landing between them.
    let first = state.note_endpoint(cfg);
    let started = std::time::Instant::now();
    let answer = rewriter.reword(&text);
    let elapsed = started.elapsed();

    let deadline = Duration::from_millis(cfg.timeout_ms);
    match answer {
        Ok(candidate) => {
            // Before the guard, not after: an answer that the guard threw
            // away still proves the key works, which is the only thing this
            // latch is about.
            state.clear_auth_latch(cfg);
            match sayd_core::reword::check(&text, &candidate) {
                Ok(text) if elapsed > deadline => TestOutcome::Slower {
                    text,
                    elapsed,
                    deadline,
                    first,
                },
                Ok(text) => TestOutcome::Rewritten {
                    text,
                    elapsed,
                    deadline,
                    first,
                },
                Err(reason) => TestOutcome::Rejected {
                    answer: candidate,
                    reason,
                },
            }
        }
        Err(e) => outcome_for_error(e, cfg),
    }
}

/// Which row of §6's table a failure is. One variant per failure a user can
/// actually do something about, because "error" tells them nothing.
fn outcome_for_error(e: RewordError, cfg: &RewordConfig) -> TestOutcome {
    match e {
        RewordError::Auth {
            status,
            host,
            message,
        } => TestOutcome::AuthRejected {
            status,
            host,
            // Named even when it is unset: a user who exported one and then
            // edits the password field is editing the wrong thing.
            env_var: cfg.api_key_env.clone(),
            message,
        },
        RewordError::NoSuchModel {
            status,
            model,
            message,
        } => TestOutcome::NoSuchModel {
            status,
            model,
            message,
        },
        RewordError::RateLimited { retry_after, .. } => TestOutcome::RateLimited { retry_after },
        RewordError::Unreachable(detail) => TestOutcome::Unreachable {
            detail,
            endpoint: cfg.base_url.clone(),
        },
        RewordError::Ceiling => TestOutcome::NoAnswer {
            ceiling: crate::reword::REWORD_TEST_CEILING,
            deadline: Duration::from_millis(cfg.timeout_ms),
        },
        RewordError::NotConfigured(reason) => TestOutcome::NotConfigured { reason },
        RewordError::Unavailable => TestOutcome::Unavailable,
        // A body that came back and could not be used. *Not* an unreachable
        // provider: the request completed and something answered it, so a
        // row titled "could not reach" would send the user to check that the
        // server is running and that no firewall is in the way -- the one
        // investigation the answer has already ruled out. See
        // [`TestOutcome::Unusable`].
        RewordError::Malformed(detail) => TestOutcome::Unusable {
            detail,
            endpoint: cfg.base_url.clone(),
        },
    }
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
    // Bounded here because this is where an entry is *added*, and nothing
    // downstream bounds it: `sayd-core`'s `Config` takes whatever string it
    // is handed, so an `app_name` off the bus -- which is where a suggestion
    // row's name comes from, and which the sender chooses -- could be
    // written into `config.toml` a megabyte long and read back every time
    // the file is parsed. `MAX_APP_NAME_LEN` and not a number of its own:
    // the discovery log and the seen registry already truncate at that
    // length, so a suggestion row shows a bounded name and adding it stores
    // exactly the name that was shown.
    let name = truncate_chars(name.trim(), MAX_APP_NAME_LEN);
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

/// What an `app_icon` string actually refers to.
///
/// The notification spec allows three shapes for this one field: a
/// freedesktop icon *name*, looked up in the user's icon theme; a path to an
/// image file; or a `file://` URI wrapping that same path. Which one a given
/// application sends is not negotiable or discoverable ahead of time --
/// `notify::seen` records whatever string arrived verbatim -- so the window
/// cannot draw an icon without first classifying which of the three it has.
/// That classification is a rule, so, like everything else in this file, it
/// lives here rather than in `window.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconSource {
    /// A freedesktop icon theme name, e.g. `"signal-desktop"` or
    /// `"org.gnome.Fractal"` -- looked up through the icon theme, not
    /// loaded from disk directly.
    Named(String),
    /// A path to an image file -- loaded from disk directly rather than
    /// through a theme. Absolute in every case this codebase has actually
    /// seen, but a relative one is accepted the same way; see
    /// `icon_source`'s doc comment for why that costs nothing extra.
    File(PathBuf),
    /// Nothing to show: an empty or whitespace-only `app_icon`. Not every
    /// application sends one.
    None,
}

/// Classify one `app_icon` string, per [`IconSource`].
///
/// The freedesktop icon naming spec forbids a `/` in an icon *name* --
/// names are resolved through the theme, never as filesystem paths -- so any
/// string containing one is necessarily a path of some kind, whether or not
/// it happens to be absolute. That single check is what lets a relative path
/// (`"icons/x.png"`, say) fall out of the same rule as an absolute one,
/// without a second case for it.
///
/// A `file://` URI is unwrapped to the path it names first, since it would
/// otherwise be caught by that same `/`-means-a-path rule and turned into a
/// nonsense `File` whose path starts with `"file:"`. Only the two authority
/// forms real senders actually produce are handled -- an empty one
/// (`file:///abs/path`) and `file://localhost/abs/path`, both meaning "this
/// host" per RFC 8089 -- because that is what covers everything this
/// codebase has seen a `file://` icon carry. A URI naming some *other* host
/// has no local path to hand back and is not something a notification icon
/// would plausibly carry, so it is deliberately left unhandled (it falls
/// through to the `/`-means-a-path rule above and becomes a `File` with a
/// nonsense leading `host/` segment -- wrong, but no worse than the icon a
/// theme lookup on a bare URI would have produced anyway). Percent-encoded
/// characters are likewise left undecoded. Both are a "not seen in practice,
/// so not worth the code" judgement call, not an oversight -- either would
/// need to actually appear in some application's `app_icon` before it earns
/// a decoder.
pub fn icon_source(app_icon: &str) -> IconSource {
    let trimmed = app_icon.trim();
    if trimmed.is_empty() {
        return IconSource::None;
    }
    if let Some(rest) = trimmed.strip_prefix("file://") {
        // `"localhost/"` and not `"localhost"` (Minor): the authority ends
        // at the slash that begins the path, so stripping the bare word
        // also matched `file://localhostile/x` -- a URI naming the host
        // `localhostile` -- and handed back `/x`, a path on this machine
        // that the sender never named. Matching the slash as part of the
        // prefix and keeping it is what makes the result an absolute path
        // rather than a relative one.
        let path = match rest.strip_prefix("localhost") {
            Some(after_host) if after_host.starts_with('/') => after_host,
            _ => rest,
        };
        return IconSource::File(PathBuf::from(path));
    }
    if trimmed.contains('/') {
        return IconSource::File(PathBuf::from(trimmed));
    }
    IconSource::Named(trimmed.to_string())
}

/// Every icon string a seen application has given us, classified, best
/// first, with the ones that name nothing dropped.
///
/// CRITICAL 1: `app_icon` alone is empty for essentially every real sender
/// -- see `notify::decode::Notification`, which records what was measured
/// against a real bus -- so a window drawn from it showed the fallback glyph
/// for every row, forever. The order is what a sender is most likely to have
/// filled with something that resolves:
///
/// 1. `desktop-entry`, an application id (`org.gnome.Fractal`) that icon
///    themes are indexed by, sent by every GLib `GNotification`. It names
///    the *application*, which is exactly what a suggestion row is about,
///    and it cannot be a temporary file the sender has already deleted.
/// 2. `image-path`, which is what `notify-send -i` and `GNotification`'s
///    own icon end up in. Either shape -- it carries a bare theme name as
///    often as a path, which is why it goes through `icon_source` like
///    everything else here rather than being assumed to be a file.
/// 3. `app_icon`, the argument the specification nominally puts this in.
///    Last because the applications that do fill it are, measurably, the
///    ones most likely to have filled it with a generic dialog glyph or a
///    temporary file they then delete.
///
/// A list rather than one winner, because "does this resolve?" is not a
/// question this layer can answer: whether the theme has a name and whether
/// a path is still there both need the display and the filesystem, which is
/// `window.rs`'s half of the job. This decides what to try and in what
/// order; the window takes the first that draws.
pub fn icon_candidates(app: &SeenApp) -> Vec<IconSource> {
    let mut out = Vec::new();
    for candidate in [&app.desktop_entry, &app.image_path, &app.app_icon] {
        push_candidate(&mut out, candidate);
    }
    out
}

/// Classify one icon string onto the end of `out`, skipping what names
/// nothing and what is already there.
///
/// A theme *name* is pushed twice: as sent, and then with `-symbolic`
/// appended. That is not speculative -- measured against Adwaita 50, the
/// theme has `mail-unread-symbolic` and `dialog-information-symbolic` but
/// neither `mail-unread` nor `dialog-information`, and those are exactly the
/// strings a GLib `GNotification` and a `notify-send -i` put in `image-path`.
/// Without the second form, a sender that named a perfectly ordinary
/// freedesktop icon still gets the fallback glyph on a stock GNOME desktop,
/// which is most of what CRITICAL 1 was about. Second and not first, because
/// where a theme has both, the application's own full-colour icon is the one
/// worth showing.
fn push_candidate(out: &mut Vec<IconSource>, raw: &str) {
    let source = icon_source(raw);
    if let IconSource::Named(name) = &source {
        let symbolic = IconSource::Named(format!("{name}-symbolic"));
        if !out.contains(&source) {
            out.push(source.clone());
        }
        if !name.ends_with("-symbolic") && !out.contains(&symbolic) {
            out.push(symbolic);
        }
        return;
    }
    if source != IconSource::None && !out.contains(&source) {
        out.push(source);
    }
}

/// Whether an icon file is small enough to decode on the main thread.
///
/// CRITICAL 3: the window loads a suggestion's icon from a path the
/// *sending application* chose, synchronously, on the glib main thread, once
/// per row. Measured through the same gdk-pixbuf path GTK uses, a 435 KB PNG
/// declaring 12000x12000 pixels decodes in 442 ms and 432 MB; at `MAX_SEEN`
/// rows that is a frozen desktop and tens of gigabytes, rebuilt on every
/// redraw, and it costs the sender one line to arrange. A byte limit alone
/// does not catch it -- that is what "435 KB" is doing in that sentence --
/// so the pixel count is the limit that matters and the byte count is the
/// cheap one that bounds everything else.
///
/// Both are far above any real icon: application icons top out at 512
/// pixels a side in every theme this has been checked against, and the
/// largest icon PNG in Adwaita is a few tens of kilobytes.
///
/// Pure, and here rather than in `window.rs`, for the reason everything else
/// is: it is a rule, and the window is the layer with no test coverage. The
/// two questions that need a filesystem -- how big is the file, what does
/// its header say -- are the window's to ask.
///
/// Two functions rather than one because they are answered at different
/// prices: `stat` gives the byte count, while learning the dimensions means
/// handing the file to an image loader (20 ms for the bomb above, measured),
/// so the window asks this one first and only pays for the second on a file
/// that passed.
pub fn icon_file_size_within_limit(bytes: u64) -> bool {
    bytes <= MAX_ICON_FILE_BYTES
}

/// Is a declared image size one this thread can afford to decode? See
/// [`icon_file_size_within_limit`], which is asked first.
///
/// A zero or negative dimension is out: it is not an image anybody can draw,
/// and it is what a loader reports for a header it could not make sense of.
pub fn icon_pixels_within_limit(width: i32, height: i32) -> bool {
    width > 0 && height > 0 && width <= MAX_ICON_PIXELS && height <= MAX_ICON_PIXELS
}

/// Largest icon file, in bytes, [`icon_file_size_within_limit`] will accept.
pub const MAX_ICON_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Largest icon, in pixels a side, [`icon_pixels_within_limit`] will accept.
pub const MAX_ICON_PIXELS: i32 = 1024;

/// Which of `suggestions()`'s two sources a [`Suggestion`] came from.
///
/// A two-variant enum rather than the bare `bool` this was, because both
/// this type and the window's group table read as a question with no
/// question mark otherwise -- `s.seen == kind`, and a `[(bool, &str, &str)]`
/// whose first column has to be looked up to be understood.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionKind {
    /// An application `sayd` has actually watched notify this run (see
    /// `notify::seen`).
    Seen,
    /// One drawn from [`CURATED`].
    Curated,
}

/// One row the notification allowlist editor's "add an application" picker
/// could offer.
///
/// The window uses `kind` to label the two sources differently rather than
/// the model picking a rendering, which would put a rule back in the one
/// layer meant to hold none.
///
/// `icons` is every icon string this suggestion could be drawn from, best
/// first -- see [`icon_candidates`] for why there is more than one and what
/// decides the order. It can be empty: not every application sends an icon
/// in any of the three fields, and `notify-send` sends none in any of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub app_name: String,
    pub icons: Vec<IconSource>,
    pub kind: SuggestionKind,
}

/// A short, hand-checked list of applications worth suggesting even when
/// they have never notified this run -- offered underneath whatever
/// `notify::seen` has actually observed (see `SettingsModel::suggestions`),
/// never in place of it.
///
/// Each `app_name` is exactly what the *application* itself passes to
/// `Notify`, not a marketing name: matching against the allowlist is literal
/// and case-insensitive (`allow_contains`'s rule), so a plausible-looking
/// guess that is not what the application actually sends would silently
/// never match anything -- worse than not suggesting the application at all,
/// because it looks like it worked. Checked against upstream `.desktop`
/// files, the packaging that ships them, and -- where the application is
/// open source -- the code that actually calls `Notify`/`notify_init`,
/// rather than trusted as given; Task 2's report says what was verified
/// against what and where a name is a reasoned guess instead.
///
/// The second column is a *theme icon name to draw the row with*, and that
/// is a weaker claim than the first column makes. It deliberately no longer
/// claims to be what the application passes as `app_icon`: measured against
/// a real bus, almost nothing passes `app_icon` at all (see
/// `notify::decode::Notification`). An empty second column means "this
/// application has no icon worth guessing", and the row falls back to the
/// generic glyph -- which is honest, where naming an icon that does not
/// exist is not.
///
/// `notify-send` is here deliberately: it is what a user reaches for to
/// test the feature, and it is the one entry whose *name* is guaranteed
/// correct, because `notify-send` really does pass its own program name
/// (`g_get_prgname()`, i.e. `"notify-send"`) as `app_name` whenever `-a` is
/// not given -- measured again here against a stub notification server. Its
/// icon column is empty for the same measurement's other half: it passes no
/// `app_icon`, so it has no icon of its own to show, and the
/// `utilities-terminal` this used to name was fiction twice over -- nothing
/// sends it, and Adwaita has not shipped an icon by that name in years
/// (checked against Adwaita 50, where the fallback
/// `application-x-executable-symbolic` does resolve).
pub const CURATED: &[(&str, &str)] = &[
    ("Signal", "signal-desktop"),
    ("Element", "element"),
    // `discord`, lowercase, and the same on stable, PTB and Canary --
    // measured from the shipped `app.asar`s of four builds (0.0.10 through
    // 0.0.700): the root `package.json` has no `productName`, so Electron
    // falls back to `name`, which is `"discord"`. `Constants.js` really does
    // build `'Discord' + channel_suffix`, but every use of it is a log line,
    // the Windows AppUserModelId, or a `.lnk` filename -- never
    // `app.setName`, which is what would reach `notify_init`. sayd matches
    // case-insensitively so `Discord` would work too; this is the string the
    // application actually sends, which is what this table promises.
    // Unverified for the current 1.0.x host, which ships no `app.asar` and
    // fetches itself at runtime.
    ("Fractal", "org.gnome.Fractal"),
    ("Telegram Desktop", "org.telegram.desktop"),
    ("discord", "discord"),
    // "Slack", capitalised: `productName` is present and is `"Slack"`,
    // measured from the 4.51.180 .deb, and its .desktop `Name=` agrees.
    ("Slack", "slack"),
    ("Thunderbird", "org.mozilla.Thunderbird"),
    ("evolution-mail-notification", "evolution"),
    ("Firefox", "firefox"),
    ("Nextcloud", "Nextcloud"),
    ("KeePassXC", "keepassxc"),
    ("Spotify", "spotify-client"),
    ("notify-send", ""),
];

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

    /// Same shape as `an_unknown_model_is_rejected_rather_than_silently_
    /// downgraded`: the window is about to write the value, so it can refuse
    /// one this build's `KokoroSynthesizer` would not recognise, rather than
    /// letting the file claim a speed mode nothing honours.
    #[test]
    fn an_unknown_speed_mode_is_rejected_rather_than_silently_downgraded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart"], dir.path());
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::new(store, models, Config::default());

        let err = m
            .edit(|c| c.speed_mode = "warp".into())
            .expect_err("must be rejected");
        assert!(
            err.contains("warp"),
            "the rejected value must appear: {err}"
        );
        assert_eq!(
            m.current().speed_mode,
            "model",
            "a rejected edit must not stick"
        );
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

    /// Same shape as `normalize_names_an_unknown_model_and_what_will_run_
    /// instead`, for `speed_mode`: the reload path cannot refuse a file the
    /// user already wrote, so it says what will actually run instead of
    /// silently falling back to `"model"`.
    #[test]
    fn normalize_names_an_unknown_speed_mode_and_what_will_run_instead() {
        let mut cfg = Config {
            speed_mode: "warp".into(),
            ..Config::default()
        };
        let warnings = normalize(&mut cfg);
        assert_eq!(
            cfg.speed_mode, FALLBACK_SPEED_MODE,
            "the config must say what runs"
        );
        assert_eq!(warnings.len(), 1, "one field, one warning: {warnings:?}");
        assert!(
            warnings[0].contains("warp"),
            "the rejected value must be named: {warnings:?}"
        );
        assert!(
            warnings[0].contains(FALLBACK_SPEED_MODE),
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

    /// A hand-edited `timeout_ms` past `sayd-cli`'s 3 s D-Bus bound must be
    /// clamped and *warned about*, not refused -- refusing would lock the
    /// user out of every unrelated settings row, which is exactly what
    /// `model = "int4"` used to do.
    #[test]
    fn out_of_range_reword_bounds_are_clamped_and_warned_about_not_rejected() {
        let mut cfg = Config::default();
        cfg.reword.timeout_ms = 9000;
        cfg.reword.max_chars = 1;
        let warnings = normalize(&mut cfg);
        assert_eq!(cfg.reword.timeout_ms, REWORD_TIMEOUT_MAX as u64);
        assert_eq!(cfg.reword.max_chars, 32);
        assert_eq!(warnings.len(), 2, "both clamps must say so: {warnings:?}");

        let mut cfg = Config::default();
        cfg.reword.timeout_ms = 9000;
        cfg.reword.base_url = "not a url at all".into();
        assert!(
            validate(&mut cfg).is_ok(),
            "an unusable base_url is a degradation reported by the Test row, \
             not a reason to refuse an unrelated edit"
        );
        assert_eq!(cfg.reword.timeout_ms, REWORD_TIMEOUT_MAX as u64);
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

    /// Same premise as `the_fallback_model_is_the_one_the_synthesizer_
    /// actually_loads`, for `speed_mode`: `FALLBACK_SPEED_MODE` must be what
    /// a config that says nothing about it gets, so normalising an unknown
    /// value and defaulting a fresh one land on the same behaviour.
    #[test]
    fn the_fallback_speed_mode_matches_config_default() {
        assert_eq!(FALLBACK_SPEED_MODE, Config::default().speed_mode);
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

        let notification = |app_name: &str| Notification {
            app_name: app_name.into(),
            desktop_entry: String::new(),
            image_path: String::new(),
            app_icon: String::new(),
            summary: "Ada: dinner?".into(),
            body: String::new(),
        };
        let mut limiter = Limiter::new();
        assert_ne!(
            limiter.decide(
                &notification("signal"),
                &cfg.notifications,
                std::time::Instant::now()
            ),
            Decision::NotAllowed,
            "a name the window added must be one the daemon speaks for"
        );
        // IMPORTANT 4: including the padded spelling the *bus* accepts, not
        // only the tidy one. `allow_contains` said "Signal " was already
        // allowed and `allow_add` stored "Signal"; if `is_allowed` did not
        // trim, the user's click removed the suggestion row and bought
        // nothing.
        assert_ne!(
            limiter.decide(
                &notification("Signal "),
                &cfg.notifications,
                std::time::Instant::now()
            ),
            Decision::NotAllowed,
            "the name the bus actually delivers must match the entry that was added for it"
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

    /// A sighting of `app_name` carrying `app_icon` and no hints, for the
    /// tests here that only care that *some* icon survived.
    fn notified(app_name: &str, app_icon: &str) -> crate::notify::Notification {
        crate::notify::Notification {
            app_name: app_name.to_string(),
            desktop_entry: String::new(),
            image_path: String::new(),
            app_icon: app_icon.to_string(),
            summary: "s".into(),
            body: "b".into(),
        }
    }

    /// The three shapes `app_icon` can actually arrive in, per the
    /// notification spec: a freedesktop icon name, an absolute path, or a
    /// `file://` URI wrapping one -- plus nothing at all, which is not a
    /// hypothetical case (not every application sends an icon).
    #[test]
    fn an_icon_is_classified_as_a_name_a_path_or_nothing() {
        assert_eq!(
            icon_source("signal-desktop"),
            IconSource::Named("signal-desktop".into())
        );
        assert_eq!(
            icon_source("org.gnome.Fractal"),
            IconSource::Named("org.gnome.Fractal".into())
        );
        assert_eq!(
            icon_source("/usr/share/icons/x.png"),
            IconSource::File("/usr/share/icons/x.png".into())
        );
        assert_eq!(
            icon_source("file:///usr/share/icons/x.png"),
            IconSource::File("/usr/share/icons/x.png".into())
        );
        assert_eq!(icon_source(""), IconSource::None);
        assert_eq!(icon_source("   "), IconSource::None);
    }

    /// Seen apps come first: the app a user is trying to allow is almost
    /// always the one that just notified them, and a curated entry is a
    /// guess by construction.
    ///
    /// `seen::record` writes into a process-global registry every test in
    /// this binary shares -- including `notify::monitor`'s real-bus tests,
    /// several of which call a real `Notify` with `app_name` `"Signal"` or
    /// `"Fractal"` and so, through `run`'s own `seen::record` call, race
    /// this test for those two names. This asserts the *relative* order of
    /// its own tagged entry against `"KeePassXC"`, a curated name nothing
    /// else in this binary ever records, rather than an absolute position
    /// or a name shared with those tests -- exactly the race `seen.rs`'s
    /// own `recording_again_refreshes_the_icon_and_moves_the_entry_ahead`
    /// had to be rewritten to avoid.
    #[test]
    fn seen_apps_are_listed_before_curated_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart"], dir.path());
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::new(store, models, Config::default());

        seen::record(&notified("sug1-JustNotified", "sug1-icon"));

        let suggestions = m.suggestions();
        let seen_pos = suggestions
            .iter()
            .position(|s| s.app_name == "sug1-JustNotified")
            .expect("the just-recorded app must be suggested");
        let curated_pos = suggestions
            .iter()
            .position(|s| s.app_name == "KeePassXC")
            .expect("a curated entry must be suggested");

        assert!(
            seen_pos < curated_pos,
            "a seen app must be listed before curated entries: seen at {seen_pos}, curated at {curated_pos}"
        );
        assert_eq!(suggestions[seen_pos].kind, SuggestionKind::Seen);
        assert_eq!(suggestions[curated_pos].kind, SuggestionKind::Curated);
        engine.shutdown();
    }

    /// A suggestion for something already allowed is noise, and the
    /// allowlist matches case-insensitively -- so the filter must too, or
    /// "signal" in the config still offers "Signal".
    ///
    /// Exercises both sources: a seen app already allowed under a different
    /// case, and a curated entry (`CURATED[0]` is `"Signal"`) already
    /// allowed under a different case.
    #[test]
    fn already_allowed_apps_are_not_suggested_whatever_their_case() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart"], dir.path());
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::new(store, models, Config::default());

        seen::record(&notified("sug2-Already", "sug2-icon"));
        m.edit(|c| {
            c.notifications.allow = vec!["SUG2-ALREADY".into(), "signal".into()];
        })
        .expect("edit succeeds");

        let suggestions = m.suggestions();
        assert!(
            !suggestions
                .iter()
                .any(|s| s.app_name.eq_ignore_ascii_case("sug2-Already")),
            "an already-allowed seen app must not be suggested, whatever its case"
        );
        assert!(
            !suggestions
                .iter()
                .any(|s| s.app_name.eq_ignore_ascii_case("signal")),
            "an already-allowed curated app must not be suggested, whatever its case"
        );
        engine.shutdown();
    }

    /// A seen app that is also curated appears once, as the seen one -- the
    /// seen entry carries the icon the application really sent.
    ///
    /// `"nextcloud"` here matches the curated `("Nextcloud", "Nextcloud")`
    /// case-insensitively but is recorded with a different icon, which is
    /// what distinguishes "the seen entry survived" from "the curated entry
    /// happened to look the same". Neither `"Signal"`/`"Fractal"`
    /// (`notify::monitor`'s real-bus tests also record those into this same
    /// process-global registry, same reason as `seen_apps_are_listed_
    /// before_curated_ones` above) nor `"KeePassXC"` (that test's own
    /// anchor -- picking the same name here would make the two race each
    /// other within this file).
    #[test]
    fn a_seen_app_that_is_also_curated_appears_once_and_keeps_its_own_icon() {
        let dir = tempfile::tempdir().expect("tempdir");
        let models = models_dir_with(&["af_heart"], dir.path());
        let (store, engine) = store_in(dir.path());
        let m = SettingsModel::new(store, models, Config::default());

        seen::record(&notified("nextcloud", "sug3-custom-icon"));

        let suggestions = m.suggestions();
        let matches: Vec<&Suggestion> = suggestions
            .iter()
            .filter(|s| s.app_name.eq_ignore_ascii_case("nextcloud"))
            .collect();

        assert_eq!(
            matches.len(),
            1,
            "must appear once, not once per source: {matches:?}"
        );
        assert_eq!(
            matches[0].kind,
            SuggestionKind::Seen,
            "the seen entry must win, not the curated one"
        );
        assert_eq!(
            matches[0].app_name, "nextcloud",
            "and keep its own spelling"
        );
        assert_eq!(
            matches[0].icons,
            vec![
                IconSource::Named("sug3-custom-icon".into()),
                IconSource::Named("sug3-custom-icon-symbolic".into()),
            ],
            "must keep the icon the application actually sent, not CURATED's guess"
        );
        engine.shutdown();
    }

    /// A `file://` URI's authority ends at the slash that starts the path
    /// (Minor). Stripping the bare word `localhost` also matched a URI
    /// naming the host `localhostile`, and handed back a path on *this*
    /// machine that the sender never named.
    #[test]
    fn only_a_real_localhost_authority_is_stripped_from_a_file_uri() {
        assert_eq!(
            icon_source("file://localhost/usr/share/icons/x.png"),
            IconSource::File("/usr/share/icons/x.png".into()),
            "a localhost authority is stripped, and the path stays absolute"
        );
        assert_eq!(
            icon_source("file://localhostile/x.png"),
            IconSource::File("localhostile/x.png".into()),
            "some other host is not localhost and must not become /x.png"
        );
    }

    /// CRITICAL 1: the icon a real sender actually supplies is in the
    /// `desktop-entry` or `image-path` hint, and `app_icon` -- the field
    /// this used to draw from alone -- is empty for essentially every one
    /// of them. Measured against a stub notification server on a private
    /// bus: a GLib `GNotification` from an application whose app-id is
    /// `org.gnome.Fractal` sends `app_icon = ""`, `desktop-entry =
    /// "org.gnome.Fractal"` and `image-path = "mail-unread"`.
    #[test]
    fn a_gnotification_style_sender_resolves_to_its_desktop_entry() {
        let app = SeenApp {
            app_name: "gnotif".into(),
            desktop_entry: "org.gnome.Fractal".into(),
            image_path: "mail-unread".into(),
            app_icon: String::new(),
        };
        assert_eq!(
            icon_candidates(&app),
            vec![
                IconSource::Named("org.gnome.Fractal".into()),
                IconSource::Named("org.gnome.Fractal-symbolic".into()),
                IconSource::Named("mail-unread".into()),
                IconSource::Named("mail-unread-symbolic".into()),
            ],
            "the app-id must be tried first, then the image the sender named -- \
             each in the two forms a theme may have it under"
        );
    }

    /// The whole order, and what falls out of it: `app_icon` is still used
    /// when it is the only thing there (Qt applications, `notify-send -n`),
    /// an empty field is not a candidate, and a sender that repeats itself
    /// across two fields does not produce the same candidate twice.
    #[test]
    fn icon_candidates_are_ordered_deduplicated_and_never_empty_strings() {
        let all = SeenApp {
            app_name: "a".into(),
            desktop_entry: "org.example.App".into(),
            image_path: "/tmp/x.png".into(),
            app_icon: "dialog-information".into(),
        };
        assert_eq!(
            icon_candidates(&all),
            vec![
                IconSource::Named("org.example.App".into()),
                IconSource::Named("org.example.App-symbolic".into()),
                IconSource::File("/tmp/x.png".into()),
                IconSource::Named("dialog-information".into()),
                IconSource::Named("dialog-information-symbolic".into()),
            ]
        );

        let only_app_icon = SeenApp {
            app_name: "a".into(),
            desktop_entry: String::new(),
            image_path: "   ".into(),
            app_icon: "keepassxc".into(),
        };
        assert_eq!(
            icon_candidates(&only_app_icon),
            vec![
                IconSource::Named("keepassxc".into()),
                IconSource::Named("keepassxc-symbolic".into()),
            ],
            "app_icon is still the last resort, and a blank field is not one"
        );

        let repeated = SeenApp {
            app_name: "a".into(),
            desktop_entry: "signal-desktop".into(),
            image_path: "signal-desktop".into(),
            app_icon: "signal-desktop".into(),
        };
        assert_eq!(
            icon_candidates(&repeated),
            vec![
                IconSource::Named("signal-desktop".into()),
                IconSource::Named("signal-desktop-symbolic".into()),
            ],
            "one name repeated across three fields is one name, not three"
        );

        let already_symbolic = SeenApp {
            app_name: "a".into(),
            desktop_entry: String::new(),
            image_path: String::new(),
            app_icon: "mail-unread-symbolic".into(),
        };
        assert_eq!(
            icon_candidates(&already_symbolic),
            vec![IconSource::Named("mail-unread-symbolic".into())],
            "a name that is already symbolic does not get a second suffix"
        );

        let nothing = SeenApp {
            app_name: "a".into(),
            desktop_entry: String::new(),
            image_path: String::new(),
            app_icon: String::new(),
        };
        assert!(icon_candidates(&nothing).is_empty());
    }

    /// CRITICAL 3: the size limits an application-supplied image has to be
    /// inside before the main thread decodes it. The 12000x12000 PNG that
    /// occasioned this is 435 KB on disk, so the byte limit alone would
    /// have waved it straight through.
    #[test]
    fn an_oversized_icon_file_is_out_of_limits() {
        assert!(icon_file_size_within_limit(40_000), "a real icon");
        assert!(icon_pixels_within_limit(512, 512), "a real icon");
        assert!(
            icon_file_size_within_limit(32_840) && !icon_pixels_within_limit(12_000, 12_000),
            "a decompression bomb is small on disk and enormous decoded, which \
             is why the byte limit cannot be the only one"
        );
        assert!(
            !icon_file_size_within_limit(MAX_ICON_FILE_BYTES + 1),
            "and an enormous file is out regardless of its dimensions"
        );
        assert!(icon_pixels_within_limit(MAX_ICON_PIXELS, MAX_ICON_PIXELS));
        assert!(!icon_pixels_within_limit(MAX_ICON_PIXELS + 1, 1));
        assert!(
            !icon_pixels_within_limit(0, 0),
            "an image with no size at all is not one to draw"
        );
    }

    /// CRITICAL 2's other half: nothing bounded what the window could write
    /// into `config.toml`. A suggestion row's name comes off the bus, where
    /// the sender chooses it and no length limit applies, so Add could put
    /// a megabyte of it in the config file to be parsed on every load.
    #[test]
    fn an_added_name_is_length_bounded() {
        let mut cfg = Config::default();
        allow_add(&mut cfg, &"a".repeat(1_000_000));
        assert_eq!(
            cfg.notifications.allow[0].chars().count(),
            MAX_APP_NAME_LEN,
            "an entry must be bounded where it is added"
        );
    }

    /// Every curated entry must be a plausible `app_name`, not a marketing
    /// name: the list is matched literally and a wrong guess silently never
    /// matches anything.
    #[test]
    fn the_curated_table_is_well_formed() {
        for (name, icon) in CURATED {
            assert!(!name.trim().is_empty());
            assert_eq!(*name, name.trim());
            // An empty icon column is allowed and means "no icon worth
            // guessing" (see CURATED's doc comment); a *blank* one is a
            // typo that would classify as `IconSource::None` by accident.
            assert_eq!(*icon, icon.trim());
        }
        // No duplicates, case-insensitively.
        let mut seen: Vec<String> = CURATED.iter().map(|(n, _)| n.to_lowercase()).collect();
        seen.sort();
        let before = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), before, "the curated table has duplicates");
    }

    // ---- The Test row -------------------------------------------------
    //
    // Every string and every number §6's Test table shows is decided above
    // and asserted here, because `window.rs` -- which maps a variant onto
    // two labels and a visibility -- is the one layer with no tests.

    /// A rewriter with one canned answer and, optionally, a sleep so a test
    /// can drive the deadline. No network, no runtime: the same reason
    /// `crate::reword`'s own `Stub` is a struct with a `Vec`.
    struct Canned(
        std::sync::Mutex<Option<Result<String, RewordError>>>,
        Duration,
    );

    impl Canned {
        fn new(outcome: Result<String, RewordError>) -> Arc<Canned> {
            Arc::new(Canned(std::sync::Mutex::new(Some(outcome)), Duration::ZERO))
        }
        fn after(delay: Duration, outcome: Result<String, RewordError>) -> Arc<Canned> {
            Arc::new(Canned(std::sync::Mutex::new(Some(outcome)), delay))
        }
    }

    impl Rewriter for Canned {
        fn reword(&self, _text: &str) -> Result<String, RewordError> {
            if !self.1.is_zero() {
                std::thread::sleep(self.1);
            }
            lock(&self.0)
                .take()
                .unwrap_or(Err(RewordError::Malformed("exhausted".into())))
        }
    }

    /// A model whose Test row answers from `rewriter` rather than from a
    /// provider.
    fn model_with(dir: &Path, rewriter: Arc<dyn Rewriter>) -> (SettingsModel, EngineHandle) {
        model_from(dir, move |_cfg: &RewordConfig| Ok(rewriter.clone()))
    }

    /// [`model_with`] with the whole factory handed in, for the tests that
    /// care about the config it is called with or want it to fail.
    fn model_from(
        dir: &Path,
        factory: impl Fn(&RewordConfig) -> Result<Arc<dyn Rewriter>, RewordError>
            + Send
            + Sync
            + 'static,
    ) -> (SettingsModel, EngineHandle) {
        let (store, engine) = store_in(dir);
        let model = SettingsModel::new_with_rewriter(
            store,
            dir.to_path_buf(),
            Config::default(),
            Arc::new(factory),
        );
        (model, engine)
    }

    /// Point a model's Test row at an endpoint no other test in this binary
    /// uses.
    ///
    /// `crate::reword::state()` is process-wide and its "endpoints announced
    /// this run" set is keyed `base_url|model`, so whether a request is the
    /// *first* against an endpoint is only a deterministic fact about a key
    /// nothing else touches. The `model` field rather than the URL because
    /// two rows below assert on the endpoint string itself.
    fn own_endpoint(m: &SettingsModel, name: &str) {
        m.edit(|c| c.reword.model = format!("settings-test-{name}"))
            .expect("a model name is not a validated field");
    }

    /// One outcome from the Test row.
    ///
    /// Retries `Busy`, which is not flakiness in the row but sharing in the
    /// suite: the two permits are process-wide, and `dbus` and
    /// `notify::monitor` each hold one for up to three seconds against a
    /// deliberately silent provider. `Busy` is a real row with its own
    /// wording (asserted separately, from the variant); what it is not is a
    /// thing any of these tests is about.
    fn outcome_of(model: &SettingsModel, text: &str) -> TestOutcome {
        let deadline = std::time::Instant::now() + SETTLE;
        loop {
            let rx = model.test_reword(text.to_string());
            let out = rx.recv_blocking().expect("the test thread answers");
            if !matches!(out, TestOutcome::Busy) || std::time::Instant::now() > deadline {
                return out;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// One case per row of §6's Test table. This is what keeps the wording
    /// out of `window.rs`, which has no tests of its own.
    #[test]
    fn every_test_outcome_reports_its_own_row() {
        // Success. The rewritten text is the title, because it is the point;
        // the latency is the subtitle, because the question it answers is
        // "is my deadline long enough".
        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_with(
            dir.path(),
            Canned::new(Ok("Alice is asking about dinner".into())),
        );
        own_endpoint(&model, "success");
        let out = outcome_of(&model, REWORD_TEST_DEFAULT);
        assert_eq!(out.title(), "Alice is asking about dinner");
        assert!(
            out.subtitle().starts_with("Rewritten in "),
            "subtitle was {:?}",
            out.subtitle()
        );
        assert!(
            out.subtitle().contains("first request"),
            "a first-ever test says so, because it includes connection setup: {:?}",
            out.subtitle()
        );
        drop(model);
        engine.shutdown();

        // The guard rejecting it: the model's answer is still shown, which
        // is how a user discovers their model likes to explain itself. Three
        // non-empty lines is two *extra* ones -- `Rejection::ExtraLines`
        // counts the lines beyond the first, because a rewrite gets exactly
        // one.
        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_with(
            dir.path(),
            Canned::new(Ok("Sure!\nHere you go:\nAlice is asking.".into())),
        );
        let out = outcome_of(&model, REWORD_TEST_DEFAULT);
        assert_eq!(out.title(), "Sure!\nHere you go:\nAlice is asking.");
        assert_eq!(
            out.subtitle(),
            "Rejected: 2 extra lines — spoken as written"
        );
        drop(model);
        engine.shutdown();

        // A rejected key names the environment variable, because a user who
        // exported one and then edits the password field is editing the
        // wrong thing.
        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_with(
            dir.path(),
            Canned::new(Err(RewordError::Auth {
                status: 401,
                host: "api.ppq.ai".into(),
                message: None,
            })),
        );
        let out = outcome_of(&model, REWORD_TEST_DEFAULT);
        assert_eq!(out.title(), "The provider rejected the API key");
        assert_eq!(
            out.subtitle(),
            "HTTP 401 from api.ppq.ai — check the key, or SAYD_REWORD_API_KEY if it is set"
        );
        drop(model);
        engine.shutdown();

        // The provider's own message, when it sent one, in brackets before
        // the advice.
        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_with(
            dir.path(),
            Canned::new(Err(RewordError::Auth {
                status: 403,
                host: "api.ppq.ai".into(),
                message: Some("insufficient credit".into()),
            })),
        );
        assert_eq!(
            outcome_of(&model, REWORD_TEST_DEFAULT).subtitle(),
            "HTTP 403 from api.ppq.ai (insufficient credit) — check the key, or \
             SAYD_REWORD_API_KEY if it is set"
        );
        drop(model);
        engine.shutdown();

        // Unreachable: the transport error, and the resolved endpoint.
        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_with(
            dir.path(),
            Canned::new(Err(RewordError::Unreachable(
                "io: Connection refused".into(),
            ))),
        );
        let out = outcome_of(&model, REWORD_TEST_DEFAULT);
        assert_eq!(out.title(), "Could not reach the provider");
        assert_eq!(
            out.subtitle(),
            "io: Connection refused — http://localhost:11434/v1"
        );
        drop(model);
        engine.shutdown();

        // No such model: the model string exactly as sent.
        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_with(
            dir.path(),
            Canned::new(Err(RewordError::NoSuchModel {
                status: 404,
                model: "llama3.2:3b".into(),
                message: None,
            })),
        );
        let out = outcome_of(&model, REWORD_TEST_DEFAULT);
        assert_eq!(out.title(), "The provider does not have that model");
        assert_eq!(out.subtitle(), "HTTP 404 — sent as ‘llama3.2:3b’");
        drop(model);
        engine.shutdown();

        // Rate limited, with and without Retry-After.
        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_with(
            dir.path(),
            Canned::new(Err(RewordError::RateLimited {
                retry_after: Some(Duration::from_secs(12)),
                message: None,
            })),
        );
        let out = outcome_of(&model, REWORD_TEST_DEFAULT);
        assert_eq!(out.title(), "The provider is rate limiting");
        assert_eq!(out.subtitle(), "HTTP 429 — Retry-After: 12 s");
        drop(model);
        engine.shutdown();

        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_with(
            dir.path(),
            Canned::new(Err(RewordError::RateLimited {
                retry_after: None,
                message: None,
            })),
        );
        assert_eq!(
            outcome_of(&model, REWORD_TEST_DEFAULT).subtitle(),
            "HTTP 429"
        );
        drop(model);
        engine.shutdown();

        // The client's own ceiling: reported as the ceiling that was
        // actually waited, next to the deadline that was not.
        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_with(dir.path(), Canned::new(Err(RewordError::Ceiling)));
        let out = outcome_of(&model, REWORD_TEST_DEFAULT);
        assert_eq!(out.title(), "No answer after 10.0 s");
        assert_eq!(
            out.subtitle(),
            "The deadline is 1.5 s, so this provider is not usable for notifications"
        );
        drop(model);
        engine.shutdown();

        // An unusable endpoint, which is otherwise a silent degradation.
        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_from(dir.path(), |_| {
            Err(RewordError::NotConfigured(
                "reword.base_url \"nonsense\" has no scheme; expected http:// or https://".into(),
            ))
        });
        let out = outcome_of(&model, REWORD_TEST_DEFAULT);
        assert_eq!(out.title(), "The endpoint is not usable");
        assert!(
            out.subtitle().contains("expected http:// or https://"),
            "the reason names the field and the fix: {:?}",
            out.subtitle()
        );
        drop(model);
        engine.shutdown();

        // A body that came back and could not be used. The subtitle is the
        // same shape as an unreachable provider's -- the detail and the
        // endpoint are what both rows have to show -- but the title must not
        // say the provider could not be reached, because it was: it answered,
        // and what it answered is the thing to look at. A user sent to check
        // whether their server is up is a user not looking at their URL.
        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_with(
            dir.path(),
            Canned::new(Err(RewordError::Malformed("no choices[0]".into()))),
        );
        let out = outcome_of(&model, REWORD_TEST_DEFAULT);
        assert_eq!(out.title(), "The endpoint answered something unusable");
        assert_eq!(out.subtitle(), "no choices[0] — http://localhost:11434/v1");
        drop(model);
        engine.shutdown();

        // A build with no client in it.
        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_from(dir.path(), |_| Err(RewordError::Unavailable));
        let out = outcome_of(&model, REWORD_TEST_DEFAULT);
        assert_eq!(out.title(), "This build has no rewording client");
        assert_eq!(out.subtitle(), "Rebuild with --features reword to use this");
        drop(model);
        engine.shutdown();

        // The one row no test can provoke without taking both process-wide
        // permits away from the rest of the suite, so it is asserted from
        // the variant. It still has to say something a user can act on.
        assert_eq!(
            TestOutcome::Busy.title(),
            "Another rewrite is still running"
        );
        assert_eq!(
            TestOutcome::Busy.subtitle(),
            "Both rewrite slots are in use; try again in a moment"
        );

        // The row's two non-outcome strings -- shown before any
        // `TestOutcome` exists, or when none ever arrived -- pinned here for
        // the same reason every other sentence in this test is: this module
        // is the one with tests, so nothing user-facing is authored where
        // there are none.
        assert_eq!(TEST_IN_PROGRESS_TITLE, "Testing…");
        assert_eq!(TEST_INCOMPLETE_TITLE, "The test did not complete");
    }

    /// The only arithmetic in the row, and the branch that makes the latency
    /// worth reporting at all.
    #[test]
    fn the_latency_is_formatted_and_measured_against_the_deadline() {
        assert_eq!(human_elapsed(Duration::from_millis(0)), "0 ms");
        assert_eq!(human_elapsed(Duration::from_millis(840)), "840 ms");
        assert_eq!(human_elapsed(Duration::from_millis(999)), "999 ms");
        assert_eq!(
            human_elapsed(Duration::from_millis(1000)),
            "1.0 s",
            "the crossover is at exactly one second"
        );
        assert_eq!(human_elapsed(Duration::from_millis(2449)), "2.4 s");
        assert_eq!(human_elapsed(Duration::from_millis(10_000)), "10.0 s");

        assert_eq!(human_deadline(Duration::from_millis(1500)), "1.5 s");
        assert_eq!(human_deadline(Duration::from_millis(200)), "0.2 s");
        assert_eq!(human_deadline(Duration::from_millis(2000)), "2.0 s");

        // Inside the deadline: the number, and nothing to worry about.
        let inside = TestOutcome::Rewritten {
            text: "Alice is asking about dinner".into(),
            elapsed: Duration::from_millis(840),
            deadline: Duration::from_millis(1500),
            first: false,
        };
        assert_eq!(
            inside.subtitle(),
            "Rewritten in 840 ms, inside the 1.5 s deadline"
        );

        // Outside it: the number, the deadline, and what that means for a
        // real notification. This sentence is the whole reason the row
        // reports a latency.
        let outside = TestOutcome::Slower {
            text: "Alice is asking about dinner".into(),
            elapsed: Duration::from_millis(2400),
            deadline: Duration::from_millis(1500),
            first: false,
        };
        assert_eq!(
            outside.subtitle(),
            "Rewritten in 2.4 s — longer than the 1.5 s deadline, so a real \
             notification would have been spoken as written"
        );
        assert_eq!(outside.title(), "Alice is asking about dinner");

        // ...and a *first* request that misses the deadline says why the
        // number may not happen again, which is the difference between "your
        // provider is too slow" and "press Test once more".
        let first_time = TestOutcome::Slower {
            text: "Alice is asking about dinner".into(),
            elapsed: Duration::from_millis(2400),
            deadline: Duration::from_millis(1500),
            first: true,
        };
        assert_eq!(
            first_time.subtitle(),
            "Rewritten in 2.4 s — longer than the 1.5 s deadline, so a real \
             notification would have been spoken as written (first request — \
             includes connection setup)"
        );
    }

    /// A rejected answer is the one string in this window a provider writes
    /// straight into a row title, and it arrives with nothing bounding it.
    ///
    /// The guard's `Oversized` check rejects a candidate past four bytes per
    /// character of its ceiling -- about 372 bytes for the default test text
    /// -- and the HTTP client stops reading at 64 KiB, so every answer
    /// between those two bounds reaches `title()` verbatim. A local model
    /// with a runaway generation against a server that ignores `max_tokens`
    /// is the ordinary way to produce one, and that user is precisely who
    /// this row is for. It also arrives *before* `check` trims it, so a model
    /// that opens with two blank lines would push its own text out of the
    /// top of the row.
    #[test]
    fn a_rejected_answer_is_trimmed_and_bounded_before_it_becomes_a_title() {
        // Trimmed. The variant keeps the candidate as it came off the wire;
        // `title()` is what makes it fit in a row.
        let padded = TestOutcome::Rejected {
            answer: "\n\nSure!\nHere:\n".into(),
            reason: sayd_core::reword::Rejection::ExtraLines(1),
        };
        assert_eq!(
            padded.title(),
            "Sure!\nHere:",
            "leading blank lines would push the text the row is about out of sight"
        );

        // Bounded, with an ellipsis so the cut is visible rather than a
        // sentence that appears to end mid-word.
        let runaway = "x".repeat(1000);
        let long = TestOutcome::Rejected {
            answer: runaway.clone(),
            reason: sayd_core::reword::Rejection::TooLong {
                chars: 1000,
                limit: 93,
            },
        };
        let title = long.title();
        assert_eq!(title.chars().count(), ANSWER_DISPLAY_MAX_CHARS + 1);
        assert!(title.ends_with('…'));
        assert!(title.starts_with(&runaway[..ANSWER_DISPLAY_MAX_CHARS]));

        // Exactly at the bound is not a truncation, so it does not claim to
        // be one.
        let exact = "y".repeat(ANSWER_DISPLAY_MAX_CHARS);
        assert_eq!(
            TestOutcome::Rejected {
                answer: exact.clone(),
                reason: sayd_core::reword::Rejection::CodeFence,
            }
            .title(),
            exact
        );

        // Multi-byte, because the cut is by character and the string is
        // whatever a provider chose to send: an index computed in bytes
        // would panic here rather than truncate.
        let wide = "é".repeat(400);
        let title = TestOutcome::Rejected {
            answer: wide,
            reason: sayd_core::reword::Rejection::CodeFence,
        }
        .title();
        assert_eq!(title.chars().count(), ANSWER_DISPLAY_MAX_CHARS + 1);

        // And end to end, so the bound is on what the row actually shows
        // rather than on a variant a test built by hand. 300 characters is
        // over the guard's 93-character ceiling for this text and under the
        // byte bound at which it stops counting, which is the window this
        // cap exists for.
        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_with(
            dir.path(),
            Canned::new(Ok(format!("  {}", "z".repeat(300)))),
        );
        let out = outcome_of(&model, REWORD_TEST_DEFAULT);
        assert!(matches!(out, TestOutcome::Rejected { .. }), "{out:?}");
        assert_eq!(out.title().chars().count(), ANSWER_DISPLAY_MAX_CHARS + 1);
        assert!(
            out.title().starts_with('z'),
            "the leading spaces are trimmed: {:?}",
            out.title()
        );
        drop(model);
        engine.shutdown();
    }

    /// `speech()` is Speak's whole contract: `window.rs` hides the button on
    /// `None` and otherwise plays back exactly this string, so every rule
    /// about what is worth hearing has to live here, provably, or it is not
    /// tested at all.
    #[test]
    fn speech_is_the_providers_own_text_and_nothing_else() {
        let rewritten = TestOutcome::Rewritten {
            text: "Alice is asking about dinner".into(),
            elapsed: Duration::from_millis(1),
            deadline: Duration::from_millis(1),
            first: false,
        };
        assert_eq!(
            rewritten.speech().as_deref(),
            Some("Alice is asking about dinner")
        );

        let slower = TestOutcome::Slower {
            text: "Alice is asking about dinner".into(),
            elapsed: Duration::from_millis(1),
            deadline: Duration::from_millis(1),
            first: false,
        };
        assert_eq!(
            slower.speech().as_deref(),
            Some("Alice is asking about dinner")
        );

        // Untrimmed by the title's 200-character cap: hearing the model's
        // answer in full is this row's whole point.
        let long_answer = format!("  {}", "z".repeat(300));
        let rejected = TestOutcome::Rejected {
            answer: long_answer.clone(),
            reason: sayd_core::reword::Rejection::TooLong {
                chars: 300,
                limit: 93,
            },
        };
        assert_eq!(rejected.speech(), Some("z".repeat(300)));
        assert_eq!(
            rejected.speech().unwrap().chars().count(),
            300,
            "not cut to ANSWER_DISPLAY_MAX_CHARS the way the title is"
        );

        // Every status row has a sentence about the button or the transport
        // and nothing a provider wrote, so there is nothing for Speak to say.
        let nothing_to_speak: Vec<TestOutcome> = vec![
            TestOutcome::AuthRejected {
                status: 401,
                host: "h".into(),
                env_var: "E".into(),
                message: None,
            },
            TestOutcome::Unreachable {
                detail: "d".into(),
                endpoint: "e".into(),
            },
            TestOutcome::Unusable {
                detail: "d".into(),
                endpoint: "e".into(),
            },
            TestOutcome::NoSuchModel {
                status: 404,
                model: "m".into(),
                message: None,
            },
            TestOutcome::RateLimited { retry_after: None },
            TestOutcome::NoAnswer {
                ceiling: Duration::from_secs(1),
                deadline: Duration::from_secs(1),
            },
            TestOutcome::NotConfigured { reason: "r".into() },
            TestOutcome::Unavailable,
            TestOutcome::Busy,
        ];
        for status in nothing_to_speak {
            assert!(status.speech().is_none(), "{status:?}");
        }
    }

    /// The first request against an endpoint pays for DNS and a handshake,
    /// and the second does not. Both halves, because saying "first request"
    /// on every test would be exactly as useless as saying it on none.
    #[test]
    fn only_the_first_request_against_an_endpoint_is_called_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_from(dir.path(), |_| {
            Ok(Canned::new(Ok("Alice is asking about dinner".into())) as Arc<dyn Rewriter>)
        });
        own_endpoint(&model, "warmup");

        let first = outcome_of(&model, REWORD_TEST_DEFAULT);
        assert!(
            first.subtitle().contains("first request"),
            "{:?}",
            first.subtitle()
        );
        let second = outcome_of(&model, REWORD_TEST_DEFAULT);
        assert!(
            second.subtitle().starts_with("Rewritten in ")
                && !second.subtitle().contains("first request"),
            "a warm endpoint's latency is reported without the caveat: {:?}",
            second.subtitle()
        );
        drop(model);
        engine.shutdown();
    }

    /// A slow answer that still arrives is `Slower`, not `Rewritten`: Test
    /// waits the full ceiling on purpose, so it is the only place a user can
    /// find out *how much* too slow their provider is.
    #[test]
    fn an_answer_past_the_deadline_is_still_reported_with_its_real_latency() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_with(
            dir.path(),
            Canned::after(
                Duration::from_millis(300),
                Ok("Alice is asking about dinner".into()),
            ),
        );
        model
            .edit(|c| c.reword.timeout_ms = 200)
            .expect("a deadline shorter than the stub's delay");
        let out = outcome_of(&model, REWORD_TEST_DEFAULT);
        assert!(
            matches!(out, TestOutcome::Slower { .. }),
            "a 300 ms answer against a 200 ms deadline is Slower, not Rewritten: {out:?}"
        );
        assert!(
            out.subtitle().contains("longer than the 0.2 s deadline"),
            "{:?}",
            out.subtitle()
        );
        assert_eq!(
            out.title(),
            "Alice is asking about dinner",
            "the rewrite is still shown: it is what the provider can do, just \
             not in time"
        );
        drop(model);
        engine.shutdown();
    }

    /// §6: Test uses the config the window is *displaying*, not the last
    /// value written to disk. Otherwise a user who types a key and
    /// immediately presses Test is told their old key is rejected.
    #[test]
    fn test_reword_uses_the_pending_config_not_the_written_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seen = Arc::new(Mutex::new(String::new()));
        let s = seen.clone();
        let (model, engine) = model_from(dir.path(), move |cfg: &RewordConfig| {
            *lock(&s) = cfg.api_key.clone();
            Ok(Canned::new(Ok("fine".into())) as Arc<dyn Rewriter>)
        });

        // Edited but, thanks to WRITE_DEBOUNCE, certainly not on disk yet.
        model
            .edit(|c| c.reword.api_key = "sk-just-typed".into())
            .expect("edit");
        let _ = outcome_of(&model, REWORD_TEST_DEFAULT);
        assert_eq!(
            lock(&seen).clone(),
            "sk-just-typed",
            "the key the user is looking at is the key that gets tested"
        );
        drop(model);
        engine.shutdown();
    }

    /// §4's eligibility floor is bypassed: the user typed this text
    /// deliberately, and refusing to test `Ping` because it is under twelve
    /// characters would be baffling.
    #[test]
    fn test_reword_sends_text_the_notification_path_would_refuse() {
        assert!(
            sayd_core::reword::eligible("Ping", RewordConfig::default().max_chars).is_err(),
            "the premise: this text is ineligible on the automatic path"
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let sent = Arc::new(Mutex::new(false));
        let s = sent.clone();

        struct Watching(Arc<Mutex<bool>>);
        impl Rewriter for Watching {
            fn reword(&self, _text: &str) -> Result<String, RewordError> {
                *lock(&self.0) = true;
                Ok("Ping".into())
            }
        }

        let (model, engine) = model_from(dir.path(), move |_| {
            Ok(Arc::new(Watching(s.clone())) as Arc<dyn Rewriter>)
        });
        let out = outcome_of(&model, "Ping");
        assert!(*lock(&sent), "the request must actually have been made");
        assert!(
            matches!(out, TestOutcome::Rewritten { .. }),
            "an answer identical to the original is the prompt's \"reply with \
             it unchanged\" path working, not a rejection: {out:?}"
        );
        drop(model);
        engine.shutdown();
    }

    /// A successful test clears the auth latch, which is the only way back
    /// when the key came from the environment -- editing that does not change
    /// the config, so nothing else would ever clear it. And a test never
    /// *sets* a breaker: a user who has just fixed a key must get a real
    /// request, not a cached verdict.
    ///
    /// Written as the transitions it drives rather than as absolute states:
    /// `crate::reword::state()` is process-wide and this suite runs in
    /// parallel, so what this can assert is what it changed, never what it
    /// started from.
    #[test]
    fn a_successful_test_clears_the_auth_latch_and_a_failing_one_sets_nothing() {
        let state = crate::reword::state();
        let now = std::time::Instant::now();

        // A failing test leaves the latch exactly as it found it and adds no
        // breaker of its own.
        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_with(
            dir.path(),
            Canned::new(Err(RewordError::Unreachable("refused".into()))),
        );
        own_endpoint(&model, "latch-failing");
        let cfg = *model.current().reword;
        state.record(
            &cfg,
            &crate::reword::Attempt::Answered(Err(RewordError::Auth {
                status: 401,
                host: "h".into(),
                message: None,
            })),
            now,
        );
        assert_eq!(
            state.allow(&cfg, now),
            Err(crate::reword::Blocked::AuthLatched),
            "the premise: this config is latched"
        );
        let out = outcome_of(&model, REWORD_TEST_DEFAULT);
        assert!(matches!(out, TestOutcome::Unreachable { .. }), "{out:?}");
        assert_eq!(
            state.allow(&cfg, now),
            Err(crate::reword::Blocked::AuthLatched),
            "a failing test must not record anything: it is a deliberate probe, \
             not traffic -- and it must not clear what it did not fix"
        );
        drop(model);
        engine.shutdown();

        // ...and a working key un-latches. Asserted as "no longer latched"
        // rather than as `Ok(())`: an unrelated test opening the transport
        // breaker on the shared state would otherwise fail this for a reason
        // that has nothing to do with the rule.
        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_with(dir.path(), Canned::new(Ok("fine".into())));
        own_endpoint(&model, "latch-working");
        let cfg = *model.current().reword;
        state.record(
            &cfg,
            &crate::reword::Attempt::Answered(Err(RewordError::Auth {
                status: 401,
                host: "h".into(),
                message: None,
            })),
            now,
        );
        assert_eq!(
            state.allow(&cfg, now),
            Err(crate::reword::Blocked::AuthLatched)
        );
        let _ = outcome_of(&model, REWORD_TEST_DEFAULT);
        assert_ne!(
            state.allow(&cfg, now),
            Err(crate::reword::Blocked::AuthLatched),
            "a working key un-latches; nothing else ever would when the key \
             came from the environment"
        );
        drop(model);
        engine.shutdown();

        // And the half that costs the most if it is wrong: a 401 *during a
        // test* must not latch the breaker for the daemon. The user is
        // holding the key field open and is about to fix it; a latch set
        // here would switch rewording off for the run and, since the config
        // they are about to type is a different one, would then sit there
        // blocking nothing and teaching them nothing.
        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_with(
            dir.path(),
            Canned::new(Err(RewordError::Auth {
                status: 401,
                host: "api.ppq.ai".into(),
                message: None,
            })),
        );
        own_endpoint(&model, "latch-never-set");
        let cfg = *model.current().reword;
        let out = outcome_of(&model, REWORD_TEST_DEFAULT);
        assert!(matches!(out, TestOutcome::AuthRejected { .. }), "{out:?}");
        assert_ne!(
            state.allow(&cfg, now),
            Err(crate::reword::Blocked::AuthLatched),
            "a rejected key learned from a deliberate probe is reported, not \
             recorded: the next request must be a real one"
        );
        drop(model);
        engine.shutdown();
    }

    /// The group description names the destination host and says where the
    /// key is coming from -- a user who exports SAYD_REWORD_API_KEY and then
    /// sees an empty password field would otherwise conclude the feature is
    /// unconfigured.
    #[test]
    fn the_group_description_names_the_destination_and_the_key_source() {
        let mut cfg = RewordConfig::default();
        assert_eq!(
            reword_description(&cfg, None),
            "Sends the text about to be spoken to localhost. \
             No API key is set; local servers ignore it. \
             Pressing Test below is itself a network call."
        );

        assert_eq!(
            reword_description(&cfg, Some("sk-from-env")),
            "Sends the text about to be spoken to localhost. \
             The API key comes from SAYD_REWORD_API_KEY, not from the field below. \
             Pressing Test below is itself a network call."
        );

        // A variable that is set but empty is not a key, which is exactly
        // what `resolve_api_key_with` decides for the request itself.
        assert_eq!(
            reword_description(&cfg, Some("")),
            reword_description(&cfg, None)
        );

        cfg.api_key = "sk-in-file".into();
        cfg.base_url = "https://api.ppq.ai/v1".into();
        assert_eq!(
            reword_description(&cfg, None),
            "Sends the text about to be spoken to api.ppq.ai. \
             The API key comes from this config file. \
             Pressing Test below is itself a network call."
        );

        cfg.base_url = "nonsense".into();
        assert!(
            reword_description(&cfg, None).contains("not a usable endpoint"),
            "an unparseable base_url must say so here, since §8 makes it a \
             silent degradation everywhere else"
        );
    }

    /// The description a window actually shows, against the real
    /// environment, is the same function -- so the plumbing cannot be the
    /// part that is wrong.
    #[test]
    fn the_group_description_shown_is_the_one_that_was_tested() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (model, engine) = model_with(dir.path(), Canned::new(Ok("fine".into())));
        // No environment is touched: `api_key_env` names no variable, which
        // `resolve_api_key_with` treats as "never looked up".
        model
            .edit(|c| c.reword.api_key_env = String::new())
            .expect("edit");
        assert_eq!(
            model.reword_description_now(),
            reword_description(&model.current().reword, None)
        );
        drop(model);
        engine.shutdown();
    }

    /// The API key row is offered for the endpoints that can use one, and
    /// withheld from the ones that cannot -- a credential field in front of
    /// a server that takes no credential invites putting a secret into a
    /// file the settings window rewrites wholesale, for nothing.
    ///
    /// The two exceptions are the ones that make hiding it safe: a key
    /// already in the file keeps the row (it is the only way to read or
    /// clear it), and an endpoint that does not parse is not a claim about
    /// anything, so the row stays while the user repairs it.
    ///
    /// A third case sits alongside those two rather than among the hidden
    /// ones: vLLM's loopback preset. §6's table marks its Key column "as
    /// configured", not "ignored" like the other three local presets, so
    /// the loopback test alone would wrongly hide the only place to enter a
    /// key `vllm serve --api-key …` is waiting for.
    #[test]
    fn the_api_key_row_is_offered_only_where_a_key_can_be_used() {
        let mut cfg = RewordConfig::default();
        assert!(
            !reword_key_row_applies(&cfg),
            "the default endpoint is this machine, which takes no key"
        );

        for local in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:8080/v1",
            "http://[::1]:1234/v1",
            // Same host as the vLLM preset, different port: not a match for
            // the exemption, so this is still "ignored" by the loopback
            // rule -- unlike the exact preset string tested below.
            "http://LOCALHOST:9999/v1",
        ] {
            cfg.base_url = local.into();
            assert!(!reword_key_row_applies(&cfg), "{local} is this machine");
        }

        for remote in ["https://api.ppq.ai/v1", "https://api.openai.com/v1"] {
            cfg.base_url = remote.into();
            assert!(reword_key_row_applies(&cfg), "{remote} may want a key");
        }

        // vLLM is a loopback endpoint whose Key column is not "ignored": the
        // preset exemption keeps the row offered even though it never
        // leaves this machine.
        cfg.base_url = "http://localhost:8000/v1".into();
        assert!(
            reword_key_row_applies(&cfg),
            "vLLM's preset may be running with --api-key"
        );

        // Whatever the endpoint, a key the file already holds keeps its row:
        // hiding a secret that is still on disk would be worse than showing
        // a field nobody needs.
        cfg.base_url = "http://localhost:11434/v1".into();
        cfg.api_key = "sk-left-behind".into();
        assert!(reword_key_row_applies(&cfg));

        // And an unparseable endpoint errs towards visible.
        cfg.api_key = String::new();
        cfg.base_url = "nonsense".into();
        assert!(reword_key_row_applies(&cfg));

        // The environment is not consulted: `api_key_env` supplies the
        // request's key without this field, and the group description is
        // where that is said.
        cfg.base_url = "http://localhost:11434/v1".into();
        cfg.api_key_env = "SAYD_REWORD_API_KEY".into();
        assert!(!reword_key_row_applies(&cfg));
        assert!(
            reword_description(&cfg, Some("sk-from-env")).contains("SAYD_REWORD_API_KEY"),
            "with the row hidden, the description is the only place the key's \
             source is named"
        );
    }

    /// The presets are the six rows of §6's endpoint table. They live in
    /// this module rather than in `window.rs` for the reason every other
    /// table does: the window is the layer with no tests.
    #[test]
    fn the_endpoint_presets_are_the_documented_table() {
        assert_eq!(ENDPOINT_PRESETS.len(), 6);
        assert_eq!(
            ENDPOINT_PRESETS[0],
            ("Ollama", "http://localhost:11434/v1", false)
        );
        for (name, url, _takes_key) in ENDPOINT_PRESETS {
            assert!(
                sayd_core::reword::parse_base_url(url).is_ok(),
                "{name}'s preset must be a usable endpoint"
            );
        }
        assert!(
            ENDPOINT_PRESETS
                .iter()
                .any(|(_, url, _)| *url == RewordConfig::default().base_url),
            "the default endpoint must be offered as a preset, so a user who \
             wandered away from it can get back"
        );

        // The Key column, collapsed to a bool: "ignored" for the three
        // local servers whose loopback address is enough on its own, "as
        // configured" or `sk-…` for the three that can (or must) carry one.
        for name in ["Ollama", "llama.cpp server", "LM Studio"] {
            let &(_, _, takes_key) = ENDPOINT_PRESETS
                .iter()
                .find(|(n, _, _)| *n == name)
                .expect("preset present");
            assert!(!takes_key, "{name}'s Key column is \"ignored\"");
        }
        for name in ["vLLM", "PPQ", "OpenAI"] {
            let &(_, _, takes_key) = ENDPOINT_PRESETS
                .iter()
                .find(|(n, _, _)| *n == name)
                .expect("preset present");
            assert!(takes_key, "{name}'s Key column is not \"ignored\"");
        }
    }

    /// The cooldown floor was enforced on load and nowhere else, so the
    /// window could write a `cooldown_secs = 2` that silently became 3 the
    /// next time the file was read -- a file that disagrees with the running
    /// config.
    #[test]
    fn a_cooldown_shorter_than_a_rewrite_is_raised_before_it_is_written() {
        use sayd_core::config::NOTIFY_COOLDOWN_MIN_SECS;

        let mut cfg = Config::default();
        cfg.notifications.cooldown_secs = 2;
        let warnings = clamp_ranges(&mut cfg);
        assert_eq!(cfg.notifications.cooldown_secs, NOTIFY_COOLDOWN_MIN_SECS);
        assert_eq!(warnings.len(), 1, "{warnings:?}");

        // `0` means rate limiting is off, not "no wait": no coalescing window
        // ever opens, so the ordering the floor protects does not exist.
        let mut off = Config::default();
        off.notifications.cooldown_secs = 0;
        assert!(clamp_ranges(&mut off).is_empty());
        assert_eq!(off.notifications.cooldown_secs, 0);

        // And the two doors agree, which is the whole point of mirroring it:
        // what the window writes is what a load of that file produces.
        let mut written = Config::default();
        written.notifications.cooldown_secs = 2;
        validate(&mut written).expect("a short cooldown is clamped, not refused");
        let (loaded, err) = sayd_core::config::Config::load_str(&format!(
            "[notifications]\ncooldown_secs = {}\n",
            written.notifications.cooldown_secs
        ));
        assert!(err.is_none(), "{err:?}");
        assert_eq!(
            loaded.notifications.cooldown_secs,
            written.notifications.cooldown_secs
        );
    }
}
