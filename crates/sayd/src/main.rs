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
use std::time::Duration;

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
            return match proxy.call_method("Say", &args).await {
                Ok(_) => std::process::ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
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

                // Recover from a device failure by reacquiring the sink.
                if last.state == State::Error
                    && last.error.as_deref().unwrap_or("").contains("device")
                {
                    if let Ok(s) = open_sink() {
                        eprintln!("info: audio device reacquired");
                        engine.send(sayd_core::engine::Command::Stop);
                        engine.replace_sink(s);
                    }
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
