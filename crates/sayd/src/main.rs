//! The sayd daemon.
//!
//! Runs resident, owning the engine on its own thread and serving
//! `sh.sayd.Sayd1` on the session bus. A second instance detects that the
//! well-known name is taken, forwards its arguments to the running daemon,
//! and exits -- so `sayd` is safe to put in a sway config that gets reloaded.

mod config_watch;
mod dbus;
mod kokoro_synth;
mod mpris;
mod notify;
mod pipeline;
mod resample;
mod reword;
mod ring;
mod selection;
mod settings;
mod tray;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use sayd_core::audio::AudioSink;
use sayd_core::config::Config;
use sayd_core::handle::EngineHandle;
use sayd_core::synth::Synthesizer;
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::zvariant::OwnedValue;

const BUS_NAME: &str = "sh.sayd.Sayd";
const OBJECT_PATH: &str = "/sh/sayd/Sayd";

/// How often the daemon publishes property changes.
///
/// Fast enough that a tray or MPRIS client feels live, slow enough that
/// `RemainingSeconds` ticking down does not flood the bus.
const PUBLISH_INTERVAL: Duration = Duration::from_millis(200);

/// How often `RemainingSeconds` republishes while it is actively counting
/// down.
///
/// `RemainingSeconds` is a `#[zbus(property)]`, which zbus's introspection
/// XML annotates `emits-change` by default; a spec-conformant client (e.g.
/// waybar's D-Bus module) is entitled to cache a property's value forever
/// once read, until a change signal says otherwise. The publish loop used
/// to never send one for this property at all -- deliberately, per the
/// comment that used to sit above the emission list -- which meant such a
/// client saw the value it first read and nothing else, forever.
///
/// The design doc's target for a live countdown is 1 Hz: fast enough to
/// read as ticking on a tray or waybar module, slow enough not to flood the
/// bus the way publishing every `PUBLISH_INTERVAL` (200 ms) would -- five
/// times the traffic for a number nobody can read that precisely anyway.
const REMAINING_SECONDS_PUBLISH_INTERVAL: Duration = Duration::from_secs(1);

/// Minimum spacing between device-reacquisition attempts while the engine is
/// stuck in `State::Error`.
///
/// The publish loop ticks every `PUBLISH_INTERVAL` (200 ms). Retrying
/// `open_sink` -- which opens a real audio stream -- that often while the
/// device stays gone would spin hot and, without the log-once throttling
/// next to the retry below, flood stderr five times a second for as long as
/// the outage lasts. A couple of seconds is generous for how quickly
/// PulseAudio/PipeWire or a device typically comes back, while still
/// noticing recovery promptly once it does.
const RECOVERY_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// How long the publish loop waits for a single tray or MPRIS fan-out call
/// (`ksni::Handle::update`, `mpris_server::Server::properties_changed`)
/// before giving up on it for this tick.
///
/// C1: `ksni`'s background service task processes tray updates and its own
/// `RegisterStatusNotifierItem` re-registration (fired on every
/// `NameOwnerChanged` for `org.kde.StatusNotifierWatcher` -- in practice,
/// every waybar restart or config reload) on the same task, behind the same
/// internal mutex, held across whichever `.await` is in flight at the time.
/// A watcher that is slow -- or never -- to answer that re-registration call
/// holds the mutex for the same duration, and `Handle::update`'s own
/// internal wait for that mutex, previously awaited here with no bound at
/// all, blocked this entire `tokio::select!` loop for exactly as long:
/// measured 29.49s of zero `PropertiesChanged` on *both* the control
/// interface and MPRIS against a watcher that stalled 30s, and SIGTERM
/// ignored for 60.6s against one that never answered -- `select!` cannot
/// preempt a branch once chosen, so the shutdown arms never got a turn
/// either. Wrapping the await in a timeout is what lets this branch finish
/// -- unsuccessfully, but promptly -- and hand control back to `select!` for
/// the next tick, the signal handlers, and device recovery, none of which
/// are otherwise related to a stuck tray host but were blocked alongside it.
///
/// The alternative this project considered was moving the fan-out off the
/// loop's critical path entirely (e.g. a dedicated task per consumer, fed by
/// a channel). A bounded, in-line timeout was chosen instead: it is a
/// smaller, easier-to-verify change to a loop that already reasons carefully
/// about ordering (see the comments throughout `main`'s publish loop), and
/// it directly targets the actual failure mode measured -- an unbounded
/// wait -- without introducing a second task whose own lifecycle (startup,
/// shutdown ordering, backpressure if it falls behind) would need the same
/// scrutiny this file already gives everything else. A generous local
/// session-bus round trip is comfortably under 50ms, so a healthy host is
/// never affected by this bound.
const FANOUT_TIMEOUT: Duration = Duration::from_millis(500);

/// Once a fan-out call to the tray or MPRIS times out, how long to stop
/// retrying that consumer before trying again.
///
/// Without this, a permanently stuck host would pay up to `FANOUT_TIMEOUT`
/// on nearly every `PUBLISH_INTERVAL` tick forever -- 500ms owed out of
/// every 200ms -- which is a milder version of the exact problem this fix
/// exists for: the loop would spend most of its time blocked on a consumer
/// that is not coming back, rather than the brief, occasional stall this
/// backoff leaves in its place. Modeled on `RECOVERY_RETRY_INTERVAL`'s
/// reasoning for the same shape of problem on the audio-device side: a
/// couple of seconds is generous for a host to recover, longer here because
/// unlike a dead audio device (which blocks all output), a stalled tray or
/// MPRIS host only delays a diagnostic display, not audio.
const FANOUT_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// How long a forwarding call to an already-running daemon may block before
/// this instance gives up and reports a timeout instead of hanging.
///
/// zbus applies no timeout of its own here: `Connection::call_method` only
/// wraps the reply in a `tokio::time::timeout` when the connection was built
/// with `Builder::method_timeout`, and this one was not -- `Builder::new`
/// leaves `method_timeout: None` (checked against zbus 5.19's source).
/// Without this constant, a forwarding call to a wedged daemon would hang
/// indefinitely, not merely for "close to 25 seconds" as an earlier version
/// of this comment claimed. A couple of seconds is generous for a local
/// session-bus round trip.
///
/// On a zbus upgrade: if a future version starts giving `Builder::new` a
/// non-`None` default `method_timeout`, that default would apply
/// underneath this one silently -- harmless as long as it stays above 3 s,
/// but worth checking `method_timeout` in that release's `Builder::new` and
/// how `Connection::call_method` uses it before trusting this comment again.
const FORWARD_CALL_TIMEOUT: Duration = Duration::from_secs(3);

/// How long the publish loop waits for [`NotifyEnabledWatch`]'s
/// `spawn_blocking` hop -- reading `ConfigStore::current()` -- before giving
/// up on it for this tick.
///
/// CRITICAL 1: `current()` takes `config_watch::ConfigStore`'s
/// `last_written` lock, the same `std::sync::Mutex` `write_locked` holds
/// across `Config::save_to`'s synchronous, unbounded disk write (its own
/// doc comment: "unbounded on a hung NFS or FUSE home") -- and every
/// settings-window save, tray mute and MPRIS rate change reaches
/// `write_locked`. Calling `current()` directly in this arm -- no
/// `.await`, no `spawn_blocking` -- used to mean a write stuck on such a
/// filesystem blocked this entire `select!` loop, `sigterm.recv()`
/// included: measured, with a `set_muted` parked on a FIFO nobody reads (the
/// same technique `config_watch.rs`'s `a_mute_takes_effect_even_while_the_
/// write_is_stuck` uses), a concurrent `current()` did not return within
/// 500ms. That is the exact failure class `FANOUT_TIMEOUT`'s doc comment
/// above records at 60.6s of ignored SIGTERM -- an unbounded wait inside
/// this one `tokio::select!`, which cannot preempt a branch once chosen, so
/// the shutdown arms get no turn until the arm currently running gives
/// control back.
///
/// The fix moves the read onto `spawn_blocking` (as
/// `config_watch::persist_in_background` already does for this same
/// struct) and bounds the `.await` on its `JoinHandle` with this timeout --
/// the `.await` is what this loop can give up on and still hand control
/// back to `select!`; the blocking-pool thread itself stays parked on the
/// mutex for as long as the write takes, same as before, it just no longer
/// holds this loop hostage while it waits. 250ms matches `sayd-core`'s
/// `CONFIG_REPLY_TIMEOUT`, the bound on `EngineHandle::config()` -- the
/// alternative this call was chosen over specifically for being cheaper --
/// so this path is now no *less* responsive than the one it beat.
const CONFIG_STAMP_READ_TIMEOUT: Duration = Duration::from_millis(250);

