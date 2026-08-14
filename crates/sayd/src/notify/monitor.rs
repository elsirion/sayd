//! Owning the monitor connection to the session bus, and the receive loop.
//!
//! This is the layer that connects `decode` and `policy` to a real bus and a
//! real engine. It owns a *second*, dedicated `zbus::Connection`: after
//! `become_monitor` succeeds, sending anything on that connection is an
//! error, and the daemon's existing connection owns `sh.sayd.Sayd` and serves
//! the control interface. zbus encodes that rule in the signature --
//! `MonitoringProxy::become_monitor` takes `self` by value -- so the two
//! connections can never be confused for one another.
//!
//! `run` is spawned by the supervisor (M5 Task 5) when `notifications.enabled`
//! turns true, and aborted when it turns false. There is deliberately no
//! shutdown channel: a monitor holds no state that has to be flushed, so an
//! abort between messages loses nothing that was not already going to be lost
//! by the process exiting. `run` returns on its own only when the bus refuses
//! monitoring outright -- a policy decision, not an outage, see
//! `Refusal::Permanent`.
//!
//! Three things the loop has to get right, all of them measured in the spike
//! that preceded the spec rather than reasoned about here:
//!
//! - The stream carries the monitor connection's *own* bus traffic as well as
//!   what the match rule asked for; the first message off it in the spike was
//!   `NameLost`. `decode` filters on member and interface, so nothing here
//!   assumes the stream is all notifications.
//! - A `Notify` body must be deserialized as all eight fields at once, which
//!   `decode` does.
//! - Nothing may block the tokio runtime. `EngineHandle::submit` and
//!   `EngineHandle::config` are both blocking channel round trips with a
//!   250 ms bound, so both run on `spawn_blocking` here, exactly as the D-Bus
//!   interface does with `submit` (see `dbus::SaydIface::submit`).

use std::collections::HashSet;
use std::pin::Pin;
use std::time::{Duration, Instant};

use sayd_core::config::NotificationConfig;
use sayd_core::engine::SayOpts;
use sayd_core::handle::EngineHandle;
use sayd_core::queue::Source;
use zbus::export::futures_core::Stream;
use zbus::message::Type;
use zbus::{MatchRule, MessageStream};

use super::decode::decode;
use super::policy::{Decision, Limiter};

/// The interface a notification `Notify` call names, per the freedesktop
/// notification specification.
const NOTIFICATIONS_INTERFACE: &str = "org.freedesktop.Notifications";

/// How often `Limiter::due` is asked for coalesced follow-ups, and the cached
/// config is refreshed.
///
/// The follow-up half is why this exists at all: a "Signal: 3 more
/// notifications" line is owed the moment its window closes, and without a
/// timer it would sit unspoken until the *next* notification happened to
/// arrive -- which, for the burst that produced it, may be much later or
/// never. A second is plenty against the 30 s default window and cheap
/// enough to run while nothing is happening.
const DUE_INTERVAL: Duration = Duration::from_secs(1);

/// First delay after the monitor connection drops, doubling up to
/// [`MAX_RECONNECT_BACKOFF`].
///
/// Exponential rather than the fixed interval `main.rs` uses for audio-device
/// reacquisition (`RECOVERY_RETRY_INTERVAL`), because the two outages have
/// different shapes. A dead audio device typically comes back in seconds (a
/// PipeWire restart) and blocks *all* output while it is gone, so retrying
/// promptly and steadily is worth the cost. A session bus that has gone away
/// has, in practice, gone away for good -- the session is ending, and the
/// daemon is about to be torn down with it -- so a monitor that keeps
/// hammering a socket that is not coming back is pure noise. Starting at a
/// second still recovers a brief hiccup quickly.
const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);

/// Ceiling for the reconnect backoff.
///
/// Half a minute is short enough that a bus which does come back is picked up
/// well within a human's patience, and long enough that a standing outage
/// costs a handful of syscalls an hour instead of one a second.
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(30);

