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

use sayd_core::config::Config;
use sayd_core::engine::SayOpts;
use sayd_core::handle::EngineHandle;
use sayd_core::queue::Source;
use zbus::export::futures_core::Stream;
use zbus::message::Type;
use zbus::{MatchRule, MessageStream};

use super::decode::{decode, Decoded};
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

/// Ceiling on the entire `connect` handshake -- the builder's auth handshake
/// *and* `become_monitor`'s round trip, not just one method call.
///
/// zbus's `method_timeout` defaults to `None`, and the auth handshake in
/// `Builder::build` happens before there is even a `Connection` to set one
/// on, so `.method_timeout()` alone does not cover this. Measured: a unix
/// socket that accepts a connection and then never writes a byte left
/// `run_on` parked forever with nothing logged and no retry -- silent in
/// exactly the way §2's failure table exists to prevent, arriving through a
/// door the table's three rows do not name. A dying `dbus-daemon`'s socket,
/// or one wedged on fd exhaustion, looks exactly like this from here. Ten
/// seconds is generous for a real handshake (the integration tests below
/// complete in well under one) and short enough that a stalled bus is
/// retried, logged and backed off like any other transient failure instead
/// of hanging the monitor for the life of the process.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard cap on how many distinct application names §4's discovery log
/// remembers and reports in one run.
///
/// `app_name` is chosen by the calling application, not validated by
/// `sayd` (see `decode`'s doc comment), so nothing stops a compromised
/// application, a `notify-send -a "$counter"` loop, or one that embeds a
/// session id in its own name from minting a fresh name on every call.
/// Measured: 2000 `Notify` calls with 2000 distinct names produced 2000
/// permanent `HashSet` entries and 2000 `info:` lines to the journal in
/// under a second, with nothing backpressuring either. §4's "once per name"
/// throttle is exactly what becomes the leak without a ceiling. 256 names is
/// ample for any real desktop's worth of applications a user has not yet
/// allowlisted; past that this is no longer discovery, it is a flood, and
/// the right response is the one §4 already gives a chatty *allowed*
/// application -- say it once and stop.
const MAX_ANNOUNCED: usize = 256;

/// Longest `app_name` prefix the discovery log will remember or print.
///
/// Bounds the cost of one entry, not just the count of them: a name a
/// kilobyte long costs as much to hash, store and print as a short one
/// otherwise, and the freedesktop spec places no length limit on it.
const MAX_APP_NAME_LEN: usize = 256;

/// Why [`run`] stopped.
///
/// IMPORTANT 3: the supervisor (`main.rs`'s `NotifyMonitorSupervisor`) has to
/// tell a monitor that was *told no* apart from one that died, because the
/// right response to the two is opposite -- never restart, versus restart
/// after a backoff -- and `run` returning `()` made them indistinguishable.
/// The cost of guessing wrong was measured: a bus denying
/// `org.freedesktop.DBus.Monitoring` produced 37 log lines and 18 full
/// connect/auth/`BecomeMonitor` cycles in 90 seconds, forever, for a spec row
/// (§2) that says "log once with the reason, run without narration".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The bus answered `become_monitor` with a refusal that will not change
    /// while this daemon runs -- policy, or a bus with no `Monitoring`
    /// interface at all (see `classify_monitor_failure`). The reason has
    /// already been logged, once, by the time this is returned.
    Refused,
    /// The task stopped for any other reason. `run`'s own loops never
    /// produce this -- every other way out of them is a retry -- so it is
    /// the supervisor's name for "the task is gone and it did not say it was
    /// refused": it panicked, or some future edit of `run` grew a second way
    /// to stop. Restart-with-backoff is the right answer to that, and having
    /// a value to name it by is what keeps that branch honest instead of a
    /// bare `else` that quietly also swallows refusals.
    Ended,
}

/// Watch the session bus and speak the notifications the config allows.
///
/// Runs until the connection is permanently refused or the task is aborted.
pub async fn run(engine: EngineHandle) -> Outcome {
    run_on(engine, None).await
}