/// How long [`main`] lets the runtime finish what is on its blocking pool
/// before it stops waiting and lets the process exit.
///
/// `Runtime::drop` waits for every blocking task that has *started*, with no
/// bound of its own -- the failure `NotifyEnabledWatch`'s CRITICAL 1 already
/// records ("SIGTERM left the process sitting for a minute"). This milestone
/// is what makes it a routine cost rather than a pathological one: the
/// rewrite's `ureq` call runs on that pool and is bounded only by
/// `reword::http_ceiling`, which is `reword.timeout_ms` plus ten seconds of
/// grace -- a number the *user* now sets, with no ceiling over it. Measured
/// at the 1.5 s default, one detached rewrite in flight against a provider
/// that accepts and never answers: `Runtime::drop` took 9.73 s, so a
/// `systemctl --user restart sayd` in the middle of one sat for ten seconds.
/// A minute-long deadline would have made that a minute.
///
/// The value below is deliberately **not** derived from that ceiling, and
/// removing the ceiling on `timeout_ms` did not change it. Waiting for a
/// rewrite at shutdown buys nothing at any deadline -- see the last
/// paragraph -- so this is bounded by what a *bounded* task needs, and a
/// longer deadline only widens the gap between the two.
///
/// 500 ms because that is twice the longest *bounded* thing on this pool --
/// `sayd_core::handle`'s 250 ms `SUBMIT_REPLY_TIMEOUT` and
/// `CONFIG_REPLY_TIMEOUT`, and [`CONFIG_STAMP_READ_TIMEOUT`] above -- so
/// every task that can finish promptly still does, and nothing is abandoned
/// that was about to succeed. It is also well under the 2 s
/// `settings::FLUSH_TIMEOUT` already spent by this point, so it does not
/// become the shutdown budget; the whole of SIGTERM-to-exit stays inside
/// systemd's `TimeoutStopSec` by two orders of magnitude rather than by a
/// factor of nine.
///
/// What waiting longer would buy is nothing: the two tasks that can outlast
/// this are a config-stamp read (whose value the publish loop has already
/// given up on) and a rewrite whose answer §2 has already dropped --
/// `RewordPlan::resolve` holds no `EngineHandle`, so there is no path on
/// which the thing being waited for could still be spoken.
const RUNTIME_SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// `AudioSink` for `SAYD_NO_AUDIO`: accepts and immediately discards every
/// sample, reporting nothing ever pending.
///
/// This is deliberately not `sayd_core::audio::VecSink`: `VecSink` models a
/// *bounded* buffer whose `pending()` only drops when a test calls `drain`
/// to simulate playback -- fine for engine unit tests that drive it by
/// hand, but fatal here, since nothing in the daemon ever calls `drain`.
/// With `VecSink`, `Engine::go_idle` (`sayd-core/src/engine.rs`) would see
/// `pending() > 0` forever after the first utterance filled its buffer and
/// the daemon would report `State: speaking` for good. `DiscardSink`
/// reports zero pending on every call, so the engine's normal
/// speaking-until-drained-then-idle transition still runs, just against no
/// real audio.
struct DiscardSink {
    paused: bool,
}

impl AudioSink for DiscardSink {
    fn push(&mut self, samples: &[f32]) -> usize {
        samples.len()
    }

    fn pending(&self) -> usize {
        0
    }

    fn clear(&mut self) {}

    fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    fn is_paused(&self) -> bool {
        self.paused
    }

    fn capacity(&self) -> usize {
        // No real buffer to size, but "as much as fits" is unbounded here,
        // and `Engine` computes look-ahead headroom from this. A large,
        // fixed value keeps that arithmetic sane without pretending to model
        // a real device's buffer.
        sayd_core::synth::SAMPLE_RATE as usize * 60
    }

    fn total_written(&self) -> usize {
        0
    }
}

/// How long `NotifyMonitorSupervisor::reconcile` waits after noticing the
/// monitor task ended on its own before it will spawn a fresh one.
///
/// IMPORTANT 2: `notify::monitor::run` returns on its own only when the bus
/// permanently refuses `become_monitor` -- a policy decision that will not
/// change between one 200ms publish tick and the next (see `run`'s doc
/// comment). Restarting on every tick with no backoff at all would repeat
/// that same refused call five times a second, forever, for a verdict `run`
/// already logged once. This is not as short as `RECOVERY_RETRY_INTERVAL`
/// (2s -- audio devices routinely recover within seconds, so retrying
/// promptly is worth it) because a bus policy refusal has no comparable
/// "it'll probably clear itself" case; it is not permanent-and-silent
/// either, so that *if* `run` ever ends for some other, genuinely transient
/// reason, the daemon notices and retries within a bounded time rather than
/// needing `enabled` toggled off and on by hand. Same duration and same
/// reasoning as `FANOUT_RETRY_INTERVAL`, which backs off a different stuck
/// consumer for the same "bounded noise, not a hot loop" reason.
const NOTIFY_RESTART_BACKOFF: Duration = Duration::from_secs(5);

/// Starts and stops the notification monitor (`notify::monitor::run`) to
/// track `notifications.enabled` -- the one thing under `[notifications]`
/// that actually needs a restart. `run` itself re-reads `allow`,
/// `cooldown_secs`, `speak_app_name` and `speak_body` off its own
/// one-second tick (see its doc comment), so this supervisor's whole job is
/// the on/off switch -- plus, since IMPORTANT 2, noticing when the task it
/// started has ended without being asked to.
///
/// A small struct beside the publish loop, not two more locals folded into
/// it (a `JoinHandle` and a "was it enabled last tick" bool): `run_daemon`
/// already carries a dozen pieces of tick-to-tick state for the tray/MPRIS
/// fan-out and device recovery, all in loose local variables, and adding a
/// thirteenth invisible one to that list buys nothing a named type does not
/// already buy for free -- most of all, that the two tests §8 requires can
/// drive `reconcile` directly instead of standing up the whole publish loop
/// (a real bus connection, a real engine, a tray) just to prove a task did
/// or did not get spawned.
struct NotifyMonitorSupervisor {
    /// `Some` exactly while the monitor task is (or was, until the next
    /// `reconcile` notices it has ended -- deliberately stopped or not)
    /// running. Doubles as "what was enabled last tick" -- `reconcile`
    /// needs no separate bool for that, since the two facts are the same
    /// fact.
    handle: Option<tokio::task::JoinHandle<()>>,
    /// Earliest time `reconcile` may spawn a fresh task after noticing the
    /// previous one ended on its own. See [`NOTIFY_RESTART_BACKOFF`].
    /// `Instant::now()` at construction and after every deliberate stop, so
    /// neither the first start nor a normal re-enable after `enabled` went
    /// false is ever delayed by this -- it only ever pushes forward when
    /// `reconcile` finds a *finished* handle, never when it finds `None`.
    next_restart_attempt: Instant,
    /// Set by the spawned task itself when `notify::monitor::run` handed back
    /// [`notify::monitor::Outcome::Refused`], and read by `reconcile` when it
    /// notices that task has ended.
    ///
    /// A shared flag rather than the task's own return value because
    /// `reconcile` is deliberately synchronous -- `a_disabled_monitor_is_
    /// never_started` is a plain `#[test]` precisely so that a `tokio::spawn`
    /// on the `enabled = false` path would panic rather than pass quietly --
    /// and reading a `JoinHandle`'s output needs an `.await`. Reset
    /// immediately before every spawn, so it always describes the task
    /// currently held.
    refused: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Latched once a refusal has been observed: no further spawns at all
    /// until `enabled` goes false and back. See `reconcile`.
    refusal_latched: bool,
}

impl NotifyMonitorSupervisor {
    fn new() -> Self {
        Self {
            handle: None,
            next_restart_attempt: Instant::now(),
            refused: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            refusal_latched: false,
        }
    }

    /// Spawn the monitor, wiring `run`'s outcome back to [`Self::refused`].
    ///
    /// The wrapper task is what turns a value `run` returns into something
    /// the synchronous `reconcile` can read; it does nothing else, and
    /// aborting it aborts `run` with it (the inner future is being polled
    /// inside this task, so cancelling the task drops it).
    fn spawn(&mut self, engine: &EngineHandle) {
        self.refused
            .store(false, std::sync::atomic::Ordering::Release);
        let flag = self.refused.clone();
        let engine = engine.clone();
        self.handle = Some(tokio::spawn(async move {
            if notify::monitor::run(engine).await == notify::monitor::Outcome::Refused {
                flag.store(true, std::sync::atomic::Ordering::Release);
            }
        }));
    }

    /// Start or stop the monitor task to match `enabled`. Called every
    /// publish tick with the config's current value (see `run_daemon`), so
    /// this has to be a no-op when nothing changed -- calling it with the
    /// same `enabled` a hundred times running must neither spawn a second
    /// task nor abort a task still meant to be running.
    ///
    /// Stopping is a plain `abort`, not a shutdown signal down some channel:
    /// `notify::monitor::run` holds no state that needs flushing and no
    /// resource that needs releasing in order (see its doc comment), so
    /// aborting between messages loses nothing that the process exiting
    /// would not already lose. The one gap that leaves, and it is a real
    /// one: `abort` cannot cancel an in-flight `spawn_blocking` -- the
    /// monitor's `speak`, mid-submission -- so a single announcement can
    /// still land audibly shortly after `enabled` goes false. Accepted
    /// rather than engineered around; there is no cheap way to make a
    /// blocking-pool call cancellable, and the window is one utterance, not
    /// a standing leak.
    ///
    /// IMPORTANT 2: an ended task is checked first, before the
    /// `(enabled, handle)` match below, so both of that match's arms see an
    /// accurate `self.handle`. Without this, `(true, Some(_))` read "already
    /// running" unconditionally -- true the instant a task is spawned, but
    /// never re-checked after -- so a task that ended on its own left
    /// `self.handle` pointing at a dead task forever, and narration stayed
    /// dead until something toggled `enabled` off and on by hand.
    ///
    /// IMPORTANT 3: *why* it ended decides what happens next, and the two
    /// answers are opposites. `notify::monitor::run` returns `Refused` only
    /// for a bus policy that forbids `BecomeMonitor` -- §2's failure table:
    /// "log once with the reason, run without narration" -- a verdict that
    /// cannot change while the daemon runs. Restarting into it on a timer
    /// asks the same question forever: measured against a bus denying
    /// `org.freedesktop.DBus.Monitoring`, a 5s backoff produced 37 log lines
    /// in 90 seconds, alternating the restart warning and the full
    /// `AccessDenied` reason, each from a fresh connection and auth
    /// handshake with a new unique bus name -- about 35,000 journal lines and
    /// 17,000 refused calls a day, for a `log once` row in the spec. So a
    /// refusal latches: no further spawn until `enabled` goes false and back,
    /// which is the one event that can plausibly accompany a changed bus
    /// policy (and is what a user who has fixed the policy will do anyway).
    /// Anything *else* ending the task keeps IMPORTANT 2's behaviour --
    /// noticed, logged, restarted after `NOTIFY_RESTART_BACKOFF`.
    fn reconcile(&mut self, enabled: bool, engine: &EngineHandle) {
        // Checked before anything else: this is the one event that clears a
        // latched refusal, and it has to clear it whatever state the handle
        // is in -- a latched supervisor holds no handle at all, so the
        // `(false, Some(_))` arm below never runs for it.
        if !enabled {
            self.refusal_latched = false;
        }

        if self.handle.as_ref().is_some_and(|h| h.is_finished()) {
            self.handle = None;
            let outcome = if self.refused.load(std::sync::atomic::Ordering::Acquire) {
                notify::monitor::Outcome::Refused
            } else {
                notify::monitor::Outcome::Ended
            };
            match outcome {
                // Deliberately silent: `run` has already printed the bus's
                // own reason, once, including what to do about it. A second
                // line here would be this supervisor's own contribution to
                // the flood the latch exists to stop.
                notify::monitor::Outcome::Refused => self.refusal_latched = true,
                notify::monitor::Outcome::Ended => {
                    eprintln!(
                        "warning: the notification monitor task ended; it will be \
                         restarted in up to {:.0}s if notifications are still \
                         enabled",
                        NOTIFY_RESTART_BACKOFF.as_secs_f64()
                    );
                    self.next_restart_attempt = Instant::now() + NOTIFY_RESTART_BACKOFF;
                }
            }
        }

        match (enabled, &self.handle) {
            (true, None) if !self.refusal_latched => {
                // Not just "no handle" but "no handle, and not backing off
                // after the last one ended on its own" -- see
                // `NOTIFY_RESTART_BACKOFF`. On every ordinary path into this
                // arm (first start; re-enabling after a deliberate stop)
                // `next_restart_attempt` is already in the past (set in
                // `new()` and reset by the `(false, Some(_))` arm below), so
                // this check costs nothing there.
                if Instant::now() >= self.next_restart_attempt {
                    self.spawn(engine);
                }
            }
            (false, Some(_)) => {
                // `.take()` before `.abort()`: `handle` must read `None` the
                // instant this returns, not only once the aborted task has
                // actually finished unwinding -- a `reconcile` that turns
                // `enabled` back on next tick must see a clean slate to
                // start into, not "an abort is already pending for this
                // one."
                if let Some(handle) = self.handle.take() {
                    handle.abort();
                }
                // A deliberate stop, not the task ending on its own -- the
                // next `(true, None)` this sees (immediately, if `enabled`
                // flips straight back) must start it right away, not
                // inherit a backoff left over from some earlier, unrelated
                // self-ended task.
                self.next_restart_attempt = Instant::now();
            }
            // Already matches: (true, Some(_)) is already running (and just
            // confirmed alive, above), (false, None) is already stopped.
            _ => {}
        }
    }
}