/// How long a connection must survive before the backoff is considered to
/// have done its job and is reset to [`INITIAL_RECONNECT_BACKOFF`].
///
/// Without this, a bus that accepts a connection and immediately drops it --
/// which is what a dying `dbus-daemon` looks like from here -- would reset
/// the backoff on every attempt and turn the whole escalation into a hot
/// one-second loop. Resetting only after the connection has actually been
/// useful for a while is what makes the backoff mean anything.
const STABLE_CONNECTION: Duration = Duration::from_secs(60);

/// Watch the session bus and speak the notifications the config allows.
///
/// Runs until the connection is permanently refused or the task is aborted.
pub async fn run(engine: EngineHandle) {
    run_on(engine, None).await
}

/// [`run`], against a specific bus address rather than the session bus.
///
/// Split out purely so the integration test below can point the monitor at a
/// private `dbus-daemon` without touching `DBUS_SESSION_BUS_ADDRESS`, which
/// is process-wide and would race every other test in this binary.
async fn run_on(engine: EngineHandle, address: Option<String>) {
    // Cached rather than read per message: `EngineHandle::config` is a
    // blocking round trip with a 250 ms bound, and doing one of those inside
    // the message path would put a blocking-pool hop between a notification
    // arriving and being spoken -- for a value that changes when a human
    // edits a file. Refreshed on the same tick that drives `due`, so an
    // `allow` change takes effect within a second. §6 asks for "the next
    // notification"; a second of lag on a hand edit is within that.
    let mut cfg = fetch_config(&engine).await.unwrap_or_default();
    // Both outlive a reconnect on purpose: a bus hiccup must not re-announce
    // every application the user has already been told about, nor hand a
    // noisy application a fresh window to speak immediately in.
    let mut limiter = Limiter::new();
    let mut announced: HashSet<String> = HashSet::new();

    let mut backoff = INITIAL_RECONNECT_BACKOFF;
    // Log-once for a standing outage, the pattern `main.rs` uses for audio
    // device recovery: the first failure is worth a line, the two hundredth
    // is not.
    let mut outage_logged = false;
    let mut submit_failure_logged = false;

    loop {
        let mut stream = match connect(address.as_deref()).await {
            Ok(s) => s,
            Err(Refusal::Permanent(reason)) => {
                // §2's failure table: "log once with the reason, run without
                // narration". A bus policy that forbids `BecomeMonitor` is
                // not an outage that clears on its own -- retrying it on a
                // timer would be asking the same question forever and
                // logging nothing new -- so the task ends here and the rest
                // of the daemon carries on unaffected, the same as a missing
                // StatusNotifierWatcher.
                eprintln!("info: {reason}; continuing without speaking notifications");
                return;
            }
            Err(Refusal::Transient(reason)) => {
                if !outage_logged {
                    eprintln!("warning: {reason}; retrying");
                    outage_logged = true;
                }
                tokio::time::sleep(backoff).await;
                backoff = next_backoff(backoff);
                continue;
            }
        };
        outage_logged = false;
        let connected_at = Instant::now();

        // `interval`'s first tick fires immediately, which is wanted here:
        // it refreshes the config right after a (re)connect. `Delay` rather
        // than the default `Burst` so a tick missed while a slow submission
        // was in flight does not come back as a run of catch-up ticks that
        // do the same work several times over.
        let mut ticker = tokio::time::interval(DUE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                message = next_message(&mut stream) => {
                    match message {
                        Some(Ok(msg)) => {
                            let Some(n) = decode(&msg) else { continue };
                            match limiter.decide(&n, &cfg, Instant::now()) {
                                Decision::Speak(text) => {
                                    speak(&engine, text, &mut submit_failure_logged).await;
                                }
                                // Counted against an open window; the
                                // follow-up comes out of `due` below when
                                // that window closes.
                                Decision::Count => {}
                                Decision::NotAllowed => log_discovery(&n.app_name, &mut announced),
                                // Allowed, but composed to nothing worth
                                // speaking. Deliberately *not* logged: this
                                // application is already on the allowlist,
                                // so the discovery line would tell the user
                                // to add something they have added, once per
                                // empty-summary notification -- the exact
                                // flood §4 keeps the log free of.
                                Decision::NothingToSay => {}
                            }
                        }
                        // The socket reader broadcasts exactly one `Err`,
                        // for the read failure that ends it, and then closes
                        // the channel -- so this is the connection dying,
                        // not one bad message. (A malformed *body* never
                        // reaches here at all: it is a well-formed message
                        // that `decode` returns `None` for.)
                        Some(Err(e)) => {
                            if !outage_logged {
                                eprintln!("warning: the notification monitor's bus connection failed: {e}; reconnecting");
                                outage_logged = true;
                            }
                            break;
                        }
                        None => {
                            if !outage_logged {
                                eprintln!("warning: the notification monitor's bus connection closed; reconnecting");
                                outage_logged = true;
                            }
                            break;
                        }
                    }
                }
                _ = ticker.tick() => {
                    if let Some(fresh) = fetch_config(&engine).await {
                        cfg = fresh;
                    }
                    for text in limiter.due(&cfg, Instant::now()) {
                        speak(&engine, text, &mut submit_failure_logged).await;
                    }
                }
            }
        }

        // A connection that lasted long enough to be doing its job earns a
        // fresh escalation; one that dropped immediately does not.
        if connected_at.elapsed() >= STABLE_CONNECTION {
            backoff = INITIAL_RECONNECT_BACKOFF;
        }
        tokio::time::sleep(backoff).await;
        backoff = next_backoff(backoff);
    }
}

