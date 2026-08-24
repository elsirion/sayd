//! The `sh.sayd.Sayd1` D-Bus interface.
//!
//! A thin mapping from D-Bus onto engine commands. It holds an
//! `EngineHandle` and never blocks the engine: reads come from the published
//! snapshot, and selection reads -- which open their own Wayland connection
//! and block -- run on a blocking thread so they cannot stall the runtime.

use std::collections::HashMap;
use std::sync::Arc;

use sayd_core::config::Config;
use sayd_core::engine::{Command, SayOpts, State, Submitted};
use sayd_core::handle::EngineHandle;
use sayd_core::queue::{Policy, Source as QueueSource};
use zbus::fdo;
use zbus::interface;
use zbus::zvariant::{OwnedValue, Str};

use crate::config_watch::{persist_in_background, ConfigStore};
use crate::pipeline::{self, Ask};
use crate::reword::{Spoken, Written};
use crate::selection;

pub struct SaydIface {
    pub engine: EngineHandle,
    /// Where `SetMuted` goes, so a mute survives the next config apply and
    /// the next restart -- see `ConfigStore::update` and spec §6.
    pub store: Arc<ConfigStore>,
}

/// The lowercase names the `State` property uses on the bus.
fn state_name(s: State) -> &'static str {
    match s {
        State::Idle => "idle",
        State::Speaking => "speaking",
        State::Paused => "paused",
        State::Error => "error",
    }
}

/// Build `SayOpts` from a D-Bus `a{sv}`.
///
/// Unknown keys and unparseable values are ignored rather than rejected: a
/// caller asking for something we do not understand should still be heard,
/// with defaults, rather than get an error for a cosmetic option.
fn say_opts_from(opts: &HashMap<String, OwnedValue>, source: QueueSource) -> SayOpts {
    let policy = opts.get("policy").and_then(|v| {
        let s = v.downcast_ref::<Str>().ok()?;
        match s.as_str() {
            "enqueue" => Some(Policy::Enqueue),
            "interrupt" => Some(Policy::Interrupt),
            "replace" => Some(Policy::Replace),
            "front" => Some(Policy::Front),
            _ => None,
        }
    });
    let voice = opts
        .get("voice")
        .and_then(|v| v.downcast_ref::<Str>().ok().map(|s| s.as_str().to_string()));
    let speed = opts
        .get("speed")
        .and_then(|v| v.downcast_ref::<f64>().ok())
        .map(|d| d as f32);

    SayOpts {
        policy,
        voice,
        speed,
        source,
    }
}

/// Did the caller ask for this submission to be rewritten?
///
/// The same ignore-don't-reject rule as [`say_opts_from`]: an absent key, a
/// wrongly-typed value and an explicit `false` all mean no. That rule is what
/// lets a new `say` talk to an old daemon (which ignores the key) and an old
/// `say` talk to a new one (which never sees it) without either erroring.
///
/// Deliberately *not* part of `SayOpts`. The rewrite is a pre-submission
/// transform -- it happens before `Engine::submit` is called at all -- and
/// `sayd-core` has no business knowing about it; a field on the struct the
/// engine consumes that the engine never reads would suggest otherwise.
fn wants_reword(opts: &HashMap<String, OwnedValue>) -> bool {
    opts.get("reword")
        .and_then(|v| v.downcast_ref::<bool>().ok())
        .unwrap_or(false)
}

impl SaydIface {
    pub fn new(engine: EngineHandle, store: Arc<ConfigStore>) -> Self {
        SaydIface { engine, store }
    }