/// `notifications.enabled`, as the publish loop sees it: cached between
/// changes, re-read off the blocking pool when the config store says it has
/// actually changed, and never more than one read in flight at a time.
///
/// Three separate properties, all of them measured against the real daemon:
///
/// **CRITICAL 1 (single-flight).** The previous shape of this -- a bare
/// `async fn` that spawned a fresh `spawn_blocking` on every 200 ms tick and
/// bounded the `.await` on it with [`CONFIG_STAMP_READ_TIMEOUT`] -- fixed
/// the stall it was written for and replaced it with something worse. The
/// timeout abandons the `.await`, never the task: the task stays parked on
/// `ConfigStore`'s stamp for as long as the write holding it takes, and
/// nothing stopped the next tick spawning another. Measured, real daemon,
/// `config.toml.tmp` a FIFO nobody reads and one `SetMuted` to start a
/// stuck write: 30 threads to 150 in 30 s, on to tokio's 512-blocking-thread
/// cap in a few minutes -- and at that cap a `Say` over D-Bus never returned
/// (`dbus::SaydIface::submit` is a `spawn_blocking` too, and so is
/// `notify::monitor::speak`, so narration stopped with it), while SIGTERM
/// left the process sitting for a minute because `Runtime::drop` waits for
/// blocking tasks that have started. Holding the outstanding `JoinHandle`
/// here and polling *that* instead of spawning a second one is the whole
/// fix: at most one parked task exists, ever, no matter how long the write
/// stays stuck or how many ticks go by.
///
/// **IMPORTANT 2 (the gate).** Spec §8 says `enabled = false` must cost
/// nothing. The connection half holds -- no `enabled`, no task -- but the
/// read itself ran unconditionally, five times a second, forever, on a
/// daemon meant to run for weeks. `ConfigStore::generation` makes the common
/// case an atomic load: the stamp is touched only when a config change has
/// actually landed (a settings save, a tray mute, a hand edit), which on a
/// desktop is a handful of times a day rather than 432,000.
///
/// **The original stall.** Still fixed, and by the same means as before: the
/// read runs on the blocking pool and the `.await` on it is bounded, so a
/// write stuck on a hung NFS or FUSE home cannot block the publish loop's
/// `tokio::select!` -- `sigterm.recv()` included. See
/// `CONFIG_STAMP_READ_TIMEOUT`.
struct NotifyEnabledWatch {
    /// The last value read, returned as-is on every tick that finds the
    /// generation unmoved. Seeded in `new` from the store itself, so the
    /// first tick has a real value to reconcile against rather than a guess.
    enabled: bool,
    /// The generation `enabled` was read at. A read is started only when the
    /// store's generation differs from this.
    seen_generation: u64,
    /// The one outstanding blocking read, with the generation it was started
    /// at, or `None` when no read is in flight.
    ///
    /// The generation is remembered *from the moment the task was spawned*,
    /// not from when it finishes: the value the task hands back describes the
    /// store at some point at or after that, so crediting it to a later
    /// generation could mark a change as seen that this read never observed.
    /// Recording the earlier one can only ever cost one redundant re-read.
    inflight: Option<(u64, tokio::task::JoinHandle<bool>)>,
    /// Log-once for a standing stall, the same shape as
    /// `recovery_failure_logged` in the publish loop: the first tick that
    /// cannot read the store within `CONFIG_STAMP_READ_TIMEOUT` is worth a
    /// line, the fifth one 200 ms later (while the same write is presumably
    /// still stuck) is not.
    stall_logged: bool,
}

impl NotifyEnabledWatch {
    /// Seeded from the store, in that order -- generation first, value second
    /// -- so that a change landing between the two reads is seen as *not yet
    /// observed* and re-read on the first tick, rather than silently
    /// credited to the generation before it.
    fn new(store: &std::sync::Arc<config_watch::ConfigStore>) -> Self {
        let seen_generation = store.generation();
        Self {
            enabled: store.current().notifications.enabled,
            seen_generation,
            inflight: None,
            stall_logged: false,
        }
    }

    /// The value the supervisor should be reconciled against this tick.
    ///
    /// Never `Option`: a tick that cannot read the store keeps returning the
    /// last value it did read, so `reconcile` still runs and still notices a
    /// monitor task that has ended (IMPORTANT 3's real point). The previous
    /// version skipped `reconcile` entirely on a stalled read, which left a
    /// dead monitor unnoticed for as long as the stall lasted.
    async fn enabled(&mut self, store: &std::sync::Arc<config_watch::ConfigStore>) -> bool {
        let generation = store.generation();
        if self.inflight.is_none() {
            if generation == self.seen_generation {
                // The common case, and the whole of IMPORTANT 2: one atomic
                // load, no lock, no task.
                return self.enabled;
            }
            let s = store.clone();
            self.inflight = Some((
                generation,
                tokio::task::spawn_blocking(move || s.current().notifications.enabled),
            ));
        }
        let (at, handle) = self.inflight.as_mut().expect("just set above");
        let at = *at;
        // Bound the wait, not the task: whether this resolves or times out,
        // the task is either finished (and taken, below) or still parked and
        // still the only one -- the next tick polls this same handle rather
        // than starting a second.
        let result = tokio::time::timeout(CONFIG_STAMP_READ_TIMEOUT, handle).await;
        match result {
            Ok(Ok(enabled)) => {
                self.inflight = None;
                self.seen_generation = at;
                self.enabled = enabled;
                self.stall_logged = false;
            }
            // The task itself failed -- a panic under `ConfigStore::current`,
            // which is poison-tolerant and so should not happen (see its doc
            // comment). Drop the handle so the next tick starts a fresh read
            // rather than polling a task that will never answer.
            Ok(Err(e)) => {
                self.inflight = None;
                if !self.stall_logged {
                    eprintln!("warning: the config-store read task failed: {e}");
                    self.stall_logged = true;
                }
            }
            Err(_elapsed) => {
                if !self.stall_logged {
                    eprintln!(
                        "warning: reading the config store took longer than {:.0}ms \
                         (a write may be stuck); the notification monitor will keep \
                         running as last configured until it responds again",
                        CONFIG_STAMP_READ_TIMEOUT.as_millis()
                    );
                    self.stall_logged = true;
                }
            }
        }
        self.enabled
    }
}

fn models_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("SAYD_MODELS_DIR") {
        return PathBuf::from(d);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    let xdg = base.join("sayd").join("models");
    if xdg.is_dir() {
        xdg
    } else {
        PathBuf::from("models")
    }
}

/// Open the real audio device, unless `SAYD_NO_AUDIO` says to discard audio
/// instead.
///
/// `SAYD_NO_AUDIO=1` exists so the D-Bus surface can be exercised on a
/// machine with no audio device (no `/dev/snd`, PulseAudio refusing to
/// start, CI, this sandbox). It is a testing aid, not a supported mode: see
/// the README.
fn open_sink() -> Result<Box<dyn AudioSink>, String> {
    if std::env::var_os("SAYD_NO_AUDIO").is_some() {
        eprintln!("info: SAYD_NO_AUDIO set; discarding audio instead of opening a device");
        return Ok(Box::new(DiscardSink { paused: false }));
    }
    let sink = ring::RingSink::new(sayd_core::synth::SAMPLE_RATE)?;
    if sink.device_sample_rate != sayd_core::synth::SAMPLE_RATE {
        eprintln!(
            "info: audio device uses {} Hz; resampling from the synthesizer's {} Hz",
            sink.device_sample_rate,
            sayd_core::synth::SAMPLE_RATE
        );
    }
    Ok(Box::new(sink))
}

