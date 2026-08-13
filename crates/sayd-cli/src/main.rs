//! `say` -- control the sayd daemon.
//!
//! Deliberately depends on zbus and clap alone. Agent narration forks this
//! every 15-30 seconds and the caller never waits for playback, so startup
//! time matters more than sharing types with the daemon.

use std::collections::HashMap;

use clap::{Parser, Subcommand};
use zbus::zvariant::OwnedValue;

const BUS_NAME: &str = "sh.sayd.Sayd";
const OBJECT_PATH: &str = "/sh/sayd/Sayd";
const IFACE: &str = "sh.sayd.Sayd1";

#[derive(Parser)]
#[command(
    name = "say",
    version,
    about = "Speak text through the sayd daemon",
    long_about = "Speak text through the sayd daemon.\n\n\
                  With no subcommand, the arguments are spoken as text. A word \
                  that happens to match a subcommand name is treated as that \
                  subcommand -- use `say -- stop` to speak it instead.",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Text to speak. Use `--` first if it begins with a subcommand name.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    text: Vec<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Speak the PRIMARY selection (whatever is selected with the mouse)
    Selection,
    /// Speak the clipboard
    Clipboard,
    /// Pause playback
    Pause,
    /// Resume playback
    Resume,
    /// Toggle between pause and resume
    PlayPause,
    /// Stop the current utterance and clear the queue
    Stop,
    /// Skip to the next queued utterance
    Next,
    /// Skip to the next sentence
    Skip,
    /// Drop everything pending, letting the current utterance finish
    Clear,
    /// Accept submissions but discard them
    Mute,
    /// Stop discarding submissions
    Unmute,
    /// Report what the daemon is doing
    Status {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    if cli.command.is_none() && cli.text.is_empty() {
        eprintln!("say: nothing to do; try `say --help`");
        return std::process::ExitCode::from(2);
    }

    let conn = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("say: cannot reach the session bus: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let proxy = match zbus::Proxy::new(&conn, BUS_NAME, OBJECT_PATH, IFACE).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("say: sayd does not appear to be running ({e})");
            return std::process::ExitCode::FAILURE;
        }
    };

    let empty: HashMap<String, OwnedValue> = HashMap::new();
    let result = match cli.command {
        Some(Command::Selection) => proxy.call_method("SaySelection", &(empty,)).await.map(|_| ()),
        Some(Command::Clipboard) => proxy.call_method("SayClipboard", &(empty,)).await.map(|_| ()),
        Some(Command::Pause) => proxy.call_method("Pause", &()).await.map(|_| ()),
        Some(Command::Resume) => proxy.call_method("Resume", &()).await.map(|_| ()),
        Some(Command::PlayPause) => proxy.call_method("PlayPause", &()).await.map(|_| ()),
        Some(Command::Stop) => proxy.call_method("Stop", &()).await.map(|_| ()),
        Some(Command::Next) => proxy.call_method("Next", &()).await.map(|_| ()),
        Some(Command::Skip) => proxy.call_method("SkipSentence", &()).await.map(|_| ()),
        Some(Command::Clear) => proxy.call_method("ClearQueue", &()).await.map(|_| ()),
        Some(Command::Mute) => proxy.call_method("SetMuted", &(true,)).await.map(|_| ()),
        Some(Command::Unmute) => proxy.call_method("SetMuted", &(false,)).await.map(|_| ()),
        Some(Command::Status { json }) => {
            return match status(&proxy, json).await {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("say: {e}");
                    std::process::ExitCode::FAILURE
                }
            };
        }
        None => {
            let text = cli.text.join(" ");
            proxy.call_method("Say", &(text, empty)).await.map(|_| ())
        }
    };

    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("say: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn status(proxy: &zbus::Proxy<'_>, json: bool) -> Result<(), String> {
    let state: String = proxy
        .get_property("State")
        .await
        .map_err(|e| format!("could not read State: {e}"))?;
    let muted: bool = proxy.get_property("Muted").await.unwrap_or(false);
    let voice: String = proxy.get_property("Voice").await.unwrap_or_default();
    let speed: f64 = proxy.get_property("Speed").await.unwrap_or(1.0);
    let queue: u32 = proxy.get_property("QueueLength").await.unwrap_or(0);
    let remaining: f64 = proxy.get_property("RemainingSeconds").await.unwrap_or(0.0);
    let current: String = proxy.get_property("CurrentText").await.unwrap_or_default();
    let error: String = proxy.get_property("Error").await.unwrap_or_default();

    if json {
        // Hand-built so this crate need not depend on serde.
        println!(
            "{{\"state\":\"{}\",\"muted\":{},\"voice\":\"{}\",\"speed\":{},\
             \"queue_length\":{},\"remaining_seconds\":{:.2},\
             \"current_text\":\"{}\",\"error\":\"{}\"}}",
            escape(&state),
            muted,
            escape(&voice),
            speed,
            queue,
            remaining,
            escape(&current),
            escape(&error),
        );
    } else {
        println!("state:     {state}{}", if muted { " (muted)" } else { "" });
        println!("voice:     {voice} at {speed:.2}x");
        println!("queue:     {queue} pending, {remaining:.1}s remaining");
        if !current.is_empty() {
            println!("speaking:  {current}");
        }
        if !error.is_empty() {
            println!("error:     {error}");
        }
    }
    Ok(())
}

/// Minimal JSON string escaping, so `--json` output is always parseable.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn bare_words_are_treated_as_text() {
        let c = Cli::try_parse_from(["say", "hello", "there"]).expect("parses");
        assert!(c.command.is_none());
        assert_eq!(c.text.join(" "), "hello there");
    }

    #[test]
    fn a_subcommand_name_wins_over_text() {
        let c = Cli::try_parse_from(["say", "stop"]).expect("parses");
        assert!(matches!(c.command, Some(Command::Stop)));
        assert!(c.text.is_empty(), "a recognised verb must not also be spoken");
    }

    #[test]
    fn double_dash_forces_a_subcommand_name_to_be_spoken() {
        let c = Cli::try_parse_from(["say", "--", "stop"]).expect("parses");
        assert!(c.command.is_none());
        assert_eq!(c.text.join(" "), "stop");
    }

    #[test]
    fn text_beginning_with_a_hyphen_is_accepted_after_a_double_dash() {
        let c = Cli::try_parse_from(["say", "--", "-40 degrees"]).expect("parses");
        assert_eq!(c.text.join(" "), "-40 degrees");
    }

    #[test]
    fn status_takes_a_json_flag() {
        let c = Cli::try_parse_from(["say", "status", "--json"]).expect("parses");
        assert!(matches!(c.command, Some(Command::Status { json: true })));
    }

    #[test]
    fn no_arguments_parses_but_carries_nothing_to_do() {
        let c = Cli::try_parse_from(["say"]).expect("parses");
        assert!(c.command.is_none() && c.text.is_empty());
    }
}