    /// Which [`Ask`] this submission carries.
    ///
    /// The config is reached only through the ask that needs it, so an
    /// ordinary `Say` still pays nothing at all for a feature it did not
    /// ask for -- not even the mutex `ConfigStore::published` takes. That
    /// promise is pinned by `published_reads` in the tests below, and
    /// `Ask`'s own shape is what keeps it from being broken by accident.
    fn ask<'a>(
        cfg: &'a mut Option<Arc<Config>>,
        store: &ConfigStore,
        opts: &HashMap<String, OwnedValue>,
    ) -> Ask<'a> {
        if !wants_reword(opts) {
            return Ask::Never;
        }
        Ask::Requested(cfg.insert(store.published()))
    }

    /// Shared body of `SaySelection` and `SayClipboard`: read, rewrite if the
    /// caller asked, submit.
    ///
    /// IMPORTANT 4. The two methods used to spell all three steps out
    /// themselves, and the middle one was pinned by nothing: deleting
    /// `maybe_reword` from *both* of them passed 263 of 263, while the same
    /// deletion in `Say` correctly failed a test. The missing compositor is
    /// only half the reason -- the daemon-side line was simply untouched by
    /// any test. One body means one line to delete and one test to fail; the
    /// tests below drive `say_selection` and `say_clipboard` themselves,
    /// through `selection::read`'s own seam, so both names stay pinned rather
    /// than only the shared function they happen to call today.
    ///
    /// `read` is run on the blocking pool because it opens its own Wayland
    /// connection and blocks -- see `selection`'s module doc.
    async fn say_read(
        &self,
        read: impl FnOnce() -> Result<String, String> + Send + 'static,
        what: &str,
        opts: &HashMap<String, OwnedValue>,
    ) -> fdo::Result<u32> {
        let text = tokio::task::spawn_blocking(read)
            .await
            .map_err(|e| fdo::Error::Failed(format!("{what} read panicked: {e}")))?
            .map_err(fdo::Error::Failed)?;
        let mut held = None;
        let spoken = pipeline::prepare(Written(text), Self::ask(&mut held, &self.store, opts))
            .map_err(|too_long| fdo::Error::Failed(too_long.message()))?
            .resolve()
            .await;
        self.submit_spoken(spoken, say_opts_from(opts, QueueSource::Hotkey))
            .await
    }

    /// Submit what [`SaydIface::maybe_reword`] produced, falling back to the
    /// text a rewrite replaced if the engine refuses the rewrite.
    ///
    /// CRITICAL 2, and the reason it is a method rather than two lines at
    /// each call site: `Say`, `SaySelection` and `SayClipboard` all reach the
    /// engine through here, and a caller that submitted `spoken.text` and
    /// dropped `spoken.fallback` would compile and would silently reinstate
    /// the loss. [`Spoken::fallback`] is `None` unless something actually
    /// rewrote the text, so this retries at most once and never submits the
    /// same string twice.
    ///
    /// The retried submission is the one whose id the caller gets, which is
    /// right: it is the utterance that was queued.
    async fn submit_spoken(&self, spoken: Spoken, opts: SayOpts) -> fdo::Result<u32> {
        let Spoken { text, fallback } = spoken;
        let Some(original) = fallback else {
            return self.submit(text, opts).await;
        };
        match self.submit(text, opts.clone()).await {
            Err(e) => {
                eprintln!(
                    "warning: reword: the engine refused the rewritten text ({e}); \
                     speaking it as written instead"
                );
                self.submit(original, opts).await
            }
            ok => ok,
        }
    }

    /// Shared body of `Say`, `SaySelection` and `SayClipboard`.
    ///
    /// Returns the queued id; 0 when the submission was accepted but
    /// nothing was queued (muted, or empty after cleanup); or `u32::MAX`
    /// when `EngineHandle::submit`'s own bounded wait timed out before the
    /// engine could confirm what happened -- see the `match` below for why
    /// that is not also reported as 0.
    ///
    /// C2: `EngineHandle::submit` blocks the calling thread on a channel
    /// receive (bounded, but still potentially the full bound) waiting for
    /// the engine's answer. Calling it directly from this `async fn` would
    /// block whichever tokio worker thread happened to run it for that
    /// whole wait; with only a handful of worker threads and every `Say`/
    /// `SaySelection`/`SayClipboard` call doing this, a few concurrent
    /// submissions were enough to exhaust the pool and stall *every* other
    /// D-Bus method -- including fire-and-forget ones like `Stop`, whose own
    /// handler never blocks on anything but still could not get scheduled.
    /// `spawn_blocking` runs it on tokio's separate, much larger blocking
    /// thread pool instead, so a slow submission no longer starves the
    /// async runtime.
    async fn submit(&self, text: String, opts: SayOpts) -> fdo::Result<u32> {
        let engine = self.engine.clone();
        let result = tokio::task::spawn_blocking(move || engine.submit(text, opts))
            .await
            .map_err(|e| fdo::Error::Failed(format!("submit task panicked: {e}")))?;
        match result {
            // The queue's ids are `u64` and monotonically increasing, but the
            // wire type is `u32` (chosen to match `CurrentId` and `Cancel`,
            // and because a client has no use for ids beyond that range).
            // `Queue::next_id` starts at 1 and increments by 1 per accepted
            // utterance, so wrapping past `u32::MAX` needs over four billion
            // accepted utterances in one process lifetime -- not reachable
            // in practice, but truncating silently would hand a client the
            // wrong id if it ever were. Fall back to 0 ("nothing queued")
            // rather than lie about which utterance this is.
            Ok(Submitted::Queued(id)) => Ok(u32::try_from(id).unwrap_or(0)),
            Ok(Submitted::Discarded) => Ok(0),
            // Finding 3: reporting a timeout as 0 conflated it with
            // `Discarded` -- "nothing was queued" versus "something *was*
            // queued, but the engine thread was too busy to confirm it in
            // time." A caller cannot `Cancel` either way (no id came back),
            // but the two are not the same fact, and the daemon can tell
            // them apart internally (`Submitted::TimedOut` vs
            // `Submitted::Discarded`), so silently erasing the difference
            // on the wire would undo exactly the distinction Finding 3
            // exists to preserve.
            //
            // `u32::MAX` is free to use as that sentinel the same way 0 is:
            // `Queue::next_id` starts at 1 and climbs by one per accepted
            // utterance, so colliding with a real id needs the same
            // not-reachable-in-practice four billion utterances as the
            // overflow fallback above.
            //
            // A `fdo::Error` was considered and rejected. The typical
            // caller here is `sayd-cli`'s agent-narration loop, which fires
            // and forgets (see its own doc comment) -- and unlike a genuine
            // rejection, nothing is wrong with this submission: it *was*
            // handled, just not confirmed in time. Turning that into a
            // bus-level error would make a successfully-handled utterance
            // look exactly like a failure to a caller that already ignores
            // the return value, while costing the rarer caller that does
            // check it nothing it does not already pay for the `Discarded`
            // case: both are sentinels it is free to ignore, not errors it
            // is forced to handle.
            Ok(Submitted::TimedOut) => Ok(u32::MAX),
            Err(e) => Err(fdo::Error::Failed(e)),
        }
    }
}