/// Print a short usage summary for `sayd --help`.
///
/// Deliberately not `clap`: `sayd`'s only argument surface is "text to
/// speak, or nothing" (see `main`'s module doc), and pulling in a parser
/// crate for two flags would be the tail wagging the dog. `sayd-cli`'s
/// `say` binary is where real argument parsing belongs.
fn print_help() {
    println!("sayd {}", env!("CARGO_PKG_VERSION"));
    println!("Local text-to-speech daemon for Wayland");
    println!();
    println!("USAGE:");
    println!("    sayd [TEXT...]");
    println!();
    println!("Run with no arguments to start the daemon (a no-op if one is already");
    println!("running). Any other arguments are joined with spaces and spoken --");
    println!("starting the daemon first if it is not already running.");
    println!();
    println!("For pause/stop/selection/clipboard/status and everything else, use");
    println!("the `say` command instead of talking to `sayd` directly.");
    println!();
    println!("OPTIONS:");
    println!("    -h, --help       Print this message and exit");
    println!("    -V, --version    Print version and exit");
}

/// Sender for "open the settings window", read by the glib main loop.
///
/// Bounded at 1 -- see `request_settings`'s doc comment for why that is only
/// sound together with `try_send`, never `send`/`send_blocking`.
///
/// A `OnceLock` rather than a parameter because the tray's menu callbacks
/// are built deep inside `ksni`'s tree and run on tokio's threads; threading
/// a channel down to them would mean changing every menu constructor for one
/// entry.
static SETTINGS_REQUESTS: std::sync::OnceLock<async_channel::Sender<()>> =
    std::sync::OnceLock::new();

/// Ask the main thread to open the settings window. Safe to call from any
/// thread, including tokio worker threads and (in Task 5) ksni's own
/// callback threads.
///
/// Uses `try_send`, not `send`/`send_blocking`. The channel is bounded at 1,
/// which expresses "at most one pending open request" -- if one is already
/// queued, a second click before the main loop drains it is dropped rather
/// than piling up (fine as long as `settings::window::open` is idempotent:
/// re-presenting an already-open window). This pairing is load-bearing: on
/// a *bounded* channel, `send`/`send_blocking` parks the calling thread once
/// the buffer is full, which here would block a tokio worker thread or a
/// ksni callback thread -- do not swap this back to `send`/`send_blocking`
/// without also going back to `async_channel::unbounded`.
///
/// This only enqueues (or drops) the request; it does not confirm the glib
/// loop is actually draining it. If the loop has not started yet
/// (`SETTINGS_REQUESTS` unset) or the receiver has been dropped, that is
/// logged below. But if the loop *has* started and then stalls or exits
/// without dropping the receiver, `try_send` still reports success -- the
/// request just sits queued, silently, until (if ever) something drains it.
/// That gap is not closed here; it would need the loop to expose whether it
/// is actually running.
pub fn request_settings() {
    match SETTINGS_REQUESTS.get() {
        Some(tx) => match tx.try_send(()) {
            Ok(()) => {}
            Err(async_channel::TrySendError::Full(())) => {
                // A request is already queued; `open()` is about to run for
                // it, and re-presenting an already-open window covers this
                // click too.
            }
            Err(async_channel::TrySendError::Closed(())) => {
                eprintln!("warning: the settings window host is gone");
            }
        },
        None => eprintln!("warning: settings requested before the UI host started"),
    }
}