/// Why a monitor connection could not be established.
enum Refusal {
    /// The bus answered, and the answer will not change: monitoring is
    /// forbidden by policy, or this bus has no such interface. Retrying asks
    /// the same question forever, so `run` stops instead.
    Permanent(String),
    /// Anything else -- no bus to connect to yet, a socket that went away
    /// mid-handshake. Worth retrying with backoff.
    Transient(String),
}

/// Build a dedicated connection, turn it into a monitor, and hand back its
/// message stream.
///
/// The `MessageStream` is created *before* `become_monitor` rather than after
/// it, deliberately. zbus's stream is a clone of an `async_broadcast`
/// receiver that only sees what is broadcast after it is activated, so
/// creating it afterwards leaves a window -- small, but real -- in which the
/// bus is already copying traffic to this connection and nothing is listening.
/// Nothing is lost by creating it first: the only extra traffic it sees is
/// this connection's own -- the `BecomeMonitor` reply, and the `NameAcquired`
/// signal the bus sends every new connection -- which `decode` rejects like
/// any other non-`Notify` message, exactly as it does for the `NameLost` the
/// spike saw arrive first.
async fn connect(address: Option<&str>) -> Result<MessageStream, Refusal> {
    let builder = match address {
        Some(a) => zbus::connection::Builder::address(a),
        None => zbus::connection::Builder::session(),
    };
    let builder = builder.map_err(|e| {
        Refusal::Transient(format!("no session bus for the notification monitor: {e}"))
    })?;
    let connection = builder.build().await.map_err(|e| {
        Refusal::Transient(format!(
            "the notification monitor could not connect to the session bus: {e}"
        ))
    })?;
    let stream = MessageStream::from(&connection);

    // §2's one match rule, verbatim. `type='method_call'` is not
    // redundant next to the member: `org.freedesktop.Notifications` also
    // carries signals (`NotificationClosed`, `ActionInvoked`), and narrowing
    // to calls keeps them off this connection entirely rather than relying on
    // `decode` to drop them after the bus has already copied them here.
    let rule = MatchRule::builder()
        .msg_type(Type::MethodCall)
        .interface(NOTIFICATIONS_INTERFACE)
        .map_err(|e| Refusal::Permanent(format!("the notification match rule is invalid: {e}")))?
        .member("Notify")
        .map_err(|e| Refusal::Permanent(format!("the notification match rule is invalid: {e}")))?
        .build();

    let proxy = zbus::fdo::MonitoringProxy::new(&connection)
        .await
        .map_err(|e| {
            Refusal::Transient(format!(
                "the notification monitor could not reach the bus driver: {e}"
            ))
        })?;
    // Consumes the proxy: nothing may be sent on this connection again. The
    // `connection` binding is dropped at the end of this function for the
    // same reason -- `stream` keeps the underlying connection alive by
    // itself, and holding a second, *sendable* handle to it would only be an
    // opportunity to misuse it.
    proxy
        .become_monitor(&[rule], 0)
        .await
        .map_err(classify_monitor_failure)?;

    Ok(stream)
}

