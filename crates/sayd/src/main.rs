//! M1 entry point: speak the text given on the command line.
//!
//! No D-Bus, no tray, no UI -- those are M2-M4. This exists to prove the
//! engine, the real synthesizer and the real audio sink work together.

mod kokoro_synth;
mod ring;

use std::path::PathBuf;
use std::time::Duration;

use sayd_core::config::Config;
use sayd_core::engine::{Engine, SayOpts, State};

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

fn main() -> std::process::ExitCode {
    let text: String = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    if text.trim().is_empty() {
        eprintln!("usage: sayd <text to speak>");
        return std::process::ExitCode::from(2);
    }

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
    let sink = match ring::RingSink::new(sayd_core::synth::SAMPLE_RATE) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to open audio output: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    // M1 does not resample: if the device refused our rate, warn rather
    // than silently play audio at the wrong pitch/speed.
    if sink.device_sample_rate != sayd_core::synth::SAMPLE_RATE {
        eprintln!(
            "warning: audio device uses {} Hz, not the synthesizer's {} Hz; \
             playback will be mis-pitched (no resampler in M1)",
            sink.device_sample_rate,
            sayd_core::synth::SAMPLE_RATE
        );
    }

    let mut engine = Engine::new(cfg, Box::new(synth), Box::new(sink));

    // `submit`, not `Command::Say`: a rejected submission (e.g. text longer
    // than `max_chars`) must be reported here on stderr with a non-zero
    // exit, rather than only being discoverable by polling a snapshot field.
    if let Err(e) = engine.submit(text, SayOpts::default()) {
        eprintln!("error: {e}");
        return std::process::ExitCode::FAILURE;
    }

    loop {
        // Called unconditionally, even while idle: `tick`'s idle branch is
        // what runs `maybe_unload`, so skipping it while idle would mean the
        // idle-unload policy never fires.
        engine.tick();
        let s = engine.snapshot();
        match s.state {
            State::Error => {
                eprintln!("error: {}", s.error.unwrap_or_default());
                return std::process::ExitCode::FAILURE;
            }
            State::Idle => break,
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
    }

    // Let the ring drain before the stream is dropped.
    std::thread::sleep(Duration::from_millis(500));
    std::process::ExitCode::SUCCESS
}