/// Runs the daemon: connects to the session bus, spawns the engine, tray and
/// MPRIS, and drives the publish/shutdown loop until something ends it
/// (SIGTERM, `Quit()`, or a fatal startup error).
///
/// Split out from `main` so that GTK can own the main thread (see `main`'s
/// doc comment) while this runs on the tokio runtime's own threads instead.
/// The body is unchanged from when this function itself was `main`.
async fn run_daemon() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Finding 4: with no special-casing, these two are joined into text and
    // spoken by the daemon instead of doing what every other CLI does with
    // them. Recognised only as the *sole* argument, exactly like a real
    // parser would treat a bare `--help`/`--version` with nothing else on
    // the line -- `sayd --help really means this` still gets spoken, which
    // is correct: `sayd`'s whole argument surface is "text, or nothing."
    match args.as_slice() {
        [flag] if flag == "--help" || flag == "-h" => {
            print_help();
            return std::process::ExitCode::SUCCESS;
        }
        [flag] if flag == "--version" || flag == "-V" => {
            println!("sayd {}", env!("CARGO_PKG_VERSION"));
            return std::process::ExitCode::SUCCESS;
        }
        _ => {}
    }

    let text: String = args.join(" ");

    let connection = match zbus::connection::Builder::session() {
        Ok(b) => match b.build().await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: could not connect to the session bus: {e}");
                return std::process::ExitCode::FAILURE;
            }
        },
        Err(e) => {
            eprintln!("error: could not connect to the session bus: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // Single instance: if the name is taken, a daemon is already running.
    let owned = connection
        .request_name_with_flags(BUS_NAME, RequestNameFlags::DoNotQueue.into())
        .await;

    match owned {
        Ok(RequestNameReply::PrimaryOwner) => {}
        _ => {
            // Another daemon owns the name. Forward and exit.
            if text.trim().is_empty() {
                eprintln!("sayd is already running");
                return std::process::ExitCode::SUCCESS;
            }
            let proxy = zbus::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, "sh.sayd.Sayd1").await;
            let proxy = match proxy {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("error: could not reach the running daemon: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            let args: (String, HashMap<String, OwnedValue>) = (text, HashMap::new());
            return match tokio::time::timeout(FORWARD_CALL_TIMEOUT, proxy.call_method("Say", &args))
                .await
            {
                Ok(Ok(_)) => std::process::ExitCode::SUCCESS,
                Ok(Err(e)) => {
                    eprintln!("error: {e}");
                    std::process::ExitCode::FAILURE
                }
                Err(_) => {
                    eprintln!(
                        "error: the running sayd daemon did not respond within {:.0}s; \
                         it may be stuck or unresponsive",
                        FORWARD_CALL_TIMEOUT.as_secs_f64()
                    );
                    std::process::ExitCode::FAILURE
                }
            };
        }
    }

    // We are the daemon.
    let (mut cfg, cfg_err) = Config::load();
    // IMPORTANT 3, at startup rather than on reload: the same normalisation
    // the reload path applies (`ConfigStore::reload`), so the engine, the
    // store's stamp and the file's meaning agree from t=0 instead of from
    // the first hand edit. Without it a daemon started against `model =
    // "int4"` ran fp32 while claiming int4 until something happened to
    // rewrite the file.
    let cfg_warnings = settings::model::normalize(&mut cfg);
    for w in &cfg_warnings {
        eprintln!("warning: {}: {w}", Config::path().display());
    }
    // Kept for the tray, per spec §11 -- see `config_watch::ConfigStatus`.
    // A parse failure wins over a normalisation warning: there is no
    // honoured value to complain about in a file that did not parse at all.
    let cfg_problem = match cfg_err {
        Some(e) => {
            eprintln!("warning: {e}; using defaults");
            // Without the path, like `ConfigStore::reload`'s -- see there.
            // `load_from` builds this message as exactly `"{path}: {reason}"`,
            // so stripping that prefix is precise rather than a guess.
            let prefix = format!("{}: ", Config::path().display());
            Some(e.strip_prefix(&prefix).unwrap_or(&e).to_string())
        }
        None if !cfg_warnings.is_empty() => Some(cfg_warnings.join("; ")),
        None => None,
    };

    // A contradiction rather than a degradation, so it is the one reword
    // misconfiguration that stops a boot: automatic rewriting was asked for
    // and cannot be delivered. Gated, because in a build without the feature
    // `enabled = true` is already an inert no-op with its own diagnostic, and
    // refusing to start over a table that build never reads would be a
    // failure invented out of nothing.
    #[cfg(feature = "reword")]
    if let Some(refusal) = sayd_core::config::reword_startup_refusal(&cfg.reword) {
        eprintln!("error: {}: {refusal}", Config::path().display());
        return std::process::ExitCode::FAILURE;
    }

    // Finding 2: load and validate the ONNX Runtime dylib now, up front,
    // where a failure can be reported with a clean exit -- rather than
    // letting the first synthesis discover it lazily inside `ort`, which
    // panics instead of returning `Err` (see `sayd_kokoro::init_environment`'s
    // doc comment for the SIGABRT this avoids).
    if let Err(e) = sayd_kokoro::init_environment() {
        eprintln!("error: {e}");
        return std::process::ExitCode::FAILURE;
    }

    let synth = match kokoro_synth::KokoroSynthesizer::new(&models_dir(), &cfg) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to initialize synthesizer: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // M21: a bad *configured* default voice (as opposed to a bad
    // `--voice` on one submission) is the same footgun wearing a different
    // hat. `Engine::submit` already rejects it synchronously, submission by
    // submission, rather than wedging (see `sayd_core::engine`'s M21 doc
    // comment) -- but that only surfaces once the first utterance is
    // spoken. Checking here, once, at startup, lets an operator notice a
    // typo'd config *before* every default-voice submission starts failing
    // the same way.
    if !synth.voice_exists(&cfg.voice) {
        eprintln!(
            "warning: configured default voice '{}' has no installed voice pack; \
             every submission that does not override it with its own --voice will \
             be rejected until this is fixed. Check {}/voices for installed voices.",
            cfg.voice,
            models_dir().display()
        );
    }
    let sink = match open_sink() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to open audio output: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let engine = EngineHandle::spawn(cfg.clone(), Box::new(synth), sink);

    // Config is a two-way surface from here on: the settings window writes
    // through this, and a hand edit comes back through the watcher. Both
    // end at `Command::ApplyConfig`. The store is told what the engine was
    // spawned with so that the two agree from t=0 rather than from the
    // first write.
    let store = std::sync::Arc::new(config_watch::ConfigStore::new(
        Config::path(),
        engine.clone(),
        cfg,
    ));
    store.status().set(cfg_problem);
    // §8: asking for rewording in a build with no client in it is not an
    // error -- everything is spoken as written, exactly as it was before
    // this feature existed -- but it must not be silent, or a user who set
    // the switch would have no way at all to discover why nothing changed.
    //
    // Keyed on `notifications`, not on the `enabled` master: the master
    // defaults to true and the 2026-08-24 migration turns it on for every
    // existing config, so keying there would print this at every start on
    // every machine that has never asked for rewording at all.
    #[cfg(not(feature = "reword"))]
    if store.current().reword.notifications {
        eprintln!(
            "info: [reword] notifications = true, but this build has no rewording \
             client (rebuild with --features reword); text will be spoken as written"
        );
    }
    // The settings window is built and destroyed on demand, so the model it
    // edits has to outlive every window: it lives behind `settings`'s own
    // `OnceLock` from here on.
    //
    // Seeded from `store.current()` rather than `engine.config()`: the store
    // was just told what the engine was spawned with, so the two are the
    // same value, but this one needs no 250ms round trip through an engine
    // thread that may be mid-chunk and has no `Option` to invent a fallback
    // for. `SettingsModel::refresh` re-reads it every time a window opens
    // regardless, so a hand edit in between is not missed.
    settings::install(
        std::sync::Arc::new(settings::model::SettingsModel::new(
            store.clone(),
            models_dir(),
            store.current(),
        )),
        engine.clone(),
    );

    // Held for the life of the process: dropping the watcher stops the
    // watch, silently.
    let _config_watcher = match config_watch::spawn(store.clone()) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!("warning: {e}; config changes will need a restart");
            None
        }
    };

    let iface = dbus::SaydIface::new(engine.clone(), store.clone());
    if let Err(e) = connection.object_server().at(OBJECT_PATH, iface).await {
        eprintln!("error: could not serve the interface: {e}");
        return std::process::ExitCode::FAILURE;
    }

    eprintln!("sayd: listening on {BUS_NAME} at {OBJECT_PATH}");

    // A tray registration failure must not be fatal: a bare sway config
    // without waybar has no StatusNotifierWatcher running at all, and the
    // daemon is still useful serving just the control interface. Log once
    // and carry on rather than exit.
    let tray_handle = match tray::spawn(engine.clone(), store.clone(), store.status()).await {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!("info: {e}; continuing without a tray icon");
            None
        }
    };

    // Same reasoning as the tray immediately above: no MPRIS means no media
    // keys and no playerctl/waybar mpris module, but the daemon is still
    // useful serving just the control interface, so this must not be fatal
    // either.
    let mpris_handle = match mpris::spawn(engine.clone(), store.clone()).await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("info: {e}; continuing without MPRIS (media keys, playerctl)");
            None
        }
    };

    // If text was given on the command line, speak it now.
    if !text.trim().is_empty() {
        match engine.submit(text, sayd_core::engine::SayOpts::default()) {
            Ok(_) => {}
            Err(e) => eprintln!("error: {e}"),
        }
    }

    // Publish property changes and watch for shutdown.
    let iface_ref = match connection
        .object_server()
        .interface::<_, dbus::SaydIface>(OBJECT_PATH)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: could not obtain the interface reference: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let mut last = engine.snapshot();
    // C1: tray and MPRIS fan-out are tracked independently of `last` (which
    // stays purely for the D-Bus control interface's own property diffing)
    // so that a fan-out skipped during a `FANOUT_RETRY_INTERVAL` backoff
    // still gets delivered on the next attempt even if nothing *else* has
    // changed in the meantime -- see `FANOUT_TIMEOUT`'s doc comment.
    let mut tray_last_sent = last.clone();
    // IMPORTANT 4: the tray reads the config file's standing problem live
    // from the shared slot when it renders its menu, but a host is entitled
    // to cache the layout until told otherwise -- so a problem appearing
    // (or clearing) while nothing else moves still has to trigger a fan-out,
    // exactly as a snapshot change does.
    let config_status = store.status();
    let mut tray_last_problem = config_status.get();
    let mut next_tray_attempt = Instant::now();
    let mut tray_backoff_logged = false;
    let mut mpris_last_sent = last.clone();
    let mut next_mpris_attempt = Instant::now();
    let mut mpris_backoff_logged = false;
    // Reacquisition state for the recovery branch below: `next_recovery_attempt`
    // throttles retries while `State::Error` persists, and `recovery_failure_logged`
    // keeps a standing failure to one line instead of one every `PUBLISH_INTERVAL`.
    let mut next_recovery_attempt = Instant::now();
    let mut recovery_failure_logged = false;
    // Finding 3: `RemainingSeconds` publish throttling. `next_remaining_publish`
    // paces periodic emissions to `REMAINING_SECONDS_PUBLISH_INTERVAL` while
    // counting down; `remaining_was_active` remembers whether the previous
    // tick was counting down, so the transition into Idle/Paused/Error gets
    // exactly one settling emission (correcting a client's cache to the
    // final value) instead of either silence or a steady stream of zeros.
    let mut next_remaining_publish = Instant::now();
    let mut remaining_was_active = false;
    // M5 Task 5: owns the notification monitor's `JoinHandle`, started and
    // stopped from `notifications.enabled` on every tick below. See
    // `NotifyMonitorSupervisor::reconcile`'s doc comment.
    let mut notify_supervisor = NotifyMonitorSupervisor::new();
    // CRITICAL 1 / IMPORTANT 2: what the supervisor is reconciled against,
    // and everything that keeps reading it cheap and bounded. See
    // `NotifyEnabledWatch`.
    let mut notify_enabled = NotifyEnabledWatch::new(&store);
    let mut ticker = tokio::time::interval(PUBLISH_INTERVAL);
    let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: could not install the SIGTERM handler: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now = engine.snapshot();
                let now_instant = Instant::now();

                // M5 Task 5: start/stop the notification monitor with
                // `notifications.enabled`. `ConfigStore::current()` is a
                // plain mutex read, not a round trip through the engine
                // thread (contrast `EngineHandle::config`, a bounded channel
                // call), and its value is provably always in sync with what
                // the engine is running: every config mutation in this
                // daemon (a settings-window save, a hand edit picked up by
                // `reload`, a tray mute, an MPRIS rate change) goes through
                // `ConfigStore::write_locked`, which stamps it here *and*
                // sends the engine's own `ApplyConfig` in the same critical
                // section -- so this can never observe a config the engine
                // itself has not also been told about. See
                // `ConfigStore::current`'s doc comment.
                //
                // CRITICAL 1: that same mutex is held by `write_locked`
                // across a synchronous, unbounded disk write, so the read
                // goes through `NotifyEnabledWatch` -- gated on
                // `ConfigStore::generation`, off the blocking pool, one read
                // in flight at a time, and bounded -- rather than being taken
                // directly here. Taken directly, a write stuck on a hung
                // filesystem stalled this whole `select!` loop,
                // `sigterm.recv()` included; taken on an unbounded number of
                // `spawn_blocking` hops, it exhausted tokio's blocking pool
                // and wedged the daemon harder. See `NotifyEnabledWatch` and
                // `CONFIG_STAMP_READ_TIMEOUT` for both measurements.
                //
                // `reconcile` runs on every tick regardless of whether a
                // fresh value could be read: it is also what notices a
                // monitor task that has ended on its own (IMPORTANT 2/3).
                let notifications_enabled = notify_enabled.enabled(&store).await;
                notify_supervisor.reconcile(notifications_enabled, &engine);

                // Finding 3: bounded `RemainingSeconds` publishing. "Active"
                // means it is actually counting down -- `Speaking` with
                // something still outstanding -- not merely nonzero (a
                // `Paused` utterance mid-playback has a nonzero
                // `remaining_secs` that is not currently changing, so there
                // is nothing to publish about it beyond the settle emission
                // below).
                let remaining_is_active =
                    now.state == sayd_core::engine::State::Speaking && now.remaining_secs > 0.0;
                let remaining_due =
                    remaining_is_active && now_instant >= next_remaining_publish;
                // Just stopped counting down: one more emission corrects a
                // caching client to the settled value (typically 0) instead
                // of leaving it stuck on the last mid-countdown number.
                let remaining_settling = !remaining_is_active && remaining_was_active;

                if now != last || remaining_due || remaining_settling {
                    let ctx = iface_ref.signal_emitter();
                    let i = iface_ref.get().await;
                    // Emit only what changed; a client diffing every property
                    // on every tick would see them churn on every tick.
                    if now.state != last.state {
                        let _ = i.state_changed(ctx).await;
                    }
                    if now.muted != last.muted {
                        let _ = i.muted_changed(ctx).await;
                    }
                    if now.voice != last.voice {
                        let _ = i.voice_changed(ctx).await;
                    }
                    if (now.speed - last.speed).abs() > f32::EPSILON {
                        let _ = i.speed_changed(ctx).await;
                    }
                    if now.queue_len != last.queue_len {
                        let _ = i.queue_length_changed(ctx).await;
                    }
                    if now.queue_heads != last.queue_heads {
                        let _ = i.queue_heads_changed(ctx).await;
                    }
                    if now.current_id != last.current_id {
                        let _ = i.current_id_changed(ctx).await;
                        let _ = i.current_text_changed(ctx).await;
                    }
                    if now.error != last.error {
                        let _ = i.error_changed(ctx).await;
                    }
                    if remaining_due || remaining_settling {
                        let _ = i.remaining_seconds_changed(ctx).await;
                    }
                    last = now.clone();
                }

                // C1: tray and MPRIS fan-out, bounded and backed off
                // independently of the D-Bus emissions above and of each
                // other -- see `FANOUT_TIMEOUT`'s doc comment for why an
                // unbounded wait here used to freeze the whole loop.
                let config_problem = config_status.get();
                if let Some(h) = tray_handle.as_ref() {
                    if (now != tray_last_sent || config_problem != tray_last_problem)
                        && now_instant >= next_tray_attempt
                    {
                        let s = now.clone();
                        match tokio::time::timeout(
                            FANOUT_TIMEOUT,
                            h.update(move |t| t.set_snapshot(s)),
                        )
                        .await
                        {
                            Ok(_) => {
                                tray_last_sent = now.clone();
                                tray_last_problem = config_problem.clone();
                                tray_backoff_logged = false;
                            }
                            Err(_) => {
                                next_tray_attempt = now_instant + FANOUT_RETRY_INTERVAL;
                                if !tray_backoff_logged {
                                    eprintln!(
                                        "warning: the tray host did not respond to an update \
                                         within {:.1}s; backing off tray updates for {:.0}s so \
                                         a stuck host cannot stall the daemon",
                                        FANOUT_TIMEOUT.as_secs_f64(),
                                        FANOUT_RETRY_INTERVAL.as_secs_f64()
                                    );
                                    tray_backoff_logged = true;
                                }
                            }
                        }
                    }
                }
                // Same "emit only what changed" discipline as the D-Bus
                // properties above -- MPRIS's `PropertiesChanged` is opt-in
                // per property (`Server::properties_changed` takes the
                // changed values themselves, not a dirty flag), so this
                // builds exactly the ones that moved rather than resending
                // all three on every publish. Diffed against `mpris_last_sent`
                // rather than `last` for the same reason as the tray above.
                if let Some(server) = mpris_handle.as_ref() {
                    if now_instant >= next_mpris_attempt {
                        let mut mpris_props = Vec::new();
                        if now.state != mpris_last_sent.state {
                            mpris_props.push(mpris_server::Property::PlaybackStatus(
                                mpris::playback_status_for(now.state),
                            ));
                        }
                        // `current_text` only ever changes together with
                        // `current_id` (a new utterance becoming current),
                        // so this mirrors the D-Bus `current_id_changed`
                        // branch above rather than diffing the text too.
                        if now.current_id != mpris_last_sent.current_id {
                            mpris_props.push(mpris_server::Property::Metadata(
                                mpris::metadata_for(now.current_id, &now.current_text),
                            ));
                        }
                        // I1: `configured_speed`, not `speed` -- `Rate`
                        // reads `configured_speed` (see `mpris::rate`'s doc
                        // comment), so the proactive change notification
                        // must be built from the same field or a client
                        // would receive a `PropertiesChanged` whose value
                        // disagrees with what a following `Get` returns.
                        if (now.configured_speed - mpris_last_sent.configured_speed).abs()
                            > f32::EPSILON
                        {
                            mpris_props.push(mpris_server::Property::Rate(
                                now.configured_speed as f64,
                            ));
                        }
                        if !mpris_props.is_empty() {
                            match tokio::time::timeout(
                                FANOUT_TIMEOUT,
                                server.properties_changed(mpris_props),
                            )
                            .await
                            {
                                Ok(_) => {
                                    mpris_last_sent = now.clone();
                                    mpris_backoff_logged = false;
                                }
                                Err(_) => {
                                    next_mpris_attempt = now_instant + FANOUT_RETRY_INTERVAL;
                                    if !mpris_backoff_logged {
                                        eprintln!(
                                            "warning: the MPRIS host did not respond to an \
                                             update within {:.1}s; backing off MPRIS updates \
                                             for {:.0}s so a stuck host cannot stall the daemon",
                                            FANOUT_TIMEOUT.as_secs_f64(),
                                            FANOUT_RETRY_INTERVAL.as_secs_f64()
                                        );
                                        mpris_backoff_logged = true;
                                    }
                                }
                            }
                        }
                    }
                }
                if remaining_due {
                    next_remaining_publish = now_instant + REMAINING_SECONDS_PUBLISH_INTERVAL;
                } else if remaining_settling {
                    // Ready to fire immediately the moment it becomes active
                    // again, rather than waiting out a stale interval.
                    next_remaining_publish = now_instant;
                }
                remaining_was_active = remaining_is_active;

                // Recover from a *sink*-kind engine error by reacquiring the
                // device, regardless of which cpal `StreamError` variant
                // produced it -- not just ones whose message happens to
                // mention "device". cpal 0.17.1's `StreamInvalidated` and
                // `BufferUnderrun` messages don't contain "device" the way
                // `DeviceNotAvailable`'s does, so an earlier version of this
                // loop that matched on the message text missed exactly the
                // ordinary desktop hiccups (a PulseAudio restart, an XRUN)
                // this loop exists to recover from -- hence gating on
                // `error_kind == Sink` rather than the message string.
                //
                // Gating on `state == Error` alone (an even earlier version)
                // was wrong in the other direction: `Engine::tick`'s
                // *synthesis* failure path (bad model path, corrupt weights)
                // sets the same `state == Error`, and reacquiring a sink that
                // was never the problem would clear it via `replace_sink`
                // every time, making the daemon look recovered while every
                // submission kept failing the same way, silently. `Engine`
                // now tags *why* it is in `Error` (`Snapshot::error_kind`,
                // sayd-core/src/engine.rs); this loop only ever reacquires
                // for `Sink`. A `Synth` (or rejected-submission) error is
                // left alone: it persists, `Engine::submit` now rejects new
                // submissions itself rather than silently clearing it (see
                // `ErrorKind`'s doc comment), and an explicit Stop/Next/
                // SkipSentence/SetMuted can still dismiss it.
                let is_sink_error =
                    matches!(last.error_kind, Some(sayd_core::engine::ErrorKind::Sink));
                if is_sink_error {
                    let now_instant = Instant::now();
                    if now_instant >= next_recovery_attempt {
                        // Throttle to `RECOVERY_RETRY_INTERVAL` rather than
                        // retrying every `PUBLISH_INTERVAL`: `open_sink` opens a
                        // real audio stream, and a persistent outage must not
                        // spin that hot.
                        next_recovery_attempt = now_instant + RECOVERY_RETRY_INTERVAL;
                        match open_sink() {
                            Ok(s) => {
                                eprintln!("info: audio device reacquired");
                                engine.send(sayd_core::engine::Command::Stop);
                                engine.replace_sink(s);
                                recovery_failure_logged = false;
                            }
                            Err(e) => {
                                // Log only the first failure of a standing
                                // outage, not once per retry, so a long outage
                                // doesn't flood stderr.
                                if !recovery_failure_logged {
                                    eprintln!("warning: could not reacquire audio device: {e}");
                                    recovery_failure_logged = true;
                                }
                            }
                        }
                    }
                } else {
                    recovery_failure_logged = false;
                }

                if engine.has_shut_down() {
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
        }
    }

    eprintln!("sayd: shutting down");
    // MINOR 3: explicit, like every other step in this teardown sequence,
    // rather than relying on the monitor task simply ending when the tokio
    // runtime drops at process exit. That implicit path is not currently a
    // leak or a hang -- traced through tokio's runtime shutdown -- but
    // leaving it implicit is inconsistent with `settings::flush_pending()`
    // and `engine.shutdown()` right below, both spelled out on purpose, and
    // would silently stop being safe if this function's shutdown strategy
    // ever changed. `reconcile(false, ...)` is `NotifyMonitorSupervisor`'s
    // own, already-tested way of stopping the task (see its doc comment for
    // why a plain `abort` loses nothing here) -- reused rather than reaching
    // into `handle` by hand, so shutdown takes exactly the path
    // `toggling_enabled_starts_and_stops_the_monitor` already exercises.
    notify_supervisor.reconcile(false, &engine);
    // Before the engine goes away: a settings edit made in the last 250ms
    // (`settings::model::WRITE_DEBOUNCE`) can still be sitting on the
    // writer thread's queue, shown to the user as saved, and this model is
    // never otherwise dropped in the daemon's lifetime -- see
    // `settings::flush_pending`'s doc comment.
    settings::flush_pending();
    engine.shutdown();
    std::process::ExitCode::SUCCESS
}