/// Is a `BecomeMonitor` failure worth retrying?
///
/// `AccessDenied` is the bus policy saying no -- the case §2's failure table
/// is written for. `NotSupported`, `UnknownMethod` and `UnknownInterface` are
/// a bus too old to have the `Monitoring` interface at all. None of the four
/// changes while the daemon runs, so all four end the task. Everything else
/// (a socket that went away mid-call, a timeout) is treated as an outage.
fn classify_monitor_failure(e: zbus::fdo::Error) -> Refusal {
    match e {
        zbus::fdo::Error::AccessDenied(_)
        | zbus::fdo::Error::NotSupported(_)
        | zbus::fdo::Error::UnknownMethod(_)
        | zbus::fdo::Error::UnknownInterface(_) => Refusal::Permanent(format!(
            "the session bus refused to make sayd a monitor: {e}"
        )),
        other => Refusal::Transient(format!(
            "the notification monitor could not start monitoring: {other}"
        )),
    }
}

/// Double the backoff, capped.
fn next_backoff(current: Duration) -> Duration {
    // `saturating_mul` rather than `*`: `current` is bounded by the cap below
    // so overflow is unreachable, but a cheap saturating multiply is one
    // fewer panic to reason about in a daemon that must not die.
    current.saturating_mul(2).min(MAX_RECONNECT_BACKOFF)
}

/// The next message off `stream`, or `None` once it has ended.
///
/// `MessageStream` implements `futures_core::Stream`, whose `next()` adapter
/// lives in `futures-util` -- a crate `sayd` does not otherwise depend on.
/// `poll_fn` over the `Stream` impl gets the same thing from the trait zbus
/// already re-exports, so this needs no new dependency.
///
/// Cancel-safe, which is what makes it legal in a `select!` arm: the future
/// only registers a waker on the underlying broadcast receiver, so dropping
/// it un-polled leaves every queued message exactly where it was.
async fn next_message(stream: &mut MessageStream) -> Option<zbus::Result<zbus::Message>> {
    std::future::poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}

/// Read `notifications` out of the engine's live config, or `None` if the
/// engine could not answer.
///
/// On the blocking pool: `EngineHandle::config` waits up to 250 ms on an
/// engine thread that may be mid-chunk, and blocking a runtime worker for
/// that is exactly what `dbus::SaydIface::submit`'s doc comment describes
/// going wrong.
async fn fetch_config(engine: &EngineHandle) -> Option<NotificationConfig> {
    let engine = engine.clone();
    tokio::task::spawn_blocking(move || engine.config())
        .await
        .ok()
        .flatten()
        .map(|c| c.notifications)
}

/// The options every notification is submitted with.
///
/// No explicit policy: `Source::Notification`'s own default is `Policy::Front`
/// (spec §5, `sayd_core::queue::Source::default_policy`), which is precisely
/// what a notification wants -- ahead of the queue, without cutting off what
/// is currently being spoken. Naming `Policy::Front` here as well would be a
/// second place for that decision to live and drift from the first.
fn notification_opts() -> SayOpts {
    SayOpts {
        source: Source::Notification,
        ..SayOpts::default()
    }
}

