//! The `sh.sayd.Sayd1` D-Bus interface.
//!
//! A thin mapping from D-Bus onto engine commands. It holds an
//! `EngineHandle` and never blocks the engine: reads come from the published
//! snapshot, and selection reads -- which open their own Wayland connection
//! and block -- run on a blocking thread so they cannot stall the runtime.

use std::collections::HashMap;
use std::sync::Arc;

use sayd_core::engine::{Command, SayOpts, State, Submitted};
use sayd_core::handle::EngineHandle;
use sayd_core::queue::{Policy, Source as QueueSource};
use zbus::fdo;
use zbus::interface;
use zbus::zvariant::{OwnedValue, Str};

use crate::config_watch::{persist_in_background, ConfigStore};
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

    /// Rewrite `text` if the caller asked for it, or hand it straight back.
    ///
    /// **Awaited inline**, unlike `notify::monitor::speak`, which detaches.
    /// The asymmetry is forced: `Say` returns an utterance id,
    /// `Engine::submit` is what allocates that id, and there is no way to
    /// allocate one ahead of the text -- `Queue` exposes no `iter_mut`,
    /// `Utterance::text` is never reassigned anywhere in `sayd-core`, and
    /// `Current`'s fields are private. Returning an id and rewriting
    /// afterwards would need a text-replacement hook inside the engine, a
    /// far larger change for a caller that is already blocked on a
    /// synchronous method call. So `say --reword "..."` can take up to
    /// `reword.timeout_ms` to return -- which is why `Config::load_str`
    /// clamps that at 2500 ms: `sayd-cli` bounds every D-Bus interaction at
    /// 3 s, and a rewrite that outlived the caller would turn an enhancement
    /// into "sayd is not responding".
    ///
    /// Every way this can fail ends in the original text being returned and
    /// therefore spoken -- no configured endpoint, no client in this build, a
    /// latched breaker, a provider that never answers. A `--reword` on a
    /// keybind must not stop speaking because an optional enhancement is
    /// misconfigured; the diagnosis is in the log, once per run.
    ///
    /// The config is fetched only *after* the opt has been seen, so an
    /// ordinary `Say` pays nothing at all for this -- not even the 250 ms
    /// round trip `EngineHandle::config` can cost.
    async fn maybe_reword(&self, text: String, opts: &HashMap<String, OwnedValue>) -> String {
        if !wants_reword(opts) {
            return text;
        }
        let engine = self.engine.clone();
        // On the blocking pool for the reason every other `EngineHandle`
        // round trip in this file is: it waits up to 250 ms on an engine
        // thread that may be mid-chunk. C2 again.
        let Ok(Some(cfg)) = tokio::task::spawn_blocking(move || engine.config()).await else {
            return text;
        };
        // `requested`, never `automatic`: `[reword] enabled` means "rewrite
        // my notifications without being asked", and this caller is asking.
        // Everything else -- endpoint, eligibility, all three breakers -- is
        // the same code, because both constructors share `admit`.
        match crate::reword::RewordPlan::requested(text, &cfg.reword) {
            // The plan owns the text it was admitted for, so what is sent is
            // what `will_reword` judged; `Err` hands the original straight
            // back.
            Ok(plan) => plan.resolve().await,
            Err(text) => text,
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
        let text = self.maybe_reword(text, &opts).await;
        self.submit(text, say_opts_from(&opts, QueueSource::DBus))
            .await
    }

    /// Speak the PRIMARY selection. Returns the utterance id; 0 if nothing
    /// was queued (muted, or empty after cleanup); or `u32::MAX` if the
    /// daemon could not confirm an id in time -- see `submit`'s doc
    /// comment.
    async fn say_selection(&self, opts: HashMap<String, OwnedValue>) -> fdo::Result<u32> {
        let text = tokio::task::spawn_blocking(|| selection::read(selection::Source::Primary))
            .await
            .map_err(|e| fdo::Error::Failed(format!("selection read panicked: {e}")))?
            .map_err(fdo::Error::Failed)?;
        let text = self.maybe_reword(text, &opts).await;
        self.submit(text, say_opts_from(&opts, QueueSource::Hotkey))
            .await
    }

    /// Speak the clipboard. Returns the utterance id; 0 if nothing was
    /// queued (muted, or empty after cleanup); or `u32::MAX` if the daemon
    /// could not confirm an id in time -- see `submit`'s doc comment.
    async fn say_clipboard(&self, opts: HashMap<String, OwnedValue>) -> fdo::Result<u32> {
        let text = tokio::task::spawn_blocking(|| selection::read(selection::Source::Clipboard))
            .await
            .map_err(|e| fdo::Error::Failed(format!("clipboard read panicked: {e}")))?
            .map_err(fdo::Error::Failed)?;
        let text = self.maybe_reword(text, &opts).await;
        self.submit(text, say_opts_from(&opts, QueueSource::Hotkey))
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
    /// never answers must come back with a *spoken utterance*, inside
    /// `sayd-cli`'s 3 s bound -- not a "sayd is not responding".
    ///
    /// The two halves of that are `reword.timeout_ms`, clamped to
    /// `REWORD_TIMEOUT_MAX_MS` (2500 ms) by `Config::load_str` no matter
    /// what the file says, and `sayd-cli`'s own `TIMEOUT` of 3 s. They live
    /// in two crates and neither can import the other's constant, so the
    /// relationship is checked here the only way it can be: end to end,
    /// through the real loader, against a real socket, with the deadline the
    /// clamp actually produced. `86400000` is what a hand-edited config can
    /// say and no spin row can.
    ///
    /// In a build without the `reword` feature there is no client to make a
    /// request with, so this returns at once and still passes -- which is the
    /// other half of the promise: a `--reword` that cannot be honoured is
    /// spoken as written rather than refused. `--features reword` is the
    /// configuration where the timing can actually fail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reword_against_a_silent_provider_still_answers_inside_the_cli_bound() {
        // `sayd-cli`'s own bound on any one D-Bus interaction, restated
        // because this crate cannot import it: `sayd-cli` is a binary.
        const CLI_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

        let dir = tempfile::tempdir().expect("tempdir");
        let (base_url, provider) = crate::reword::silent_provider(CLI_TIMEOUT);
        let (cfg, err) = Config::load_str(&format!(
            "[reword]\nbase_url = \"{base_url}\"\ntimeout_ms = 86400000\n"
        ));
        assert_eq!(err, None);
        assert_eq!(
            cfg.reword.timeout_ms,
            sayd_core::config::REWORD_TIMEOUT_MAX_MS,
            "a hand-edited timeout must be clamped on load, or the wait below \
             outlives the caller"
        );
        assert!(
            !cfg.reword.enabled,
            "and `enabled` stays off: an explicit --reword must not need it"
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
            elapsed < CLI_TIMEOUT,
            "Say took {elapsed:?} against sayd-cli's {CLI_TIMEOUT:?} bound; the \
             caller would have printed \"sayd is not responding\" for a rewrite \
             that was never going to arrive"
        );
        // ...and in a build that can actually make the request, it really did
        // wait inline rather than passing for the trivial reason. Both bounds
        // together are the whole design: long enough to be worth having, short
        // enough that the caller is still listening.
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

    #[tokio::test]
    async fn queue_heads_is_empty_when_nothing_is_pending() {
        let dir = tempfile::tempdir().expect("tempdir");
        let i = iface_in(dir.path());
        assert!(i.queue_heads().await.is_empty());
        i.engine.shutdown();
    }
}