/// [`run`], against a specific bus address rather than the session bus.
///
/// Split out purely so the integration test below can point the monitor at a
/// private `dbus-daemon` without touching `DBUS_SESSION_BUS_ADDRESS`, which
/// is process-wide and would race every other test in this binary.
async fn run_on(engine: EngineHandle, address: Option<String>) -> Outcome {
    // Cached rather than read per message: `EngineHandle::config` is a
    // blocking round trip with a 250 ms bound, and doing one of those inside
    // the message path would put a blocking-pool hop between a notification
    // arriving and being spoken -- for a value that changes when a human
    // edits a file. The whole `Config`, not just `NotificationConfig`: `speak`
    // needs `max_chars` too (Important 3), and caching one struct is no more
    // expensive than caching a piece of it. Refreshed synchronously right
    // after every (re)connect, and again on the tick that drives `due`, so an
    // `allow` change takes effect within a second. §6 asks for "the next
    // notification"; a second of lag on a hand edit is within that. Starts
    // as `Config::default()` -- an empty `allow` and a real `max_chars` -- so
    // an engine that never answers at all fails safe rather than open.
    let mut cfg = Config::default();
    // Both outlive a reconnect on purpose: a bus hiccup must not re-announce
    // every application the user has already been told about, nor hand a
    // noisy application a fresh window to speak immediately in.
    let mut limiter = Limiter::new();
    let mut announced = Announced::default();

    let mut backoff = INITIAL_RECONNECT_BACKOFF;
    // Log-once for a standing outage, the pattern `main.rs` uses for audio
    // device recovery: the first failure is worth a line, the two hundredth
    // is not.
    let mut outage_logged = false;
    let mut submit_failure_logged = false;
    let mut malformed_logged = false;
    let mut malformed_count: u64 = 0;

    loop {
        let mut stream = match connect_with_timeout(address.as_deref()).await {
            Ok(s) => s,
            Err(Refusal::Permanent(reason)) => {
                // §2's failure table: "log once with the reason, run without
                // narration". A bus policy that forbids `BecomeMonitor` is
                // not an outage that clears on its own -- retrying it on a
                // timer would be asking the same question forever and
                // logging nothing new -- so the task ends here and the rest
                // of the daemon carries on unaffected, the same as a missing
                // StatusNotifierWatcher.
                //
                // IMPORTANT 3: this is the *only* line either this task or
                // its supervisor prints about a refusal, for the life of the
                // process, so it also has to say what would make sayd ask
                // again -- the supervisor latches on `Outcome::Refused` and
                // will not respawn until `notifications.enabled` is toggled.
                eprintln!(
                    "info: {reason}; continuing without speaking notifications \
                     (toggle notifications.enabled off and on to retry)"
                );
                return Outcome::Refused;
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

        // Refreshed synchronously here, before `select!` below ever runs
        // (Minor 9). `select!` polls the message stream and the ticker
        // together and picks pseudo-randomly between branches that are both
        // ready, so a message already queued when the loop starts could
        // otherwise race the ticker's first tick -- and if the very first
        // fetch at daemon startup had failed, that race would judge a real
        // notification against the empty-`allow` default, decline it, and
        // permanently record it in `announced` as not-on-the-allowlist, which
        // is never corrected once logged. Doing the fetch here, before
        // `select!` is ever entered, removes the race instead of shrinking
        // it. Nothing already queued on the stream is lost while this runs:
        // zbus backpressures a slow consumer rather than dropping (see the
        // module doc's third fact).
        if let Some(fresh) = fetch_config(&engine).await {
            cfg = fresh;
        }

        // Starts one interval *after* now, not immediately: the fetch above
        // already did what the old immediate first tick existed for, and
        // firing it again a moment later would just be a second, redundant
        // 250 ms round trip. `Delay` rather than the default `Burst` so a
        // tick missed while a slow submission was in flight does not come
        // back as a run of catch-up ticks that do the same work several
        // times over.
        let mut ticker =
            tokio::time::interval_at(tokio::time::Instant::now() + DUE_INTERVAL, DUE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                message = next_message(&mut stream) => {
                    match message {
                        Some(Ok(msg)) => {
                            // MINOR 3: fail closed on `enabled`, rather than
                            // relying entirely on the supervisor's `abort` to
                            // stop narration. This loop reads every other
                            // field of `cfg.notifications` (`allow`,
                            // `cooldown_secs`, `speak_*`) and ignored the one
                            // that says whether to speak at all -- so the
                            // window between the config changing and the
                            // abort landing was narration the user had just
                            // switched off (measured: notifications at +7ms
                            // and +358ms after the change were still spoken).
                            // Normally sub-second; unbounded if the publish
                            // loop's own read of the store is stalled, which
                            // is exactly the state CRITICAL 1 is about.
                            // Checked before `decode` so a disabled monitor
                            // is silent in the discovery log too, not merely
                            // in the speaker.
                            if !cfg.notifications.enabled {
                                continue;
                            }
                            match decode(&msg) {
                                Decoded::Skip => {}
                                // Spec §2: "skip that message and count it".
                                // Logged once per standing run of them, the
                                // same pattern as `outage_logged` below --
                                // one malformed sender is worth a line, a
                                // hundred more of the same are not.
                                Decoded::Malformed => {
                                    malformed_count += 1;
                                    if !malformed_logged {
                                        eprintln!(
                                            "warning: a Notify call's body did not decode \
                                             as the eight-field notification signature; \
                                             skipping it ({malformed_count} so far this run)"
                                        );
                                        malformed_logged = true;
                                    }
                                }
                                Decoded::Notification(n) => {
                                    match limiter.decide(&n, &cfg.notifications, Instant::now()) {
                                        Decision::Speak(text) => {
                                            speak(&engine, text, cfg.max_chars, &mut submit_failure_logged).await;
                                        }
                                        // Counted against an open window; the
                                        // follow-up comes out of `due` below
                                        // when that window closes.
                                        Decision::Count => {}
                                        Decision::NotAllowed => log_discovery(&n.app_name, &mut announced),
                                        // Allowed, but composed to nothing
                                        // worth speaking. Deliberately *not*
                                        // logged: this application is already
                                        // on the allowlist, so the discovery
                                        // line would tell the user to add
                                        // something they have added, once per
                                        // empty-summary notification -- the
                                        // exact flood §4 keeps the log free
                                        // of.
                                        Decision::NothingToSay => {}
                                    }
                                }
                            }
                        }
                        // The socket reader broadcasts exactly one `Err`,
                        // for the read failure that ends it, and then closes
                        // the channel -- so this is the connection dying,
                        // not one bad message. (A malformed *body* never
                        // reaches here at all: it is a well-formed message
                        // that `decode` returns `Decoded::Malformed` for,
                        // handled above.)
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
                    // MINOR 3, the other half: a coalesced follow-up is owed
                    // from before the config changed, so a disabled monitor
                    // must not speak it either. Drained from the limiter
                    // regardless, so nothing accumulates to be said later.
                    let due = limiter.due(&cfg.notifications, Instant::now());
                    if cfg.notifications.enabled {
                        for text in due {
                            speak(&engine, text, cfg.max_chars, &mut submit_failure_logged).await;
                        }
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
    // `.expect()`, not `?`, on both of these (Minor 10): `interface`/`member`
    // return `Result` for callers building a rule from arbitrary strings, but
    // these two are hardcoded constants that are well-formed D-Bus names by
    // construction -- `NOTIFICATIONS_INTERFACE` and `"Notify"` cannot fail to
    // parse today or on any future recompile, so modelling that as a bus
    // *policy* refusal (what `Refusal::Permanent` otherwise means throughout
    // this module) misrepresents what would actually be a programming error
    // caught the first time this code ran at all.
    let rule = MatchRule::builder()
        .msg_type(Type::MethodCall)
        .interface(NOTIFICATIONS_INTERFACE)
        .expect("NOTIFICATIONS_INTERFACE is a hardcoded, well-formed interface name")
        .member("Notify")
        .expect("\"Notify\" is a hardcoded, well-formed member name")
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

/// [`connect`], bounded by [`CONNECT_TIMEOUT`] (Important 1).
///
/// `connect` itself has no timeout on any of its steps -- the auth handshake
/// inside `Builder::build`, and `become_monitor`'s own round trip, both run
/// under zbus's default `method_timeout` of `None`. A bus that accepts the
/// connection and then stalls (a dying `dbus-daemon`, one wedged on fd
/// exhaustion) leaves `connect`'s future simply never resolving: no error to
/// classify, nothing to log, no backoff to fall back on -- narration is dead
/// for the life of the process. Wrapping the whole call, rather than trying
/// to set a per-method timeout inside `connect`, is what actually covers the
/// auth handshake, which happens before there is even a `Connection` to
/// configure a timeout on. An elapsed timeout is treated as
/// `Refusal::Transient`: a bus that is merely slow this time is exactly what
/// backoff-and-retry exists for.
async fn connect_with_timeout(address: Option<&str>) -> Result<MessageStream, Refusal> {
    match tokio::time::timeout(CONNECT_TIMEOUT, connect(address)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(Refusal::Transient(format!(
            "the notification monitor's connection attempt did not complete within {CONNECT_TIMEOUT:?}"
        ))),
    }
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

/// Read the engine's live config, or `None` if the engine could not answer.
///
/// The whole `Config`, not just `notifications`: `speak` needs `max_chars`
/// too (Important 3), and `EngineHandle::config` hands back one struct either
/// way, so there is no cheaper thing to ask for.
///
/// On the blocking pool: `EngineHandle::config` waits up to 250 ms on an
/// engine thread that may be mid-chunk, and blocking a runtime worker for
/// that is exactly what `dbus::SaydIface::submit`'s doc comment describes
/// going wrong.
async fn fetch_config(engine: &EngineHandle) -> Option<Config> {
    let engine = engine.clone();
    tokio::task::spawn_blocking(move || engine.config())
        .await
        .ok()
        .flatten()
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
///
/// `max_chars` is checked here, against the *live* config's own limit,
/// before the text is ever handed to `submit` (Important 3). `Engine::submit`
/// already rejects an over-long text on its own, but for a source that is
/// `Idle` or `Speaking`/`Paused`-adjacent it also *sets global `State::Error`*
/// with that rejection as the reason (`engine.rs:476-492`) -- and a
/// notification is exactly the case where that is wrong: an allowlisted
/// application sending one 60,000-character summary should lose that one
/// announcement, not light `dialog-error-symbolic` on the tray and report
/// `"error"` on D-Bus for a fault the user watching the tray never caused.
/// The right long-term fix is `Engine::submit` not setting *global* error
/// state for a `Source::Notification` rejection in the first place, which
/// would make every caller of `submit` safe by construction rather than
/// leaving each one to guard itself -- out of scope for this file, noted
/// here so it is not lost.
async fn speak(engine: &EngineHandle, text: String, max_chars: usize, failure_logged: &mut bool) {
    let len = text.chars().count();
    if len > max_chars {
        eprintln!(
            "warning: a notification's announcement is {len} characters, over the \
             {max_chars}-character limit; skipping it rather than submitting it"
        );
        return;
    }
    let e = engine.clone();
    let result = tokio::task::spawn_blocking(move || e.submit(text, notification_opts())).await;
    match result {
        // `Submitted::TimedOut` lands here too, deliberately: it means the
        // engine already queued or discarded the text before
        // `EngineHandle::submit`'s bounded wait gave up on the reply, not
        // that the submission failed -- see `Submitted::TimedOut`'s doc
        // comment. Clearing `failure_logged` on it is therefore right, the
        // same as any other success, rather than treating a merely slow
        // confirmation as a reason to start (or keep) logging failures.
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

/// §4's discovery-log bookkeeping: which application names have already been
/// announced this run, and whether the cap on that set has already been
/// announced too (Important 2).
///
/// A bare `HashSet<String>` used to be all this was. Measured: 2000 `Notify`
/// calls with 2000 distinct `app_name`s produced 2000 permanent entries and
/// 2000 `info:` lines in under a second, and nothing bounded either --
/// `app_name` is chosen by the calling application (`decode`'s doc comment),
/// so an unbounded set keyed on it is a leak any application on the bus can
/// trigger, compromised or not. `cap_logged` is its own field rather than a
/// sentinel value in `names`, so the one line announcing the cap is printed
/// exactly once regardless of how the cap was reached.
#[derive(Default)]
struct Announced {
    names: HashSet<String>,
    cap_logged: bool,
}

/// Truncate `s` to at most `max_chars` characters, on a `char` boundary.
///
/// Plain byte slicing can land inside a multi-byte UTF-8 sequence and panic;
/// `app_name` is attacker-controlled text off the bus (`decode`'s doc
/// comment), so this has to be correct for arbitrary input, not just ASCII.
fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &s[..byte_idx],
        None => s,
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
///
/// Bounded two ways (Important 2): `app_name` is truncated to
/// [`MAX_APP_NAME_LEN`] before it is hashed, stored or printed, so one
/// enormous name costs no more than a short one; and once [`MAX_ANNOUNCED`]
/// distinct names have been remembered, every further undeclared application
/// is silently dropped rather than growing the set or the log without bound
/// -- except for one final line marking that the cap was hit, so the
/// operator learns discovery stopped rather than wondering why an
/// application never got a line.
fn log_discovery(app_name: &str, announced: &mut Announced) {
    let bounded = truncate_chars(app_name, MAX_APP_NAME_LEN);
    let key = bounded.to_lowercase();
    if announced.names.contains(&key) {
        return;
    }
    if announced.names.len() >= MAX_ANNOUNCED {
        if !announced.cap_logged {
            eprintln!(
                "info: notification monitor discovery log capped at {MAX_ANNOUNCED} \
                 distinct application names for this run; further undeclared \
                 applications will not be logged"
            );
            announced.cap_logged = true;
        }
        return;
    }
    announced.names.insert(key);
    eprintln!(
        "info: notification from {bounded:?} \
         (not in notifications.allow; add it to speak these)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};

    use sayd_core::audio::VecSink;
    use sayd_core::config::NotificationConfig;
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
        let mut announced = Announced::default();
        announced.names.insert("signal".to_string());
        // Re-running the real function must not add a second entry for the
        // same name in a different case.
        log_discovery("Signal", &mut announced);
        log_discovery("SIGNAL", &mut announced);
        assert_eq!(announced.names.len(), 1);
        log_discovery("Fractal", &mut announced);
        assert_eq!(announced.names.len(), 2);
    }

    /// Important 2: an unbounded set keyed on attacker-controlled `app_name`
    /// is exactly the leak measured against a real bus (2000 distinct names,
    /// 2000 permanent entries, 2000 log lines in under a second). Past
    /// `MAX_ANNOUNCED` distinct names, further ones are dropped rather than
    /// remembered, and the cap is announced exactly once regardless of how
    /// many more names arrive after it.
    #[test]
    fn the_discovery_set_is_capped_and_says_so_once() {
        let mut announced = Announced::default();
        for i in 0..MAX_ANNOUNCED {
            log_discovery(&format!("app-{i}"), &mut announced);
        }
        assert_eq!(announced.names.len(), MAX_ANNOUNCED);
        assert!(!announced.cap_logged);

        // The name that tips the set over the cap is not remembered...
        log_discovery("one-too-many", &mut announced);
        assert_eq!(announced.names.len(), MAX_ANNOUNCED);
        assert!(announced.cap_logged);

        // ...and neither are any that follow, nor does the cap line repeat
        // (there is nothing here to assert that directly on `stderr`, but
        // `cap_logged` staying `true` without a second flip is the same
        // guarantee `log_discovery`'s `if !announced.cap_logged` relies on).
        log_discovery("another-one", &mut announced);
        assert_eq!(announced.names.len(), MAX_ANNOUNCED);
    }

    /// Important 2's other half: a single enormous `app_name` must cost no
    /// more to remember than a short one, and two names that agree up to
    /// `MAX_APP_NAME_LEN` characters are the same entry once truncated.
    #[test]
    fn an_overlong_app_name_is_truncated_before_it_is_remembered() {
        let mut announced = Announced::default();
        let long_name = "a".repeat(MAX_APP_NAME_LEN + 500);
        log_discovery(&long_name, &mut announced);
        assert_eq!(announced.names.len(), 1);
        assert_eq!(
            announced.names.iter().next().expect("one entry").len(),
            MAX_APP_NAME_LEN
        );

        // A second name that only differs after the truncation point is the
        // same entry, not a new one.
        let same_prefix_longer = "a".repeat(MAX_APP_NAME_LEN + 5000);
        log_discovery(&same_prefix_longer, &mut announced);
        assert_eq!(announced.names.len(), 1);
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

    /// Important 3: an over-long announcement from an allowlisted
    /// application must not drive the whole daemon into `State::Error`.
    /// `Engine::submit`'s own `max_chars` rejection does exactly that for an
    /// idle engine (`engine.rs:476-492`) -- see `speak`'s doc comment for
    /// why that is the wrong failure mode for a notification specifically.
    /// `speak` guards on length itself and must never even reach `submit`
    /// when the text is over the limit.
    #[tokio::test]
    async fn an_overlong_announcement_is_skipped_without_erroring_the_engine() {
        let (engine, spoken) = engine_allowing("Signal");
        let mut logged = false;
        speak(&engine, "a".repeat(30), 10, &mut logged).await;

        assert!(
            spoken.lock().expect("spoken mutex").is_empty(),
            "an over-length announcement must never reach synthesis"
        );
        assert_eq!(
            engine.snapshot().state,
            sayd_core::engine::State::Idle,
            "a notification's own length must not put the whole engine into Error"
        );
        engine.shutdown();
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
            Self::start_with_policy(true)
        }

        /// A bus that denies `BecomeMonitor` outright, for Important 4's
        /// permanent-exit test.
        ///
        /// The only difference from `start`'s config is the trailing `<deny
        /// send_interface="org.freedesktop.DBus.Monitoring"/>` line: with it
        /// present, the bus driver answers `BecomeMonitor` with a real
        /// `org.freedesktop.DBus.Error.AccessDenied`, the same as an
        /// administrator's deny-policy would -- rule order matters here,
        /// `<deny>` has to come after the `<allow send_destination="*"/>` it
        /// narrows, or it has nothing to override. Everything else about the
        /// bus keeps working: ordinary method calls (the stub daemon owning
        /// its name, the application calling `Notify`) are unaffected, or a
        /// test using `start_denying_monitor` could not tell "the bus is
        /// broken" apart from "the bus specifically refused monitoring".
        /// Measured empirically against this `dbus-daemon` (5.19's vendored
        /// zbus, this crate's real `MonitoringProxy`): `eavesdrop="true"`
        /// governs *receiving* other connections' ordinary traffic, not the
        /// `BecomeMonitor` call itself, so denying it does nothing here --
        /// `send_interface` is what the bus actually checks.
        fn start_denying_monitor() -> Option<TestBus> {
            Self::start_with_policy(false)
        }

        fn start_with_policy(allow_monitor: bool) -> Option<TestBus> {
            let dir = tempfile::tempdir().expect("a temp dir for the bus config");
            let config = dir.path().join("bus.conf");
            // `unix:tmpdir=/tmp` keeps the socket off the session's real
            // one. `receive_sender="*"` is not redundant with
            // `send_destination="*"`: without it a connection cannot even
            // finish the initial `Hello` handshake on this dbus-daemon
            // (measured -- `Builder::build` simply never resolves), because
            // nothing has granted it permission to receive the reply.
            let deny_monitoring = if allow_monitor {
                ""
            } else {
                "<deny send_interface=\"org.freedesktop.DBus.Monitoring\"/>\n"
            };
            std::fs::write(
                &config,
                format!(
                    "<busconfig>\n\
                     <type>session</type>\n\
                     <listen>unix:tmpdir=/tmp</listen>\n\
                     <policy context=\"default\">\n\
                     <allow send_destination=\"*\"/>\n\
                     <allow receive_sender=\"*\"/>\n\
                     <allow own=\"*\"/>\n\
                     {deny_monitoring}\
                     </policy>\n\
                     </busconfig>\n"
                ),
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

            // Minor 7: `read_line` has no deadline of its own, unlike every
            // other wait in this test file. A `dbus-daemon` that starts but
            // never prints an address (a broken build, a config it silently
            // refuses) would otherwise hang the whole suite forever instead
            // of failing it. The blocking read runs on its own thread so a
            // 10s `recv_timeout` can bound it from outside; on timeout the
            // child is killed before panicking, so it does not survive the
            // failing test.
            let stdout = child.stdout.take().expect("piped stdout");
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut address = String::new();
                let result = BufReader::new(stdout)
                    .read_line(&mut address)
                    .map(|_| address);
                let _ = tx.send(result);
            });
            let address = match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(Ok(address)) => address,
                Ok(Err(e)) => panic!("dbus-daemon prints its address: {e}"),
                Err(_timeout) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("dbus-daemon did not print an address within 10s");
                }
            };
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
            // Minor 10: `unix:tmpdir=/tmp` makes dbus-daemon create its
            // listening socket as a real file directly under `/tmp`, outside
            // `_dir` (which only ever held the config file) and outside
            // anything `kill` cleans up on its own -- thirty of these were
            // left behind over one review's worth of runs. Best-effort: an
            // address with no `path=` (an abstract socket) or a socket
            // dbus-daemon already unlinked itself is not an error here, and
            // nothing about a test's outcome should hinge on deleting a
            // temp file succeeding.
            if let Some(rest) = self.address.split_once("path=").map(|(_, r)| r) {
                let path = rest.split(',').next().unwrap_or(rest);
                let _ = std::fs::remove_file(path);
            }
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

    /// A spawned engine allowing `allow`, with `cooldown_secs` as given, that
    /// records every text handed to synthesis rather than actually speaking
    /// it.
    fn engine_with(
        allow: Vec<String>,
        cooldown_secs: u64,
    ) -> (EngineHandle, Arc<Mutex<Vec<String>>>) {
        let cfg = Config {
            notifications: NotificationConfig {
                enabled: true,
                allow,
                cooldown_secs,
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

    fn engine_allowing(app: &str) -> (EngineHandle, Arc<Mutex<Vec<String>>>) {
        engine_with(
            vec![app.to_string()],
            NotificationConfig::default().cooldown_secs,
        )
    }

    fn engine_has(spoken: &Arc<Mutex<Vec<String>>>, text: &str) -> bool {
        spoken
            .lock()
            .expect("spoken mutex")
            .iter()
            .any(|t| t == text)
    }

    /// Call `Notify` on `app` with the eight-field signature spec §2
    /// requires, using an empty body and the defaults every test in this
    /// file is indifferent to (icon, actions, hints, a 5s expiry). Only
    /// `app_name` and `summary` vary from one call to the next here.
    async fn call_notify(app: &zbus::Connection, app_name: &str, summary: &str) {
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

    /// A stub notification daemon *and* an application connection on a fresh
    /// `TestBus`, the pair every bus-backed test in this file needs before it
    /// can do anything else. `None` when `dbus-daemon` is not on `PATH`.
    ///
    /// The daemon connection is returned, not dropped here: zbus serves an
    /// interface only as long as the `Connection` that registered it is
    /// alive, so a caller that let this local go out of scope would silently
    /// stop routing `Notify` at all.
    async fn stub_daemon_and_app() -> Option<(TestBus, zbus::Connection, zbus::Connection)> {
        let bus = TestBus::start()?;
        let daemon = zbus::connection::Builder::address(bus.address.as_str())
            .expect("address")
            .name("org.freedesktop.Notifications")
            .expect("name")
            .serve_at("/org/freedesktop/Notifications", StubNotificationDaemon)
            .expect("serve")
            .build()
            .await
            .expect("stub notification daemon");
        let app = zbus::connection::Builder::address(bus.address.as_str())
            .expect("address")
            .build()
            .await
            .expect("application connection");
        Some((bus, daemon, app))
    }

    /// The whole path, against a real bus: an application calls `Notify`, a
    /// stub daemon owning the name receives it, and the monitor turns it into
    /// a submission. Gated on `dbus-daemon` being present so a machine
    /// without one skips rather than fails.
    #[tokio::test]
    async fn a_notification_on_a_real_bus_reaches_the_engine() {
        let Some((bus, _daemon, app)) = stub_daemon_and_app().await else {
            eprintln!("skipping: dbus-daemon not on PATH");
            return;
        };

        let (engine, spoken) = engine_allowing("Signal");
        let monitor = tokio::spawn(run_on(engine.clone(), Some(bus.address.clone())));

        // `run_on` becomes a monitor asynchronously, so a single `Notify`
        // sent immediately could legitimately predate the match rule. Retry
        // until the engine has it or the deadline passes; the rate limiter
        // counts the extras against one window rather than speaking them, so
        // repeating is harmless.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut arrived = false;
        while Instant::now() < deadline {
            call_notify(&app, "Signal", "hello").await;
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
        let Some((bus, _daemon, app)) = stub_daemon_and_app().await else {
            eprintln!("skipping: dbus-daemon not on PATH");
            return;
        };

        // Allows "Signal"; "Fractal" below is not it.
        let (engine, spoken) = engine_allowing("Signal");
        let monitor = tokio::spawn(run_on(engine.clone(), Some(bus.address.clone())));

        // Send a control notification from an allowed application after the
        // unlisted one, and wait for *that*: once the allowed one has been
        // spoken, the monitor has demonstrably been running and processing,
        // so the unlisted one's absence means it was declined rather than
        // merely not arrived yet.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut control_arrived = false;
        while Instant::now() < deadline {
            call_notify(&app, "Fractal", "unlisted").await;
            call_notify(&app, "Signal", "listed").await;
            if engine_has(&spoken, "Signal: listed") {
                control_arrived = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            control_arrived,
            "the monitor never spoke the allowed control notification, so this \
             test proves nothing about the unlisted one"
        );

        // Minor 8: the retry loop above breaks the instant the control
        // notification is seen -- but if `become_monitor` took effect
        // between *that round's* `Fractal` and `Signal` calls, that round's
        // `Fractal` predates the match rule and was never delivered at all,
        // and the assertion below would pass whether or not the allowlist
        // was actually being checked. `control_arrived` proves the monitor
        // is now unambiguously up and processing, so send one more `Fractal`
        // call that is guaranteed to land after activation, and assert on
        // *that* one specifically rather than the possibly-never-delivered
        // one from the race above.
        call_notify(&app, "Fractal", "definitely after activation").await;

        // `Policy::Front` puts each accepted notification *ahead* of what is
        // already queued, so an unlisted one wrongly accepted would be
        // synthesized after this call returns, not necessarily before it.
        // Give the engine a moment to have drained it either way.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let spoke_unlisted = engine_has(&spoken, "Fractal: unlisted")
            || engine_has(&spoken, "Fractal: definitely after activation");
        monitor.abort();
        engine.shutdown();
        assert!(
            !spoke_unlisted,
            "an application absent from notifications.allow must not be spoken"
        );
    }

    /// Important 4: the one-second tick's whole reason for existing is the
    /// coalesced follow-up, and nothing before this pinned it end to end.
    /// `cooldown_secs = 2` so the test does not need to wait out the 30s
    /// default: one `Notify` speaks immediately, three more inside the
    /// window are counted, and "Signal: 3 more notifications" must arrive on
    /// its own once the window closes -- there is no fourth notification to
    /// carry it out any other way.
    #[tokio::test]
    async fn the_tick_speaks_a_coalesced_followup_on_its_own() {
        let Some((bus, _daemon, app)) = stub_daemon_and_app().await else {
            eprintln!("skipping: dbus-daemon not on PATH");
            return;
        };

        let (engine, spoken) = engine_with(vec!["Signal".to_string(), "Control".to_string()], 2);
        let monitor = tokio::spawn(run_on(engine.clone(), Some(bus.address.clone())));

        // Establish the monitor is active with a throwaway app, so the burst
        // below is not itself spent proving that -- see Minor 8's reasoning
        // for why retrying the thing under test can make its own assertion
        // vacuous.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut active = false;
        while Instant::now() < deadline {
            call_notify(&app, "Control", "ping").await;
            if engine_has(&spoken, "Control: ping") {
                active = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(active, "the monitor never became active");

        // "Signal"'s window has never been touched, so this speaks
        // immediately and opens it.
        call_notify(&app, "Signal", "first").await;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut spoke_first = false;
        while Instant::now() < deadline {
            if engine_has(&spoken, "Signal: first") {
                spoke_first = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(spoke_first, "the first Signal notification never spoke");

        // Three more inside the 2s window: counted, not spoken.
        for _ in 0..3 {
            call_notify(&app, "Signal", "more").await;
        }

        // Nothing sends a fifth `Notify`, so the only way "3 more
        // notifications" can ever be spoken is the ticker driving
        // `Limiter::due` on its own -- exactly the wiring Important 4 says
        // was untested.
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut spoke_followup = false;
        while Instant::now() < deadline {
            if engine_has(&spoken, "Signal: 3 more notifications") {
                spoke_followup = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        monitor.abort();
        engine.shutdown();
        assert!(
            spoke_followup,
            "the coalesced follow-up never arrived; the ticker's `Limiter::due` wiring is not working"
        );
    }

    /// Important 4: a bus that refuses `BecomeMonitor` must make `run_on`
    /// actually *return* -- not merely make `classify_monitor_failure` say
    /// `Permanent`, which `only_a_refusal_is_permanent` above already pins
    /// on its own and which this test does not re-test. Against a real
    /// deny-policy bus (`TestBus::start_denying_monitor`), the whole task
    /// spawned from `run_on` must resolve within a bounded time, proving the
    /// `Err(Refusal::Permanent(_)) => { ...; return; }` arm in the outer loop
    /// is actually reached and actually returns, rather than looping,
    /// panicking, or hanging.
    ///
    /// IMPORTANT 3 (M5 final review): it must also return `Outcome::Refused`
    /// specifically, not merely return. That value is the only thing telling
    /// the supervisor apart "the bus said no, permanently" from "the task
    /// died" -- and it is what makes the supervisor latch instead of
    /// re-asking every 5s forever (measured before the latch: 37 log lines
    /// and 18 connect/auth/`BecomeMonitor` cycles in 90s against this exact
    /// bus configuration).
    #[tokio::test]
    async fn a_refused_bus_makes_run_on_return() {
        let Some(bus) = TestBus::start_denying_monitor() else {
            eprintln!("skipping: dbus-daemon not on PATH");
            return;
        };

        let (engine, _spoken) = engine_allowing("Signal");
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_on(engine.clone(), Some(bus.address.clone())),
        )
        .await;
        engine.shutdown();
        assert_eq!(
            result,
            Ok(Outcome::Refused),
            "run_on must return Outcome::Refused within 10s against a bus that \
             denies BecomeMonitor -- returning at all is not enough, the \
             supervisor cannot latch on a value it is not given"
        );
    }

    /// MINOR 3: the monitor reads every other field of `cfg.notifications`
    /// and used to ignore `enabled`, so it narrated for as long as it took
    /// the supervisor's `abort` to land -- normally sub-second, unbounded
    /// while the publish loop's own read of the config store is stalled
    /// (CRITICAL 1's precondition). Here the supervisor is deliberately not
    /// in the picture at all: nothing aborts this task, and the *only* thing
    /// that can stop it speaking is its own check of `enabled` against the
    /// config it re-reads on its tick.
    ///
    /// `cooldown_secs = 0` so the rate limiter cannot be what makes the
    /// second notification silent -- with the default 30s window it would be
    /// counted rather than spoken whether or not this was fixed, and the
    /// test would pass on the broken code too.
    #[tokio::test]
    async fn a_monitor_stops_speaking_when_the_config_disables_notifications() {
        let Some((bus, _daemon, app)) = stub_daemon_and_app().await else {
            eprintln!("skipping: dbus-daemon not on PATH");
            return;
        };

        let (engine, spoken) = engine_with(vec!["Signal".to_string()], 0);
        let monitor = tokio::spawn(run_on(engine.clone(), Some(bus.address.clone())));

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut active = false;
        while Instant::now() < deadline {
            call_notify(&app, "Signal", "while enabled").await;
            if engine_has(&spoken, "Signal: while enabled") {
                active = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(active, "the monitor never became active");

        // Exactly what `ConfigStore::write_locked` does when the settings
        // window's Notifications switch is turned off: the engine is the
        // monitor's source of truth for config (see `fetch_config`).
        let mut cfg = engine.config().expect("the engine answers");
        cfg.notifications.enabled = false;
        engine.send(sayd_core::engine::Command::ApplyConfig(cfg));

        // The monitor re-reads its cached config on `DUE_INTERVAL`; give it
        // a few of those rather than racing the one that happens to be next.
        tokio::time::sleep(DUE_INTERVAL * 3).await;
        call_notify(&app, "Signal", "after disabling").await;
        tokio::time::sleep(Duration::from_millis(500)).await;

        let spoke_after = engine_has(&spoken, "Signal: after disabling");
        monitor.abort();
        engine.shutdown();
        assert!(
            !spoke_after,
            "a notification arriving after notifications.enabled went false \
             was still narrated; the monitor fails open on the one field that \
             says whether to speak at all"
        );
    }

    /// Important 4: the reconnect path. The monitor's connection is severed
    /// out from under it by killing the `dbus-daemon` it is attached to and
    /// starting a fresh one on a new socket -- `run_on` cannot be told the
    /// new address (the interface is fixed, see the module doc), so this
    /// only demonstrates the *detection and backoff* half of reconnect, not
    /// resuming narration on a literal address change. That is enough to
    /// pin the actual bug class this row exists for: `next_message`
    /// returning `None`/`Err` must `break` the inner loop and go back around
    /// the outer one rather than ending the task or panicking.
    #[tokio::test]
    async fn a_dropped_connection_is_detected_and_the_outer_loop_survives_it() {
        let Some((bus, daemon, app)) = stub_daemon_and_app().await else {
            eprintln!("skipping: dbus-daemon not on PATH");
            return;
        };

        let (engine, spoken) = engine_allowing("Signal");
        let monitor = tokio::spawn(run_on(engine.clone(), Some(bus.address.clone())));

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut arrived = false;
        while Instant::now() < deadline {
            call_notify(&app, "Signal", "before").await;
            if engine_has(&spoken, "Signal: before") {
                arrived = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(arrived, "the monitor never became active before the drop");

        // Sever the connection out from under the monitor: drop the
        // application and daemon connections and kill the bus itself. The
        // monitor's own connection dies the same way a real dbus-daemon
        // restart would kill it -- the socket closes -- which is what
        // exercises `Some(Err(_))`/`None` off `next_message` rather than a
        // clean shutdown this daemon chose itself.
        drop(app);
        drop(daemon);
        drop(bus);

        // The monitor task must still be alive and not have panicked --
        // `run_on` only returns on `Refusal::Permanent`, which a dropped
        // connection is not (`classify_monitor_failure`'s `other` arm).
        // Give it well past `INITIAL_RECONNECT_BACKOFF` to have noticed the
        // drop, gone around the outer loop, and be sitting in its own
        // `connect` retrying against a socket that no longer exists.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            !monitor.is_finished(),
            "run_on returned after a dropped connection instead of reconnecting with backoff"
        );

        monitor.abort();
        engine.shutdown();
    }
}
