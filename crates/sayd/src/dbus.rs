//! The `sh.sayd.Sayd1` D-Bus interface.
//!
//! A thin mapping from D-Bus onto engine commands. It holds an
//! `EngineHandle` and never blocks the engine: reads come from the published
//! snapshot, and selection reads -- which open their own Wayland connection
//! and block -- run on a blocking thread so they cannot stall the runtime.

use std::collections::HashMap;

use sayd_core::engine::{Command, SayOpts, State};
use sayd_core::handle::EngineHandle;
use sayd_core::queue::{Policy, Source as QueueSource};
use zbus::fdo;
use zbus::interface;
use zbus::zvariant::{OwnedValue, Str};

use crate::selection;

pub struct SaydIface {
    pub engine: EngineHandle,
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
    pub fn new(engine: EngineHandle) -> Self {
        SaydIface { engine }
    }

    /// Shared body of `Say`, `SaySelection` and `SayClipboard`.
    ///
    /// Returns the queued id, or 0 when the submission was accepted but
    /// nothing was queued (muted, or empty after cleanup). Safe to conflate
    /// with "no id" because the queue never mints id 0 (`Queue::next_id`
    /// starts at 1) and `CurrentId` already uses 0 to mean "nothing
    /// playing", so 0 cannot collide with a real id on either side of the
    /// interface.
    fn submit(&self, text: String, opts: SayOpts) -> fdo::Result<u32> {
        match self.engine.submit(text, opts) {
            // The queue's ids are `u64` and monotonically increasing, but the
            // wire type is `u32` (chosen to match `CurrentId` and `Cancel`,
            // and because a client has no use for ids beyond that range).
            // `Queue::next_id` starts at 1 and increments by 1 per accepted
            // utterance, so wrapping past `u32::MAX` needs over four billion
            // accepted utterances in one process lifetime -- not reachable
            // in practice, but truncating silently would hand a client the
            // wrong id if it ever were. Fall back to 0 ("nothing queued")
            // rather than lie about which utterance this is.
            Ok(Some(id)) => Ok(u32::try_from(id).unwrap_or(0)),
            Ok(None) => Ok(0),
            Err(e) => Err(fdo::Error::Failed(e)),
        }
    }
}

#[interface(name = "sh.sayd.Sayd1")]
impl SaydIface {
    /// Speak `text`. Returns the utterance id, or 0 if nothing was queued
    /// (muted, or empty after cleanup) -- safe, since the queue never mints
    /// id 0 and `CurrentId` uses 0 for "nothing playing".
    async fn say(&self, text: String, opts: HashMap<String, OwnedValue>) -> fdo::Result<u32> {
        self.submit(text, say_opts_from(&opts, QueueSource::DBus))
    }

    /// Speak the PRIMARY selection. Returns the utterance id, or 0 if
    /// nothing was queued (muted, or empty after cleanup) -- safe, since the
    /// queue never mints id 0 and `CurrentId` uses 0 for "nothing playing".
    async fn say_selection(&self, opts: HashMap<String, OwnedValue>) -> fdo::Result<u32> {
        let text = tokio::task::spawn_blocking(|| selection::read(selection::Source::Primary))
            .await
            .map_err(|e| fdo::Error::Failed(format!("selection read panicked: {e}")))?
            .map_err(fdo::Error::Failed)?;
        self.submit(text, say_opts_from(&opts, QueueSource::Hotkey))
    }

    /// Speak the clipboard. Returns the utterance id, or 0 if nothing was
    /// queued (muted, or empty after cleanup) -- safe, since the queue never
    /// mints id 0 and `CurrentId` uses 0 for "nothing playing".
    async fn say_clipboard(&self, opts: HashMap<String, OwnedValue>) -> fdo::Result<u32> {
        let text = tokio::task::spawn_blocking(|| selection::read(selection::Source::Clipboard))
            .await
            .map_err(|e| fdo::Error::Failed(format!("clipboard read panicked: {e}")))?
            .map_err(fdo::Error::Failed)?;
        self.submit(text, say_opts_from(&opts, QueueSource::Hotkey))
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

    async fn set_muted(&self, muted: bool) {
        self.engine.send(Command::SetMuted(muted));
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

    fn iface() -> SaydIface {
        SaydIface::new(sayd_core::handle::EngineHandle::spawn(
            Config::default(),
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000 * 10)),
        ))
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
        let i = iface();
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
        let i = SaydIface::new(sayd_core::handle::EngineHandle::spawn(
            Config {
                max_chars: 5,
                ..Config::default()
            },
            Box::new(StubSynthesizer::new()),
            Box::new(VecSink::new(24_000)),
        ));
        assert!(i.say("far too long".into(), HashMap::new()).await.is_err());
        i.engine.shutdown();
    }
}