/// GTK4 must run on the thread that initialises it, so that thread has to be
/// this one -- `main` -- rather than whatever thread `#[tokio::main]` used to
/// hand off to. The daemon does not need the main thread for anything, so
/// the swap is: build a `tokio::runtime::Runtime`, spawn `run_daemon` on it,
/// and run a `glib::MainLoop` here instead.
///
/// GTK itself is *not* initialised here, or anywhere at startup -- only on
/// the first settings request, inside `settings::window::open` (Task 5). A
/// daemon that never opens the window never touches GTK, so it behaves
/// exactly as it did before this restructure, on a machine with no display
/// at all; an `adw::init()` that fails there costs nothing but a logged
/// warning when a settings request actually arrives.
fn main() -> std::process::ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: could not start the runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let main_ctx = glib::MainContext::default();
    let main_loop = glib::MainLoop::new(Some(&main_ctx), false);

    // Bounded at 1, drained with `try_send` -- see `request_settings`'s doc
    // comment for why the two go together.
    let (tx, rx) = async_channel::bounded::<()>(1);
    let _ = SETTINGS_REQUESTS.set(tx);
    main_ctx.spawn_local(async move {
        while rx.recv().await.is_ok() {
            settings::window::open();
        }
    });

    // The daemon is what decides when sayd stops -- SIGTERM, `Quit()`, a
    // fatal startup error -- so ending its future has to bring the glib loop
    // down with it, carrying the exit code it chose out through `exit`
    // rather than letting `main_loop.run()` return one of its own (it has
    // none to give: `MainLoop::quit` takes no code).
    //
    // Defaults to `FAILURE`, not `SUCCESS`: the only thing that overwrites
    // this is the explicit assignment below, on `run_daemon`'s normal
    // return. If that line never runs -- see `QuitOnDrop` -- `main` must not
    // report success for a process that never actually finished.
    let exit = std::sync::Arc::new(std::sync::Mutex::new(std::process::ExitCode::FAILURE));
    let daemon_exit = exit.clone();

    // Drops the glib main loop even if `run_daemon` panics instead of
    // returning.
    //
    // Before this restructure, the daemon's body ran under `#[tokio::main]`
    // directly on `main`'s thread: a panic there unwound out of `main`
    // itself and the process died with exit code 101, same as any other
    // panicking `main`. Moving that body onto a `tokio::spawn`'d task
    // changed this silently -- tokio wraps a spawned task's poll in
    // `catch_unwind`, stores the panic in the (here, discarded) `JoinHandle`,
    // and the task simply ends. Nothing tells `main_loop.run()` to return:
    // it blocks forever, immune to both SIGTERM and SIGINT. It is worse than
    // a plain hang, too -- `tokio::signal`'s process-wide handlers for both
    // signals are already installed by the time the publish loop is
    // running, and the runtime keeps them drained with nowhere to deliver
    // them, so both signals are silently swallowed rather than merely
    // unhandled. Only SIGHUP (which `sayd` does not install a handler for)
    // or SIGKILL end it. `Restart=on-failure` never fires, because the unit
    // never exits.
    //
    // A local variable in the spawned async block is dropped during unwind
    // exactly like it would be on a synchronous panic, so a guard held there
    // for the lifetime of `run_daemon().await` runs on every exit path --
    // normal return, an early `return` inside `run_daemon`, or a panic --
    // with no special-casing needed for any of them.
    struct QuitOnDrop {
        main_ctx: glib::MainContext,
        main_loop: glib::MainLoop,
    }
    impl Drop for QuitOnDrop {
        fn drop(&mut self) {
            // Finding 7: `run_daemon`'s tidy shutdown flushes a pending
            // settings edit before it returns, but it is not the only way
            // out. Three post-`install` early returns (the interface not
            // serving, the interface reference not obtainable, the SIGTERM
            // handler not installing) and a panic anywhere in the body all
            // land here instead, and used to quit the loop with an edit the
            // user was shown as saved still sitting on the writer's queue.
            // Nothing can request a settings window that early *today* --
            // the tray is what opens it and those returns precede
            // `tray::spawn` -- but that is an accident of ordering, not an
            // invariant, and this guard is the one place every exit passes
            // through. Idempotent: on the tidy path the flush has already
            // happened and this is the documented fast no-op.
            settings::flush_pending();

            // `Priority::DEFAULT`, not `invoke`'s own default of
            // `DEFAULT_IDLE` (the same priority `idle_add_once` used to run
            // this at): once Task 5 opens a real window, `DEFAULT_IDLE`
            // (200) sits *below* `GDK_PRIORITY_REDRAW` (120, and lower
            // numbers run first in glib), so a continuously animating
            // widget could keep preempting the quit source and shutdown
            // latency would become unbounded while that window is open.
            // `DEFAULT` (0) outranks the redraw source and is serviced
            // ahead of it.
            let main_loop = self.main_loop.clone();
            self.main_ctx
                .invoke_with_priority(glib::Priority::DEFAULT, move || main_loop.quit());
        }
    }

    let quit_guard = QuitOnDrop {
        main_ctx: main_ctx.clone(),
        main_loop: main_loop.clone(),
    };
    rt.spawn(async move {
        let _quit_guard = quit_guard;
        let code = run_daemon().await;
        *daemon_exit.lock().expect("exit mutex") = code;
    });

    main_loop.run();
    let code = *exit.lock().expect("exit mutex");
    // Bounded, rather than letting `rt` drop here: `Runtime::drop` waits
    // without limit for blocking tasks that have started, and since this
    // milestone one of those is a `ureq` request bounded only by
    // `reword::http_ceiling` -- which is as long as the user's configured
    // deadline. See `RUNTIME_SHUTDOWN_GRACE`.
    rt.shutdown_timeout(RUNTIME_SHUTDOWN_GRACE);
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use sayd_core::synth::StubSynthesizer;

    /// A real, spawned `EngineHandle` -- a stub synthesizer and a
    /// [`DiscardSink`] so `NotifyMonitorSupervisor::reconcile` has a genuine
    /// `EngineHandle` to clone into a spawned task, without touching a real
    /// model or a real audio device.
    fn test_engine() -> EngineHandle {
        EngineHandle::spawn(
            Config::default(),
            Box::new(StubSynthesizer::new()),
            Box::new(DiscardSink { paused: false }),
        )
    }

    /// `enabled = false` must cost exactly nothing -- no second D-Bus
    /// connection, no task. The cheap wrong implementation starts the
    /// monitor unconditionally and filters `enabled` inside it, which looks
    /// identical from the outside until someone counts connections.
    ///
    /// Deliberately a plain `#[test]`, not `#[tokio::test]`: no tokio
    /// runtime is running underneath it. `tokio::spawn` panics when called
    /// outside one, so if `reconcile` ever reached its `tokio::spawn` call
    /// on the `enabled = false` path -- exactly the cheap wrong
    /// implementation this test exists to catch -- this test would panic
    /// instead of quietly passing.
    #[test]
    fn a_disabled_monitor_is_never_started() {
        let engine = test_engine();
        let mut sup = NotifyMonitorSupervisor::new();

        sup.reconcile(false, &engine);
        assert!(
            sup.handle.is_none(),
            "no monitor task may exist while notifications.enabled is false"
        );

        engine.shutdown();
    }

    /// Turning it on at runtime starts it; turning it off stops it -- and
    /// "stops it" is checked against the task actually ending, not just the
    /// handle being dropped.
    #[tokio::test]
    async fn toggling_enabled_starts_and_stops_the_monitor() {
        let engine = test_engine();
        let mut sup = NotifyMonitorSupervisor::new();

        sup.reconcile(true, &engine);
        let running = sup
            .handle
            .as_ref()
            .expect("enabled = true must start a task");
        assert!(!running.is_finished(), "the monitor must be running");
        // Taken before the task is touched again, so it can prove the task
        // itself ends below -- `sup.handle` is about to become `None`, which
        // says nothing about whether the task it pointed at is still alive.
        let abort_handle = running.abort_handle();

        // Reconciling again with the same value must be a no-op: it must
        // neither spawn a second task nor abort the one already running.
        sup.reconcile(true, &engine);
        assert!(
            !abort_handle.is_finished(),
            "reconcile must not restart an already-running monitor"
        );

        sup.reconcile(false, &engine);
        assert!(
            sup.handle.is_none(),
            "the handle must be gone once disabled"
        );

        // `abort()` only requests cancellation; give the runtime a moment to
        // actually schedule and finish tearing the task down before checking
        // it, rather than asserting on a request that has not landed yet.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !abort_handle.is_finished() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            abort_handle.is_finished(),
            "the monitor task must actually stop once disabled, not just lose its handle"
        );

        engine.shutdown();
    }

    /// A store on a real path, with `config.toml.tmp` a FIFO nobody reads,
    /// and a thread parked inside `set_muted`'s write to it.
    ///
    /// This is `config_watch.rs`'s `a_mute_takes_effect_even_while_the_write_
    /// is_stuck` technique: `Config::save_to` writes `<path>.tmp` with a
    /// plain `std::fs::write`, and opening a FIFO for writing blocks until
    /// something opens the read end -- so the write never returns, with the
    /// stamp's lock held across it, which is precisely the state CRITICAL 1
    /// is about.
    ///
    /// The returned closure *must* be called before the test ends. Dropping
    /// a tokio runtime waits for in-flight `spawn_blocking` tasks, and a read
    /// parked on that same lock is exactly such a task: leaving the write
    /// stuck hangs the whole test binary at teardown instead of failing it.
    fn stuck_write(
        dir: &tempfile::TempDir,
        engine: &EngineHandle,
    ) -> (
        std::sync::Arc<config_watch::ConfigStore>,
        impl FnOnce() + use<>,
    ) {
        let path = dir.path().join("config.toml");
        let tmp = path.with_extension("toml.tmp");
        let tmp_c = std::ffi::CString::new(tmp.to_str().expect("utf8 path")).expect("no NUL");
        assert_eq!(
            unsafe { libc::mkfifo(tmp_c.as_ptr(), 0o600) },
            0,
            "mkfifo failed: {}",
            std::io::Error::last_os_error()
        );

        let store = std::sync::Arc::new(config_watch::ConfigStore::new(
            path,
            engine.clone(),
            Config::default(),
        ));
        let writer = store.clone();
        let writer_thread = std::thread::spawn(move || {
            let _ = writer.set_muted(true);
        });
        // Give the writer thread a moment to actually reach the stuck write
        // and take the stamp's lock, so what follows contends with it
        // instead of racing to go first.
        std::thread::sleep(Duration::from_millis(50));

        let unstick = move || {
            // Opening the read end lets `save_to`'s `write` complete, which
            // releases the stamp's lock, which lets every task parked on it
            // finish.
            let _ = std::fs::read(&tmp);
            writer_thread.join().expect("writer thread does not panic");
        };
        (store, unstick)
    }

    /// CRITICAL 1, the property that actually matters and that the previous
    /// fix in these lines got wrong: a stuck write must not let the publish
    /// loop accumulate blocking tasks. The 250ms timeout abandons the
    /// `.await`, never the task -- so without single-flight every tick left
    /// one more thread parked on the stamp, and the daemon walked into
    /// tokio's 512-blocking-thread cap in a couple of minutes (measured, real
    /// daemon: 30 threads to 541, after which a `Say` over D-Bus never
    /// returned).
    ///
    /// `ConfigStore::stamp_reads` counts `current()` calls, and `current()`
    /// increments it *before* it takes the stamp -- so it counts blocking
    /// tasks that have started and parked, which is exactly the quantity that
    /// used to grow without bound. Ticking the watch several times while the
    /// write is stuck must produce one, not one per tick.
    #[tokio::test]
    async fn a_stuck_write_cannot_pile_up_blocking_config_reads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = test_engine();
        let (store, unstick) = stuck_write(&dir, &engine);

        // Constructed before the gate can be consulted, so it holds no
        // generation the store has moved past -- then `set_muted`'s write
        // (stuck, but its `ApplyConfig`/generation bump comes only when it
        // finishes) leaves the watch with a read to make and no way to
        // complete it.
        let mut watch = NotifyEnabledWatch {
            enabled: false,
            // A generation the store cannot be at, so `enabled()` is forced
            // to actually read rather than short-circuiting on the gate --
            // this test is about the read path, not about the gate (that is
            // `an_unchanged_config_is_not_re_read_on_every_tick`).
            seen_generation: u64::MAX,
            inflight: None,
            stall_logged: false,
        };

        let before = store.stamp_reads();
        for _ in 0..4 {
            // Each of these times out; the value it hands back is the cached
            // one, unchanged.
            assert!(!watch.enabled(&store).await);
        }
        let started = store.stamp_reads() - before;

        unstick();
        engine.shutdown();

        assert_eq!(
            started, 1,
            "four ticks against a stuck write started {started} blocking reads; \
             the loop must hold the one outstanding task and never spawn a \
             second while it is in flight"
        );
    }

    /// IMPORTANT 2: §8's "`enabled = false` costs nothing" applies to the
    /// read too, not just to the connection. Nothing changed the config, so
    /// no tick may take the stamp at all -- and when something does change
    /// it, the very next tick must see it.
    #[tokio::test]
    async fn an_unchanged_config_is_not_re_read_on_every_tick() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let engine = test_engine();
        let store = std::sync::Arc::new(config_watch::ConfigStore::new(
            path,
            engine.clone(),
            Config::default(),
        ));

        let mut watch = NotifyEnabledWatch::new(&store);
        let after_seeding = store.stamp_reads();
        for _ in 0..20 {
            assert!(
                !watch.enabled(&store).await,
                "the default config has notifications disabled"
            );
        }
        assert_eq!(
            store.stamp_reads(),
            after_seeding,
            "an unchanged config must be answered from the generation counter \
             alone -- no lock, no blocking task, 5 times a second for weeks"
        );

        // A real change, through the same path every config mutation in the
        // daemon takes.
        let seed = store.current();
        let mut next = seed.clone();
        next.notifications.enabled = true;
        store
            .save_merging(&seed, &next)
            .expect("the write succeeds");
        let before_change = store.stamp_reads();

        assert!(
            watch.enabled(&store).await,
            "a config change must be picked up on the next tick"
        );
        assert!(
            store.stamp_reads() > before_change,
            "picking it up means actually reading the store"
        );
        engine.shutdown();
    }

    /// CRITICAL 1: the reviewer's probe, reproduced directly against
    /// `NotifyEnabledWatch::enabled` rather than the whole publish loop --
    /// there is no way to drive the loop's `tokio::select!` from a unit
    /// test without a real bus connection, a real engine and a real tray,
    /// but the thing that actually blocked it (`ConfigStore::current()`
    /// behind a stuck write) is exercised exactly the way `config_watch.rs`'s
    /// `a_mute_takes_effect_even_while_the_write_is_stuck` reproduces
    /// "stuck": a named pipe at the write's temp-file path blocks
    /// `Config::save_to`'s `std::fs::write` forever, with the stamp's lock
    /// held across it the whole time (see `ConfigStore::update`'s doc
    /// comment).
    #[tokio::test]
    async fn a_stuck_write_does_not_delay_reading_notifications_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = test_engine();
        let (store, unstick) = stuck_write(&dir, &engine);

        let mut watch = NotifyEnabledWatch {
            enabled: true,
            // Forces a read: the stuck write has not bumped the generation
            // (it bumps only once it finishes), so the gate would otherwise
            // -- correctly, and this is IMPORTANT 2's point -- decline to
            // read at all, and there would be no wait left to bound.
            seen_generation: u64::MAX,
            inflight: None,
            stall_logged: false,
        };

        let started = tokio::time::Instant::now();
        let result = watch.enabled(&store).await;
        let elapsed = started.elapsed();

        // The bound this actually proves: whatever awaits this (the publish
        // loop's `ticker.tick()` arm) is capped at
        // `CONFIG_STAMP_READ_TIMEOUT`, not at however long the write takes --
        // which here is "forever." The margin over the timeout absorbs
        // scheduling jitter without weakening what is being checked: this
        // used to not return within 500ms at all.
        assert!(
            elapsed < CONFIG_STAMP_READ_TIMEOUT + Duration::from_millis(750),
            "reading the config store while a write was stuck took {elapsed:?}; \
             the publish loop's tokio::select! (and SIGTERM handling with it) \
             would have been blocked for at least that long"
        );
        assert!(
            result,
            "a read that could not complete within the bound must hand back \
             the last value it did read, not a default -- the supervisor is \
             reconciled against this on every tick"
        );

        unstick();
        engine.shutdown();
    }

    /// IMPORTANT 2: a monitor task that ends on its own (the documented
    /// trigger is a permanently refused `become_monitor`; see
    /// `notify::monitor::run`'s doc comment) must be noticed rather than
    /// mistaken for "still running" forever, and noticing it must not turn
    /// into a hot restart loop against a refusal that will not change from
    /// one tick to the next.
    #[tokio::test]
    async fn a_task_that_ends_on_its_own_is_noticed_and_not_hot_restarted() {
        let engine = test_engine();
        let mut sup = NotifyMonitorSupervisor::new();

        // Stand in for `run` ending on its own with a task that finishes
        // immediately: what `reconcile` has to notice is "the handle it
        // holds has finished", true of any ended task, not something only
        // the real monitor can produce -- and the real one needs a session
        // bus to refuse `become_monitor` against, which this test has no
        // business standing up just to prove this.
        sup.handle = Some(tokio::spawn(async {}));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !sup.handle.as_ref().expect("just set").is_finished()
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            sup.handle.as_ref().expect("just set").is_finished(),
            "setup: the stand-in task must actually have finished"
        );

        sup.reconcile(true, &engine);
        assert!(
            sup.handle.is_none(),
            "a finished task must be noticed and cleared, not held onto \
             forever as if it were still running"
        );

        // Immediately afterward, `enabled = true` must not restart it --
        // doing so on every tick would make a standing policy refusal a hot
        // loop, re-spawning (and, for the real monitor, re-attempting
        // `become_monitor`) five times a second forever.
        sup.reconcile(true, &engine);
        assert!(
            sup.handle.is_none(),
            "a restart must wait out NOTIFY_RESTART_BACKOFF, not fire on \
             the very next tick"
        );

        // Once the backoff has elapsed, the next tick does restart it.
        tokio::time::sleep(NOTIFY_RESTART_BACKOFF + Duration::from_millis(200)).await;
        sup.reconcile(true, &engine);
        assert!(
            sup.handle.is_some(),
            "once the backoff has elapsed, enabled = true must restart the monitor"
        );

        engine.shutdown();
    }

    /// IMPORTANT 3: a task that ended because the bus *refused* monitoring is
    /// the one case that must never be restarted on a timer. §2's failure
    /// table says "log once with the reason, run without narration"; the
    /// backoff-restart above turned that into a fresh connection, auth
    /// handshake and refused `BecomeMonitor` every 5s for the life of the
    /// process -- measured, 37 log lines in 90 seconds, each with a new
    /// unique bus name, for a verdict that cannot change.
    ///
    /// The refusal is staged the way the real task reports it (the flag
    /// `NotifyMonitorSupervisor::spawn`'s wrapper sets from
    /// `Outcome::Refused`) rather than by standing up a deny-policy
    /// `dbus-daemon` here: that the real `run` actually returns
    /// `Outcome::Refused` against such a bus is pinned in
    /// `notify::monitor`'s own `a_refused_bus_makes_run_on_return`, and this
    /// test is about what the supervisor does with that answer.
    #[tokio::test]
    async fn a_refused_monitor_is_not_restarted_until_enabled_is_toggled() {
        let engine = test_engine();
        let mut sup = NotifyMonitorSupervisor::new();

        sup.handle = Some(tokio::spawn(async {}));
        sup.refused
            .store(true, std::sync::atomic::Ordering::Release);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !sup.handle.as_ref().expect("just set").is_finished()
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        sup.reconcile(true, &engine);
        assert!(sup.handle.is_none(), "the ended task must be cleared");
        assert!(
            sup.refusal_latched,
            "a refusal must be latched, not treated as an unexplained death"
        );

        // Well past `NOTIFY_RESTART_BACKOFF`: a backoff is what an
        // *unexplained* death gets. A refusal gets nothing, ever, while
        // `enabled` stays true.
        tokio::time::sleep(NOTIFY_RESTART_BACKOFF + Duration::from_millis(200)).await;
        for _ in 0..5 {
            sup.reconcile(true, &engine);
        }
        assert!(
            sup.handle.is_none(),
            "a refused monitor must not be respawned, however many ticks pass: \
             every respawn is another connection, auth handshake and refused \
             BecomeMonitor against a bus that has already said no"
        );

        // Toggling `enabled` off and on is the one thing that asks again --
        // it is what a user who has just fixed their bus policy does, and
        // what the settings window's switch produces.
        sup.reconcile(false, &engine);
        assert!(!sup.refusal_latched, "disabling must clear the latch");
        sup.reconcile(true, &engine);
        assert!(
            sup.handle.is_some(),
            "toggling notifications.enabled off and on must retry"
        );
        assert!(
            !sup.refused.load(std::sync::atomic::Ordering::Acquire),
            "the fresh task's flag must not inherit the previous task's refusal"
        );

        sup.reconcile(false, &engine);
        engine.shutdown();
    }

    /// IMPORTANT 2: SIGTERM to exit must not gain the rewrite's ceiling.
    ///
    /// `main` used to let `rt` drop when the glib loop returned, and
    /// `Runtime::drop` waits -- with no bound of its own -- for every
    /// blocking task that has started. This milestone put a `ureq` request
    /// on that pool whose only bound is `reword::http_ceiling`, the
    /// configured deadline plus `reword::REWORD_HTTP_GRACE`. Measured with
    /// `drop(rt)` in place of the call below and one rewrite in flight
    /// against a provider that accepts and never answers: 9.73 s, which is
    /// what a `systemctl --user restart sayd` mid-rewrite sat for -- at the
    /// default deadline, and there is no longer any ceiling on how much
    /// worse a configured one could make it.
    ///
    /// The stuck task here stands in for that request, and sleeps the grace
    /// alone rather than a whole ceiling: it only has to be long enough that
    /// nothing shorter could tell a bounded shutdown from a lucky one, and
    /// the grace is already twenty times the value under test.
    #[test]
    fn a_stuck_blocking_task_cannot_hold_shutdown_past_the_grace() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.spawn_blocking(|| std::thread::sleep(crate::reword::REWORD_HTTP_GRACE));
        // `Runtime::drop` waits for blocking tasks that have *started*, so
        // the task has to have started for this to be measuring anything.
        std::thread::sleep(Duration::from_millis(100));

        let started = Instant::now();
        rt.shutdown_timeout(RUNTIME_SHUTDOWN_GRACE);
        let waited = started.elapsed();

        assert!(
            waited < RUNTIME_SHUTDOWN_GRACE * 2,
            "shutdown waited {waited:?} on a request whose answer §2 has already \
             dropped; the bound is {RUNTIME_SHUTDOWN_GRACE:?}"
        );
    }
}
