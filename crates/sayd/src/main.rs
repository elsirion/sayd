//! The sayd daemon.
//!
//! Runs resident, owning the engine on its own thread and serving
//! `sh.sayd.Sayd1` on the session bus. A second instance detects that the
//! well-known name is taken, forwards its arguments to the running daemon,
//! and exits -- so `sayd` is safe to put in a sway config that gets reloaded.

mod dbus;
mod kokoro_synth;
mod resample;
mod ring;
mod selection;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use sayd_core::audio::AudioSink;
use sayd_core::config::Config;
use sayd_core::engine::State;
use sayd_core::handle::EngineHandle;
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::zvariant::OwnedValue;

const BUS_NAME: &str = "sh.sayd.Sayd";
const OBJECT_PATH: &str = "/sh/sayd/Sayd";

/// How often the daemon publishes property changes.
///
/// Fast enough that a tray or MPRIS client feels live, slow enough that
/// `RemainingSeconds` ticking down does not flood the bus.
const PUBLISH_INTERVAL: Duration = Duration::from_millis(200);

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

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::process::ExitCode {
    let text: String = std::env::args().skip(1).collect::<Vec<_>>().join(" ");

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

    let synth = match kokoro_synth::KokoroSynthesizer::new(&models_dir(), &cfg) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to initialize synthesizer: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let sink = match open_sink() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to open audio output: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let engine = EngineHandle::spawn(cfg, Box::new(synth), sink);

    let iface = dbus::SaydIface::new(engine.clone());
    if let Err(e) = connection.object_server().at(OBJECT_PATH, iface).await {
        eprintln!("error: could not serve the interface: {e}");
        return std::process::ExitCode::FAILURE;
    }

    eprintln!("sayd: listening on {BUS_NAME} at {OBJECT_PATH}");

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
    // Reacquisition state for the recovery branch below: `next_recovery_attempt`
    // throttles retries while `State::Error` persists, and `recovery_failure_logged`
    // keeps a standing failure to one line instead of one every `PUBLISH_INTERVAL`.
    let mut next_recovery_attempt = Instant::now();
    let mut recovery_failure_logged = false;
    let mut ticker = tokio::time::interval(PUBLISH_INTERVAL);
    let mut sigterm =
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
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
                if now != last {
                    let ctx = iface_ref.signal_emitter();
                    let i = iface_ref.get().await;
                    // Emit only what changed; a client diffing every property
                    // on every tick would see RemainingSeconds churn forever.
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
                    if now.current_id != last.current_id {
                        let _ = i.current_id_changed(ctx).await;
                        let _ = i.current_text_changed(ctx).await;
                    }
                    if now.error != last.error {
                        let _ = i.error_changed(ctx).await;
                    }
                    last = now;
                }

                // Recover from *any* engine error by reacquiring the sink, not
                // just ones whose message happens to mention "device".
                //
                // `Engine::tick`'s two failure paths (a `take_error()` from the
                // sink, or a synth error) both set `state = Error` only after
                // clearing the queue and dropping `current` in the same step
                // (see sayd-core/src/engine.rs), and `submit`'s own
                // Error-setting branch only fires when nothing was already
                // playing or paused, so nothing legitimate is ever in flight
                // while `state == Error`. Handing the engine a fresh sink is
                // therefore always safe here, regardless of which cpal
                // `StreamError` variant (or unrelated synth failure) produced
                // the text in `error` -- cpal 0.17.1's `StreamInvalidated` and
                // `BufferUnderrun` messages don't contain "device" the way
                // `DeviceNotAvailable`'s does, so matching on the string missed
                // exactly the ordinary desktop hiccups (a PulseAudio restart,
                // an XRUN) this loop exists to recover from.
                if last.state == State::Error {
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