/// Submit one announcement, logging only the first failure of a standing run
/// of them.
///
/// The composed text is *not* cleaned here, even though `policy::compose`
/// leaves runs of whitespace and newlines behind and its module doc says the
/// announcement must go through `sayd_core::cleanup::clean`. It does:
/// `Engine::submit` cleans every submission with the engine's own
/// `cleanup` config before queueing it (`sayd-core/src/engine.rs`, "let
/// cleaned = clean(&text, &self.cfg.cleanup)"), and this path reaches the
/// engine through exactly that call. Cleaning here as well would run the
/// whole regex pipeline twice per notification for an identical result, and
/// -- worse -- would leave two places claiming responsibility for an
/// invariant only one of them actually enforces.
async fn speak(engine: &EngineHandle, text: String, failure_logged: &mut bool) {
    let e = engine.clone();
    let result = tokio::task::spawn_blocking(move || e.submit(text, notification_opts())).await;
    match result {
        Ok(Ok(_)) => *failure_logged = false,
        Ok(Err(reason)) => {
            if !*failure_logged {
                eprintln!("warning: could not speak a notification: {reason}");
                *failure_logged = true;
            }
        }
        Err(e) => {
            if !*failure_logged {
                eprintln!("warning: the notification submission task failed: {e}");
                *failure_logged = true;
            }
        }
    }
}