#[interface(name = "sh.sayd.Sayd1")]
impl SaydIface {
    /// Speak `text`. Returns the utterance id; 0 if nothing was queued
    /// (muted, or empty after cleanup) -- safe, since the queue never mints
    /// id 0 and `CurrentId` uses 0 for "nothing playing"; or `u32::MAX` if
    /// the daemon accepted it but could not confirm an id in time (see
    /// `submit`'s doc comment) -- also safe, since the queue does not reach
    /// that id in practice either.
    async fn say(&self, text: String, opts: HashMap<String, OwnedValue>) -> fdo::Result<u32> {
        let mut held = None;
        let spoken = pipeline::prepare(Written(text), Self::ask(&mut held, &self.store, &opts))
            .map_err(|too_long| fdo::Error::Failed(too_long.message()))?
            .resolve()
            .await;
        self.submit_spoken(spoken, say_opts_from(&opts, QueueSource::DBus))
            .await
    }

    /// Speak the PRIMARY selection. Returns the utterance id; 0 if nothing
    /// was queued (muted, or empty after cleanup); or `u32::MAX` if the
    /// daemon could not confirm an id in time -- see `submit`'s doc
    /// comment.
    async fn say_selection(&self, opts: HashMap<String, OwnedValue>) -> fdo::Result<u32> {
        self.say_read(
            || selection::read(selection::Source::Primary),
            "selection",
            &opts,
        )
        .await
    }

    /// Speak the clipboard. Returns the utterance id; 0 if nothing was
    /// queued (muted, or empty after cleanup); or `u32::MAX` if the daemon
    /// could not confirm an id in time -- see `submit`'s doc comment.
    async fn say_clipboard(&self, opts: HashMap<String, OwnedValue>) -> fdo::Result<u32> {
        self.say_read(
            || selection::read(selection::Source::Clipboard),
            "clipboard",
            &opts,
        )
        .await
    }

    async fn pause(&self) {
        self.engine.send(Command::Pause);
    }

    async fn resume(&self) {
        self.engine.send(Command::Resume);
    }

    async fn play_pause(&self) {
        self.engine.send(Command::PlayPause);
    }

    /// Stop the current utterance and clear the queue.
    async fn stop(&self) {
        self.engine.send(Command::Stop);
    }

    async fn next(&self) {
        self.engine.send(Command::Next);
    }

    async fn skip_sentence(&self) {
        self.engine.send(Command::SkipSentence);
    }

    /// Drop everything pending; let the current utterance finish.
    async fn clear_queue(&self) {
        self.engine.send(Command::ClearQueue);
    }

    async fn cancel(&self, id: u32) {
        self.engine.send(Command::Cancel(id as u64));
    }

    /// Mute or unmute -- persistently, per spec §6 ("mute is sticky across
    /// utterances and persists to config").
    ///
    /// CRITICAL 1: this used to send `Command::SetMuted` and nothing else,
    /// which changed `cfg.muted` inside the engine alone. Any later
    /// `ApplyConfig` -- a settings-window save, a hand edit, a dotfile
    /// manager rewriting `~/.config/sayd` -- replaced the whole `Config` and
    /// unmuted the daemon on its own, and a restart lost the mute too.
    /// Routing it through the store makes the file and the engine agree;
    /// the engine's own transport behaviour on a false -> true transition is
    /// unchanged (`sayd-core`'s `ApplyConfig`, IMPORTANT 5).
    ///
    /// Off this thread because it writes to disk, and still fire-and-forget
    /// on the bus: the D-Bus signature has no return value to report a
    /// failed *write* through, and a mute that could not be written still
    /// mutes. `Muted` reports the truth either way, and the failure reaches
    /// the log and the tray.
    async fn set_muted(&self, muted: bool) {
        persist_in_background(self.store.clone(), move |s| s.set_muted(muted));
    }

    async fn quit(&self) {
        self.engine.send(Command::Shutdown);
    }

    #[zbus(property)]
    async fn state(&self) -> String {
        state_name(self.engine.snapshot().state).to_string()
    }

    #[zbus(property)]
    async fn muted(&self) -> bool {
        self.engine.snapshot().muted
    }

    #[zbus(property)]
    async fn voice(&self) -> String {
        self.engine.snapshot().voice
    }

    #[zbus(property)]
    async fn speed(&self) -> f64 {
        self.engine.snapshot().speed as f64
    }

    #[zbus(property)]
    async fn queue_length(&self) -> u32 {
        self.engine.snapshot().queue_len as u32
    }

