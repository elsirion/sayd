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

impl SaydIface {
    pub fn new(engine: EngineHandle, store: Arc<ConfigStore>) -> Self {
        SaydIface { engine, store }
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

    #[tokio::test]
    async fn queue_heads_is_empty_when_nothing_is_pending() {
        let dir = tempfile::tempdir().expect("tempdir");
        let i = iface_in(dir.path());
        assert!(i.queue_heads().await.is_empty());
        i.engine.shutdown();
    }
}