/// §4's discovery log: every application whose notifications are declined for
/// not being on the allowlist is named once, so an empty `allow` is a
/// starting point rather than a dead end.
///
/// The seen-set is keyed case-insensitively even though the line prints the
/// name as the application spelled it. `allow` is matched case-insensitively
/// (`policy::is_allowed`), so an application that spells itself "Signal"
/// today and "signal" tomorrow is one entry to add, not two -- and telling
/// the user twice would be the flood this log is throttled to avoid, in
/// miniature.
fn log_discovery(app_name: &str, announced: &mut HashSet<String>) {
    if announced.insert(app_name.to_lowercase()) {
        eprintln!(
            "info: notification from {app_name:?} \
             (not in notifications.allow; add it to speak these)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};

    use sayd_core::audio::VecSink;
    use sayd_core::config::{Config, NotificationConfig};
    use sayd_core::queue::Policy;
    use zbus::zvariant::OwnedValue;

    /// Spec §5, pinned where the decision is actually made. The submission
    /// carries no explicit policy on purpose, so this checks both halves:
    /// that the source is right, and that the source's own default is the
    /// `Front` placement §5 asks for.
    #[test]
    fn notifications_are_submitted_at_the_front_as_notifications() {
        let opts = notification_opts();
        assert_eq!(opts.source, Source::Notification);
        assert!(
            opts.policy.is_none(),
            "no explicit policy: the source's default is what carries §5"
        );
        assert_eq!(Source::Notification.default_policy(), Policy::Front);
    }

    /// §4: once per name per run, and case-insensitively so one application
    /// that varies its own spelling is still one line.
    #[test]
    fn each_undeclared_application_is_logged_once() {
        let mut announced = HashSet::new();
        assert!(announced.insert("signal".to_string()));
        // Re-running the real function must not add a second entry for the
        // same name in a different case.
        log_discovery("Signal", &mut announced);
        log_discovery("SIGNAL", &mut announced);
        assert_eq!(announced.len(), 1);
        log_discovery("Fractal", &mut announced);
        assert_eq!(announced.len(), 2);
    }

    /// The backoff escalates and then stops, rather than growing without
    /// bound into a monitor that would never notice the bus coming back.
    #[test]
    fn the_reconnect_backoff_doubles_up_to_the_cap() {
        let mut d = INITIAL_RECONNECT_BACKOFF;
        let mut seen = vec![d];
        for _ in 0..12 {
            d = next_backoff(d);
            seen.push(d);
        }
        assert_eq!(seen[1], Duration::from_secs(2));
        assert_eq!(seen[2], Duration::from_secs(4));
        assert_eq!(*seen.last().expect("non-empty"), MAX_RECONNECT_BACKOFF);
        assert!(seen.iter().all(|d| *d <= MAX_RECONNECT_BACKOFF));
    }

    /// A bus policy that forbids monitoring is answered once and never
    /// retried; anything that looks like an outage is retried.
    #[test]
    fn only_a_refusal_is_permanent() {
        for e in [
            zbus::fdo::Error::AccessDenied("no".into()),
            zbus::fdo::Error::NotSupported("no".into()),
            zbus::fdo::Error::UnknownMethod("no".into()),
            zbus::fdo::Error::UnknownInterface("no".into()),
        ] {
            assert!(
                matches!(classify_monitor_failure(e), Refusal::Permanent(_)),
                "a refusal must end the task rather than be retried forever"
            );
        }
        assert!(matches!(
            classify_monitor_failure(zbus::fdo::Error::IOError("socket went away".into())),
            Refusal::Transient(_)
        ));
        assert!(matches!(
            classify_monitor_failure(zbus::fdo::Error::NoReply("timed out".into())),
            Refusal::Transient(_)
        ));
    }

    /// A private `dbus-daemon` on its own socket, killed when the test ends.
    ///
    /// Its address is handed to each connection explicitly rather than being
    /// exported as `DBUS_SESSION_BUS_ADDRESS`: that variable is process-wide,
    /// and every other test in this binary shares the process.
    struct TestBus {
        child: Child,
        address: String,
        // Holds the config file's directory open for the daemon's lifetime.
        _dir: tempfile::TempDir,
    }

    impl TestBus {
        /// `None` when `dbus-daemon` is not installed, so a machine without
        /// one skips rather than fails.
        fn start() -> Option<TestBus> {
            let dir = tempfile::tempdir().expect("a temp dir for the bus config");
            let config = dir.path().join("bus.conf");
            // `eavesdrop="true"` is what lets `BecomeMonitor` succeed at all
            // -- the whole point of the test. `unix:tmpdir=/tmp` keeps the
            // socket off the session's real one.
            std::fs::write(
                &config,
                "<busconfig>\n\
                 <type>session</type>\n\
                 <listen>unix:tmpdir=/tmp</listen>\n\
                 <policy context=\"default\">\n\
                 <allow send_destination=\"*\" eavesdrop=\"true\"/>\n\
                 <allow eavesdrop=\"true\"/>\n\
                 <allow own=\"*\"/>\n\
                 </policy>\n\
                 </busconfig>\n",
            )
            .expect("writing the bus config");

            // Deliberately not `--fork`: forking detaches the daemon and the
            // process this spawns exits immediately, leaving nothing to kill
            // on `Drop` short of parsing a pid back out. Run in the
            // foreground as a direct child instead -- it still prints its
            // address and then serves.
            let mut child = match Command::new("dbus-daemon")
                .arg(format!("--config-file={}", config.display()))
                .arg("--print-address")
                .arg("--nofork")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(c) => c,
                // The gate, and *only* this: a machine without `dbus-daemon`
                // skips. Any other spawn failure (a `dbus-daemon` that is
                // there but cannot be executed) is a broken environment, and
                // silently skipping would turn it into a green run that
                // tested nothing.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
                Err(e) => panic!("dbus-daemon is on PATH but would not start: {e}"),
            };

            let stdout = child.stdout.take().expect("piped stdout");
            let mut address = String::new();
            BufReader::new(stdout)
                .read_line(&mut address)
                .expect("dbus-daemon prints its address");
            assert!(
                !address.trim().is_empty(),
                "dbus-daemon started but printed no address"
            );

            Some(TestBus {
                child,
                address: address.trim().to_string(),
                _dir: dir,
            })
        }
    }

    impl Drop for TestBus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Stands in for mako: owns `org.freedesktop.Notifications` so the bus
    /// has somewhere to route the call, and answers it with an id.
    ///
    /// Without an owner the bus rejects the method call before routing it and
    /// the monitor never sees it -- which is the whole path under test.
    struct StubNotificationDaemon;

    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl StubNotificationDaemon {
        #[allow(clippy::too_many_arguments)]
        async fn notify(
            &self,
            _app_name: String,
            _replaces_id: u32,
            _app_icon: String,
            _summary: String,
            _body: String,
            _actions: Vec<String>,
            _hints: HashMap<String, OwnedValue>,
            _expire_timeout: i32,
        ) -> u32 {
            1
        }
    }

    /// Records every text the engine hands to synthesis.
    ///
    /// `Snapshot` is the wrong instrument for this test: a stub synthesizer
    /// returns instantly, so an utterance is queued, made current and
    /// finished between two polls of `current_text`, leaving the assertion
    /// racing a window measured in microseconds. What actually has to be
    /// true is that the announcement *reached* the engine, and this records
    /// that permanently.
    struct RecordingSynthesizer {
        spoken: Arc<Mutex<Vec<String>>>,
    }

    impl sayd_core::synth::Synthesizer for RecordingSynthesizer {
        fn phonemize(&mut self, text: &str, _voice: &str) -> String {
            self.spoken
                .lock()
                .expect("spoken mutex")
                .push(text.to_string());
            text.to_string()
        }
        fn fits(&mut self, _phonemes: &str) -> bool {
            true
        }
        fn synth(&mut self, phonemes: &str, _voice: &str, _speed: f32) -> Result<Vec<f32>, String> {
            Ok(vec![0.0; phonemes.len()])
        }
        fn unload(&mut self) {}
        fn is_loaded(&self) -> bool {
            true
        }
    }

    fn engine_allowing(app: &str) -> (EngineHandle, Arc<Mutex<Vec<String>>>) {
        let cfg = Config {
            notifications: NotificationConfig {
                enabled: true,
                allow: vec![app.to_string()],
                ..NotificationConfig::default()
            },
            ..Config::default()
        };
        let spoken = Arc::new(Mutex::new(Vec::new()));
        let engine = EngineHandle::spawn(
            cfg,
            Box::new(RecordingSynthesizer {
                spoken: spoken.clone(),
            }),
            Box::new(VecSink::new(24_000 * 60)),
        );
        (engine, spoken)
    }

    fn engine_has(spoken: &Arc<Mutex<Vec<String>>>, text: &str) -> bool {
        spoken
            .lock()
            .expect("spoken mutex")
            .iter()
            .any(|t| t == text)
    }

    /// The whole path, against a real bus: an application calls `Notify`, a
    /// stub daemon owning the name receives it, and the monitor turns it into
    /// a submission. Gated on `dbus-daemon` being present so a machine
    /// without one skips rather than fails.
    #[tokio::test]
    async fn a_notification_on_a_real_bus_reaches_the_engine() {
        let Some(bus) = TestBus::start() else {
            eprintln!("skipping: dbus-daemon not on PATH");
            return;
        };

        // The notification daemon mako would be.
        let _daemon = zbus::connection::Builder::address(bus.address.as_str())
            .expect("address")
            .name("org.freedesktop.Notifications")
            .expect("name")
            .serve_at("/org/freedesktop/Notifications", StubNotificationDaemon)
            .expect("serve")
            .build()
            .await
            .expect("stub notification daemon");

        let (engine, spoken) = engine_allowing("Signal");
        let monitor = tokio::spawn(run_on(engine.clone(), Some(bus.address.clone())));

        // The application. A third connection, as in the spike.
        let app = zbus::connection::Builder::address(bus.address.as_str())
            .expect("address")
            .build()
            .await
            .expect("application connection");

        // `run_on` becomes a monitor asynchronously, so a single `Notify`
        // sent immediately could legitimately predate the match rule. Retry
        // until the engine has it or the deadline passes; the rate limiter
        // counts the extras against one window rather than speaking them, so
        // repeating is harmless.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut arrived = false;
        while Instant::now() < deadline {
            let args = (
                "Signal",
                0u32,
                "",
                "hello",
                "",
                Vec::<String>::new(),
                HashMap::<String, zbus::zvariant::Value>::new(),
                5000i32,
            );
            let _: u32 = app
                .call_method(
                    Some("org.freedesktop.Notifications"),
                    "/org/freedesktop/Notifications",
                    Some("org.freedesktop.Notifications"),
                    "Notify",
                    &args,
                )
                .await
                .expect("the stub daemon answers")
                .body()
                .deserialize()
                .expect("an id");

            if engine_has(&spoken, "Signal: hello") {
                arrived = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        monitor.abort();
        engine.shutdown();
        assert!(
            arrived,
            "a Notify call on the bus never reached the engine as 'Signal: hello'"
        );
    }

    /// The other half of the allowlist: an application the user has not named
    /// is not spoken, however many times it calls `Notify`. Same bus, same
    /// path, so a regression that ignored the allowlist entirely could not
    /// hide behind the test above passing.
    #[tokio::test]
    async fn an_unlisted_application_is_not_spoken() {
        let Some(bus) = TestBus::start() else {
            eprintln!("skipping: dbus-daemon not on PATH");
            return;
        };

        let _daemon = zbus::connection::Builder::address(bus.address.as_str())
            .expect("address")
            .name("org.freedesktop.Notifications")
            .expect("name")
            .serve_at("/org/freedesktop/Notifications", StubNotificationDaemon)
            .expect("serve")
            .build()
            .await
            .expect("stub notification daemon");

        // Allows "Signal"; the application below is not it.
        let (engine, spoken) = engine_allowing("Signal");
        let monitor = tokio::spawn(run_on(engine.clone(), Some(bus.address.clone())));

        let app = zbus::connection::Builder::address(bus.address.as_str())
            .expect("address")
            .build()
            .await
            .expect("application connection");

        // Send a control notification from an allowed application after the
        // unlisted one, and wait for *that*: once the allowed one has been
        // spoken, the monitor has demonstrably been running and processing,
        // so the unlisted one's absence means it was declined rather than
        // merely not arrived yet.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut control_arrived = false;
        while Instant::now() < deadline {
            for (app_name, summary) in [("Fractal", "unlisted"), ("Signal", "listed")] {
                let args = (
                    app_name,
                    0u32,
                    "",
                    summary,
                    "",
                    Vec::<String>::new(),
                    HashMap::<String, zbus::zvariant::Value>::new(),
                    5000i32,
                );
                let _: u32 = app
                    .call_method(
                        Some("org.freedesktop.Notifications"),
                        "/org/freedesktop/Notifications",
                        Some("org.freedesktop.Notifications"),
                        "Notify",
                        &args,
                    )
                    .await
                    .expect("the stub daemon answers")
                    .body()
                    .deserialize()
                    .expect("an id");
            }
            if engine_has(&spoken, "Signal: listed") {
                control_arrived = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // `Policy::Front` puts each accepted notification *ahead* of what is
        // already queued, so an unlisted one wrongly accepted a moment
        // earlier would be synthesized after the control one, not before it.
        // Seeing the control line is therefore not enough on its own; let
        // the engine drain what it has before looking.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let spoke_unlisted = engine_has(&spoken, "Fractal: unlisted");
        monitor.abort();
        engine.shutdown();
        assert!(
            control_arrived,
            "the monitor never spoke the allowed control notification, so this \
             test proves nothing about the unlisted one"
        );
        assert!(
            !spoke_unlisted,
            "an application absent from notifications.allow must not be spoken"
        );
    }
}