    #[zbus(property)]
    async fn remaining_seconds(&self) -> f64 {
        self.engine.snapshot().remaining_secs
    }

    #[zbus(property)]
    async fn current_text(&self) -> String {
        self.engine.snapshot().current_text
    }

    #[zbus(property)]
    async fn current_id(&self) -> u32 {
        self.engine.snapshot().current_id as u32
    }

    /// Up to five pending utterances as `(id, leading text)`, in play order.
    ///
    /// Truncated for display; this is what the tray menu shows. `QueueLength`
    /// is the true count and may be larger than this list.
    #[zbus(property)]
    async fn queue_heads(&self) -> Vec<(u32, String)> {
        self.engine
            .snapshot()
            .queue_heads
            .into_iter()
            .map(|(id, text)| (u32::try_from(id).unwrap_or(0), text))
            .collect()
    }

    /// Empty unless `State` is `error`.
    #[zbus(property)]
    async fn error(&self) -> String {
        self.engine.snapshot().error.unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sayd_core::audio::VecSink;
    use sayd_core::config::Config;
    use sayd_core::synth::StubSynthesizer;

    /// An interface over a real engine and a store rooted in `dir`, so
    /// `SetMuted` writes somewhere harmless. The store is told the same
    /// config the engine was spawned with, as `ConfigStore::new` requires.
    fn iface_in(dir: &std::path::Path) -> SaydIface {
        let engine = sayd_core::handle::EngineHandle::spawn(
            Config::default(),
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        let store = Arc::new(ConfigStore::new(
            dir.join("config.toml"),
            engine.clone(),
            Config::default(),
        ));
        SaydIface::new(engine, store)
    }

    #[test]
    fn opts_default_to_the_source_policy() {
        let o = say_opts_from(&HashMap::new(), QueueSource::DBus);
        assert!(
            o.policy.is_none(),
            "an unset policy must fall through to the source default"
        );
        assert_eq!(o.source, QueueSource::DBus);
    }

    #[test]
    fn opts_parse_an_explicit_policy() {
        let mut m = HashMap::new();
        m.insert("policy".to_string(), OwnedValue::from(Str::from("replace")));
        let o = say_opts_from(&m, QueueSource::DBus);
        assert_eq!(o.policy, Some(Policy::Replace));
    }

    #[test]
    fn an_unknown_policy_string_is_ignored_rather_than_failing() {
        let mut m = HashMap::new();
        m.insert(
            "policy".to_string(),
            OwnedValue::from(Str::from("nonsense")),
        );
        let o = say_opts_from(&m, QueueSource::DBus);
        assert_eq!(
            o.policy, None,
            "an unknown policy falls back to the source default"
        );
    }

    #[test]
    fn opts_parse_voice_and_speed() {
        let mut m = HashMap::new();
        m.insert(
            "voice".to_string(),
            OwnedValue::from(Str::from("am_fenrir")),
        );
        m.insert("speed".to_string(), OwnedValue::from(1.25f64));
        let o = say_opts_from(&m, QueueSource::DBus);
        assert_eq!(o.voice.as_deref(), Some("am_fenrir"));
        assert_eq!(o.speed, Some(1.25));
    }

    #[test]
    fn state_renders_as_the_documented_lowercase_strings() {
        assert_eq!(state_name(State::Idle), "idle");
        assert_eq!(state_name(State::Speaking), "speaking");
        assert_eq!(state_name(State::Paused), "paused");
        assert_eq!(state_name(State::Error), "error");
    }

    /// Poll a fire-and-forget engine command's effect until it lands, with a
    /// deadline. `set_muted` sends a command down a channel and returns
    /// immediately; without this, the `say` that follows can reach the
    /// engine before the mute does and the test becomes an intermittent
    /// race rather than a deterministic check.
    fn wait_for(
        engine: &sayd_core::handle::EngineHandle,
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

    #[tokio::test]
    async fn say_returns_a_nonzero_id_and_zero_when_nothing_is_queued() {
        let dir = tempfile::tempdir().expect("tempdir");
        let i = iface_in(dir.path());
        let id = i
            .say("hello there.".into(), HashMap::new())
            .await
            .expect("accepted");
        assert_ne!(id, 0);

        i.set_muted(true).await;
        wait_for(&i.engine, "muted", |s| s.muted);
        let muted_id = i
            .say("nobody hears this".into(), HashMap::new())
            .await
            .expect("accepted");
        assert_eq!(muted_id, 0, "0 means accepted but nothing queued");
        i.engine.shutdown();
    }

    #[tokio::test]
    async fn say_reports_a_rejection_as_a_dbus_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = Config {
            max_chars: 5,
            ..Config::default()
        };
        let engine = sayd_core::handle::EngineHandle::spawn(
            cfg.clone(),
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000)),
        );
        let store = Arc::new(ConfigStore::new(
            dir.path().join("config.toml"),
            engine.clone(),
            cfg,
        ));
        let i = SaydIface::new(engine, store);
        assert!(i.say("far too long".into(), HashMap::new()).await.is_err());
        i.engine.shutdown();
    }

