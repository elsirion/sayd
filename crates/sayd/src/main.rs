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
mod resample;
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
/// zbus's own default method-call timeout is close to 25 seconds, which
/// reads to a user as "sayd is frozen." A couple of seconds is generous for
/// a local session-bus round trip.
const FORWARD_CALL_TIMEOUT: Duration = Duration::from_secs(3);

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
    let (cfg, cfg_err) = Config::load();
    if let Some(e) = cfg_err {
        eprintln!("warning: {e}; using defaults");
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
    // Held for the life of the process: dropping the watcher stops the
    // watch, silently.
    let _config_watcher = match config_watch::spawn(store.clone()) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!("warning: {e}; config changes will need a restart");
            None
        }
    };

    let iface = dbus::SaydIface::new(engine.clone());
    if let Err(e) = connection.object_server().at(OBJECT_PATH, iface).await {
        eprintln!("error: could not serve the interface: {e}");
        return std::process::ExitCode::FAILURE;
    }

    eprintln!("sayd: listening on {BUS_NAME} at {OBJECT_PATH}");

    // A tray registration failure must not be fatal: a bare sway config
    // without waybar has no StatusNotifierWatcher running at all, and the
    // daemon is still useful serving just the control interface. Log once
    // and carry on rather than exit.
    let tray_handle = match tray::spawn(engine.clone()).await {
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
    let mpris_handle = match mpris::spawn(engine.clone()).await {
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
                if let Some(h) = tray_handle.as_ref() {
                    if now != tray_last_sent && now_instant >= next_tray_attempt {
                        let s = now.clone();
                        match tokio::time::timeout(
                            FANOUT_TIMEOUT,
                            h.update(move |t| t.set_snapshot(s)),
                        )
                        .await
                        {
                            Ok(_) => {
                                tray_last_sent = now.clone();
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
    code
}