    #[tokio::test]
    async fn queue_heads_reports_pending_utterances_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let i = iface_in(dir.path());
        // Fill the queue. The first becomes current; the rest stay pending.
        for n in ["first one here.", "second one here.", "third one here."] {
            i.say(n.into(), HashMap::new()).await.expect("accepted");
        }
        let heads = i.queue_heads().await;
        assert!(!heads.is_empty(), "expected pending utterances, got none");
        // Ids are positive and ascending; text is non-empty.
        let mut last = 0u32;
        for (id, text) in &heads {
            assert!(*id > last, "ids must ascend: {id} after {last}");
            assert!(!text.is_empty());
            last = *id;
        }
        i.engine.shutdown();
    }

    /// Wait for the config file to satisfy `f`. `SetMuted` writes on a
    /// blocking task (it must not block the bus handler), so the write is
    /// not done when the call returns -- same reason `wait_for` exists for
    /// the engine side.
    fn wait_for_file(path: &std::path::Path, label: &str, f: impl Fn(&Config) -> bool) -> Config {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let (cfg, _) = Config::load_from(path);
            if f(&cfg) {
                return cfg;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {label}; file = {cfg:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// CRITICAL 1, first half: spec §6 -- "mute is sticky across utterances
    /// and persists to config". It did not reach the file at all, so a
    /// restart came back unmuted.
    #[tokio::test]
    async fn set_muted_persists_to_the_config_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let i = iface_in(dir.path());

        i.set_muted(true).await;
        wait_for(&i.engine, "the engine to mute", |s| s.muted);
        wait_for_file(&path, "the file to record the mute", |c| c.muted);

        i.set_muted(false).await;
        wait_for(&i.engine, "the engine to unmute", |s| !s.muted);
        wait_for_file(&path, "the file to record the unmute", |c| !c.muted);
        i.engine.shutdown();
    }

    /// CRITICAL 1, second half, and the measured failure: mute from the
    /// tray or `say mute`, then edit an unrelated field of `config.toml`,
    /// and the daemon used to unmute itself -- `ApplyConfig` replaces the
    /// whole `Config`, and the file had never been told about the mute. Ten
    /// minutes after muting for a meeting, a dotfile manager rewriting
    /// `~/.config/sayd` was enough to make the next submission audible.
    #[tokio::test]
    async fn a_mute_survives_an_unrelated_hand_edit_of_the_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let i = iface_in(dir.path());

        i.set_muted(true).await;
        wait_for(&i.engine, "the engine to mute", |s| s.muted);
        wait_for_file(&path, "the file to record the mute", |c| c.muted);

        // A hand edit of the *voice*, exactly as an editor makes it: read
        // what is there, change one field, write it back.
        let (mut edited, err) = Config::load_from(&path);
        assert_eq!(err, None);
        edited.voice = "am_fenrir".into();
        edited.save_to(&path).expect("hand edit");
        assert_eq!(
            i.store.reload(),
            crate::config_watch::ReloadOutcome::Applied
        );

        wait_for(&i.engine, "the voice change to land", |s| {
            s.voice == "am_fenrir"
        });
        assert!(
            i.engine.snapshot().muted,
            "the mute must survive a config change that never mentioned it"
        );
        i.engine.shutdown();
    }

    fn opts_with(key: &str, value: OwnedValue) -> HashMap<String, OwnedValue> {
        let mut m = HashMap::new();
        m.insert(key.to_string(), value);
        m
    }

    /// The one new key, read the way every other one is: absent, wrongly
    /// typed and explicitly false all mean "do not rewrite", because
    /// `say_opts_from`'s ignore-don't-reject rule is what lets a new CLI
    /// talk to an old daemon and the other way round.
    #[test]
    fn the_reword_opt_is_read_and_never_rejected() {
        assert!(wants_reword(&opts_with("reword", OwnedValue::from(true))));
        assert!(!wants_reword(&opts_with("reword", OwnedValue::from(false))));
        assert!(!wants_reword(&HashMap::new()));
        assert!(
            !wants_reword(&opts_with("reword", OwnedValue::from(Str::from("yes")))),
            "a wrongly-typed value is ignored, not an error"
        );
    }

    /// IMPORTANT 3: `--reword` reads the config without taking the stamp.
    ///
    /// The stamp is held across `ConfigStore::write_locked`'s unbounded disk
    /// write -- measured, a wedged write blocked this read for 1.500 s --
    /// and this read sits inside a budget `sayd-core`'s `config.rs` asserts
    /// at compile time and allocates it nothing. "Did not take a lock" is
    /// invisible in anything the call returns, so the store counts the reads
    /// (the same instrument, and the same reasoning, as
    /// `settings::model::tests::refresh_does_not_touch_the_store_while_a_write_is_in_flight`).
    ///
    /// `base_url` is deliberately unusable, so nothing is attempted and the
    /// test needs no provider: what is under test is the read in front of
    /// the attempt, not the attempt.
    #[tokio::test]
    async fn a_reword_reads_the_config_without_taking_the_stamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = Config {
            reword: Box::new(sayd_core::config::RewordConfig {
                base_url: String::new(),
                api_key_env: String::new(),
                ..sayd_core::config::RewordConfig::default()
            }),
            ..Config::default()
        };
        let engine = sayd_core::handle::EngineHandle::spawn(
            cfg.clone(),
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        let store = Arc::new(ConfigStore::new(
            dir.path().join("config.toml"),
            engine.clone(),
            cfg,
        ));
        let i = SaydIface::new(engine, store);

        let stamp_before = i.store.stamp_reads();
        let published_before = i.store.published_reads();
        let text = "Alice: where do you want to go for dinner".to_string();
        let mut held = None;
        let opts = opts_with("reword", OwnedValue::from(true));
        let spoken = pipeline::prepare(
            Written(text.clone()),
            SaydIface::ask(&mut held, &i.store, &opts),
        )
        .expect("well under the limit")
        .resolve()
        .await;

        assert_eq!(
            i.store.stamp_reads(),
            stamp_before,
            "the stamp is held across a disk write; this read must not want it"
        );
        assert_eq!(
            i.store.published_reads(),
            published_before + 1,
            "...and it must still have read the config: not reading it at all \
             is the other way to pass the assertion above"
        );
        assert_eq!(
            spoken.text, text,
            "and an unusable endpoint speaks it as written"
        );
        i.engine.shutdown();
    }

    /// CRITICAL 2 on this path: `maybe_reword`'s "every way this can fail
    /// ends in the original being spoken" has to hold for the way it can fail
    /// *after succeeding* -- a rewrite the guard accepted and the engine then
    /// refuses, which is reachable because the guard's ceiling
    /// (`original * 3 / 2 + 32`) and `max_chars` are unrelated numbers.
    ///
    /// The control is the first assertion: the same string submitted without
    /// a fallback is an error to the caller, so the second assertion is not
    /// passing for some other reason.
    #[tokio::test]
    async fn a_rewrite_the_engine_refuses_is_still_spoken_as_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = Config {
            max_chars: 40,
            ..Config::default()
        };
        let engine = sayd_core::handle::EngineHandle::spawn(
            cfg.clone(),
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        let store = Arc::new(ConfigStore::new(
            dir.path().join("config.toml"),
            engine.clone(),
            cfg,
        ));
        let i = SaydIface::new(engine, store);

        let original = "Alice asked about dinner tonight".to_string();
        let oversize = "a much longer rewrite than the engine will take".repeat(2);
        let opts = || say_opts_from(&HashMap::new(), QueueSource::DBus);

        assert!(
            i.submit(oversize.clone(), opts()).await.is_err(),
            "control: on its own this submission is refused"
        );
        let id = i
            .submit_spoken(
                Spoken {
                    text: oversize,
                    fallback: Some(original.clone()),
                },
                opts(),
            )
            .await
            .expect("the fallback must be accepted rather than the refusal reported");
        assert!(id > 0, "and it was queued, not discarded");
        wait_for(&i.engine, "the fallback to be spoken", |s| {
            s.current_text == original
        });
        i.engine.shutdown();
    }

    /// The `reword` key is read *only* by `maybe_reword`; it must not leak
    /// into what the engine is told about the submission. `SayOpts` gains no
    /// field for it, so this pins that an opts map carrying it still produces
    /// exactly the `SayOpts` it would have produced without it.
    #[test]
    fn the_reword_opt_changes_nothing_the_engine_is_told() {
        let with = say_opts_from(
            &opts_with("reword", OwnedValue::from(true)),
            QueueSource::DBus,
        );
        let without = say_opts_from(&HashMap::new(), QueueSource::DBus);
        assert_eq!(with.policy, without.policy);
        assert_eq!(with.voice, without.voice);
        assert_eq!(with.speed, without.speed);
        assert_eq!(with.source, without.source);
    }

    /// The relationship the whole inline-await design rests on: a `--reword`
    /// submission against a provider that accepts the connection and then
    /// never answers must come back with a *spoken utterance*, at the
    /// configured deadline -- not an error, and not at some other number.
    ///
    /// The deadline here is 3500 ms, chosen because both bounds that used to
    /// end this wait early are past it: `Config::load_str` used to clamp
    /// `timeout_ms` to 2000, and `sayd-cli` used to give up on any call at
    /// 3000. Neither does now -- the clamp is gone and `sayd-cli` leaves a
    /// submission carrying `reword` unbounded -- so a wait of about 3.5 s is
    /// the correct behaviour rather than the failure it once was. If either
    /// bound came back, this test fails: with the clamp, at the assertion
    /// that the loader kept the number; with a CLI-side bound, at the wait
    /// itself, which is longer than any constant `sayd-cli` still has.
    ///
    /// End to end through the real loader and against a real socket, because
    /// that is the only way to check it: the deadline is applied by
    /// `RewordPlan::resolve` in one crate, from a value parsed in another.
    ///
    /// In a build without the `reword` feature there is no client to make a
    /// request with, so this returns at once and still passes -- which is the
    /// other half of the promise: a `--reword` that cannot be honoured is
    /// spoken as written rather than refused. `--features reword` is the
    /// configuration where the timing can actually fail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reword_against_a_silent_provider_answers_with_a_spoken_utterance() {
        /// Past the ceiling `timeout_ms` used to be clamped to (2000) and
        /// past the bound `sayd-cli` used to put on every call (3000).
        const BUDGET_MS: u64 = 3500;

        let dir = tempfile::tempdir().expect("tempdir");
        // Held past the budget, so the provider is still silent when the
        // deadline fires: a server that closed the socket first would end
        // the wait for the wrong reason.
        let (base_url, provider) =
            crate::reword::silent_provider(std::time::Duration::from_millis(BUDGET_MS + 1_500));
        let (cfg, err) = Config::load_str(&format!(
            "[reword]\nbase_url = \"{base_url}\"\nprovider = \"generic\"\ntimeout_ms = {BUDGET_MS}\n"
        ));
        assert_eq!(err, None);
        assert_eq!(
            cfg.reword.timeout_ms, BUDGET_MS,
            "the loader must keep a deadline past the ceiling it used to \
             impose -- that ceiling is what this milestone removed"
        );
        assert!(
            !cfg.reword.notifications,
            "and automatic rewording stays off: an explicit --reword must not \
             need it -- only the `enabled` master, which is on by default"
        );

        let engine = sayd_core::handle::EngineHandle::spawn(
            cfg.clone(),
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        let store = Arc::new(ConfigStore::new(
            dir.path().join("config.toml"),
            engine.clone(),
            cfg.clone(),
        ));
        let i = SaydIface::new(engine, store);

        let started = std::time::Instant::now();
        let id = i
            .say(
                "Alice: where do you want to go for dinner".into(),
                opts_with("reword", OwnedValue::from(true)),
            )
            .await
            .expect("a rewrite that cannot happen is not an error");
        let elapsed = started.elapsed();

        assert_ne!(id, 0, "the original was queued and will be spoken");
        assert!(
            elapsed < std::time::Duration::from_millis(BUDGET_MS) * 2,
            "Say took {elapsed:?} against a {BUDGET_MS} ms deadline: the \
             deadline the caller was promised is not the one that ended the \
             wait"
        );
        // ...and in a build that can actually make the request, it really did
        // wait inline rather than passing for the trivial reason. Both bounds
        // together are the whole design: the wait is the configured deadline,
        // neither cut short by something else nor unbounded.
        //
        // `endpoint_seen` is the check that the request was made at all --
        // `attempt` sets it once a permit is in hand -- and it is keyed on
        // `base_url`, which is this test's own ephemeral port. It is asserted
        // rather than branched on because the process-wide `RewordState` is
        // shared with every other test in this binary: a breaker opened, or
        // both permits taken, elsewhere in the run would make the timing
        // below prove nothing, and that is worth a loud failure rather than a
        // quiet pass.
        #[cfg(feature = "reword")]
        {
            assert!(
                crate::reword::state().endpoint_seen(&cfg.reword),
                "the rewrite never reached the provider, so the timing below \
                 would prove nothing"
            );
            assert!(
                elapsed >= std::time::Duration::from_millis(cfg.reword.timeout_ms) / 2,
                "Say returned in {elapsed:?}, far inside the {} ms budget: the \
                 rewrite was not awaited inline",
                cfg.reword.timeout_ms
            );
        }

        i.engine.shutdown();
        provider.join().expect("the silent provider thread ends");
    }

    /// An interface whose `[reword]` names no endpoint at all, so nothing in
    /// the tests below makes a network request in either build: `context`
    /// refuses on `base_url` before a client is built. Every one of them is
    /// about what happens *upstream* of that.
    fn iface_with_no_reword_endpoint(dir: &std::path::Path) -> SaydIface {
        let cfg = Config {
            reword: Box::new(sayd_core::config::RewordConfig {
                base_url: String::new(),
                ..sayd_core::config::RewordConfig::default()
            }),
            ..Config::default()
        };
        let engine = sayd_core::handle::EngineHandle::spawn(
            cfg.clone(),
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        );
        let store = Arc::new(ConfigStore::new(
            dir.join("config.toml"),
            engine.clone(),
            cfg,
        ));
        SaydIface::new(engine, store)
    }

    /// IMPORTANT 1: a `--reword` must not be silently dropped because the
    /// engine is busy.
    ///
    /// `maybe_reword` used to fetch its config with `EngineHandle::config()`,
    /// a channel round trip bounded by `CONFIG_REPLY_TIMEOUT` at 250 ms, and
    /// read the `None` an engine thread mid-chunk returns as "no rewrite".
    /// Measured with six long utterances queued: 3 of 3 `say --reword` runs
    /// handed back the original after ~250 ms, with no log line, no D-Bus
    /// error, and nothing at all the caller could see. Pressing a
    /// `say --reword selection` keybind while the daemon is still speaking is
    /// the *normal* use of it, so the feature worked when tested on an idle
    /// daemon and quietly stopped under exactly the load it exists for.
    ///
    /// This stands the engine all the way down, which is that condition taken
    /// to its limit and is deterministic rather than a race against a
    /// synthesiser: `config()` cannot answer at all.
    ///
    /// `published_reads` is what is asserted because it is the mechanism
    /// itself, and it is the same fact in a build with the `reword` feature
    /// and one without: `maybe_reword` reads `ConfigStore::published` exactly
    /// once per `--reword` and never otherwise, so 0 is "the request was
    /// dropped
    /// before anything so much as looked at a configuration" and 1 is "it
    /// reached `RewordPlan::requested`". What happens past that point belongs
    /// to `crate::reword` and is pinned there. An `endpoint_seen` assertion
    /// was considered and rejected: it needs a live provider and one of the
    /// two process-wide permits, and this binary already runs two tests that
    /// take one.
    #[tokio::test]
    async fn a_reword_is_not_dropped_when_the_engine_cannot_answer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let i = iface_with_no_reword_endpoint(dir.path());

        i.engine.shutdown();
        assert!(
            i.engine.config().is_none(),
            "the premise of this test is an engine that cannot answer; if this \
             starts passing, the condition being reproduced is gone"
        );

        let before = i.store.published_reads();
        // The submission itself fails -- there is no engine left to take it --
        // and that is not what is under test here.
        let _ = i
            .say(
                "Alice: where do you want to go for dinner".into(),
                opts_with("reword", OwnedValue::from(true)),
            )
            .await;
        assert_eq!(
            i.store.published_reads(),
            before + 1,
            "a --reword whose engine cannot answer must still read the daemon's \
             own last-known config and offer the text to the rewrite path"
        );

        // ...and the other half of the promise: an ordinary `Say` pays
        // nothing for a feature it did not ask for, not even the mutex.
        let before = i.store.published_reads();
        let _ = i.say("hello there.".into(), HashMap::new()).await;
        assert_eq!(
            i.store.published_reads(),
            before,
            "a Say without the opt must not read the config at all"
        );
    }

    /// IMPORTANT 4: the selection and clipboard wiring, pinned by name.
    ///
    /// Deleting `let text = self.maybe_reword(text, &opts).await;` from
    /// *both* `say_selection` and `say_clipboard` passed 263 of 263, while
    /// the same deletion in `say` correctly failed a test. The missing
    /// compositor was only half the reason -- the daemon-side line was simply
    /// untouched by anything. `selection::read`'s seam is what lets these two
    /// methods be driven at all; see its doc comment for why the seam is
    /// there rather than a reader injected into `SaydIface`.
    ///
    /// Asserted on `published_reads` for the reason
    /// `a_reword_is_not_dropped_when_the_engine_cannot_answer` gives.
    #[tokio::test]
    async fn the_selection_paths_offer_their_text_to_the_rewrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let i = iface_with_no_reword_endpoint(dir.path());
        let spoken = "Alice: where do you want to go for dinner";
        let _seam = selection::test_seam::install(move |_source| Ok(spoken.to_string()));

        // Without the opt: read and submitted, and the config never touched.
        let before = i.store.published_reads();
        let id = i.say_selection(HashMap::new()).await.expect("accepted");
        assert_ne!(id, 0, "the selection was read and queued");
        let id = i.say_clipboard(HashMap::new()).await.expect("accepted");
        assert_ne!(id, 0, "the clipboard was read and queued");
        assert_eq!(
            i.store.published_reads(),
            before,
            "neither path may pay for a rewrite nobody asked for"
        );

        // With it: each one offers its text to the rewrite path exactly once.
        let before = i.store.published_reads();
        i.say_selection(opts_with("reword", OwnedValue::from(true)))
            .await
            .expect("accepted");
        assert_eq!(
            i.store.published_reads(),
            before + 1,
            "SaySelection dropped the --reword on the floor"
        );
        i.say_clipboard(opts_with("reword", OwnedValue::from(true)))
            .await
            .expect("accepted");
        assert_eq!(
            i.store.published_reads(),
            before + 2,
            "SayClipboard dropped the --reword on the floor"
        );

        i.engine.shutdown();
    }

    /// A failed selection read is still a D-Bus error, and still names which
    /// of the two it was -- the shared `say_read` body must not blur them.
    #[tokio::test]
    async fn a_failed_selection_read_is_reported_as_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let i = iface_with_no_reword_endpoint(dir.path());
        let _seam = selection::test_seam::install(|source| Err(format!("the {source} is empty")));

        let e = i
            .say_selection(HashMap::new())
            .await
            .expect_err("an empty selection is an error");
        assert!(
            e.to_string().contains("primary selection"),
            "the error must say which one it was: {e}"
        );
        let e = i
            .say_clipboard(HashMap::new())
            .await
            .expect_err("an empty clipboard is an error");
        assert!(
            e.to_string().contains("clipboard"),
            "the error must say which one it was: {e}"
        );
        i.engine.shutdown();
    }

    #[tokio::test]
    async fn queue_heads_is_empty_when_nothing_is_pending() {
        let dir = tempfile::tempdir().expect("tempdir");
        let i = iface_in(dir.path());
        assert!(i.queue_heads().await.is_empty());
        i.engine.shutdown();
    }
}
