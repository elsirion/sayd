//! `say` -- control the sayd daemon.
//!
//! Deliberately depends on zbus and clap alone. Agent narration forks this
//! every 15-30 seconds and the caller never waits for playback, so startup
//! time matters more than sharing types with the daemon.

use std::collections::HashMap;
use std::time::Duration;

use clap::{Parser, Subcommand};
use zbus::zvariant::OwnedValue;

const BUS_NAME: &str = "sh.sayd.Sayd";
const OBJECT_PATH: &str = "/sh/sayd/Sayd";
const IFACE: &str = "sh.sayd.Sayd1";

/// How long any single D-Bus interaction -- connecting to the session bus,
/// resolving the daemon's name, or a method/property call -- may block
/// before this instance gives up and reports a timeout instead of hanging.
///
/// zbus's own default method-call timeout is close to 25 seconds, which
/// reads to a caller as "sayd is frozen." A couple of seconds is generous
/// for a local session-bus round trip, and this binary is forked every
/// 15-30 seconds by agent narration, so failing fast matters more than
/// tolerating a slow daemon.
const TIMEOUT: Duration = Duration::from_secs(3);

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

    let conn = match tokio::time::timeout(TIMEOUT, zbus::Connection::session()).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            eprintln!("say: cannot reach the session bus: {e}");
            return std::process::ExitCode::FAILURE;
        }
        Err(_) => {
            eprintln!("say: sayd is not responding (timed out reaching the session bus)");
            return std::process::ExitCode::FAILURE;
        }
    };
    let proxy = match tokio::time::timeout(
        TIMEOUT,
        zbus::Proxy::new(&conn, BUS_NAME, OBJECT_PATH, IFACE),
    )
    .await
    {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            eprintln!("say: sayd does not appear to be running ({e})");
            return std::process::ExitCode::FAILURE;
        }
        Err(_) => {
            eprintln!("say: sayd is not responding");
            return std::process::ExitCode::FAILURE;
        }
    };

    let empty: HashMap<String, OwnedValue> = HashMap::new();
    let result = match cli.command {
        Some(Command::Selection) => call(proxy.call_method("SaySelection", &(empty,)))
            .await
            .map(|_| ()),
        Some(Command::Clipboard) => call(proxy.call_method("SayClipboard", &(empty,)))
            .await
            .map(|_| ()),
        Some(Command::Pause) => call(proxy.call_method("Pause", &())).await.map(|_| ()),
        Some(Command::Resume) => call(proxy.call_method("Resume", &())).await.map(|_| ()),
        Some(Command::PlayPause) => call(proxy.call_method("PlayPause", &())).await.map(|_| ()),
        Some(Command::Stop) => call(proxy.call_method("Stop", &())).await.map(|_| ()),
        Some(Command::Next) => call(proxy.call_method("Next", &())).await.map(|_| ()),
        Some(Command::Skip) => call(proxy.call_method("SkipSentence", &()))
            .await
            .map(|_| ()),
        Some(Command::Clear) => call(proxy.call_method("ClearQueue", &())).await.map(|_| ()),
        Some(Command::Mute) => call(proxy.call_method("SetMuted", &(true,)))
            .await
            .map(|_| ()),
        Some(Command::Unmute) => call(proxy.call_method("SetMuted", &(false,)))
            .await
            .map(|_| ()),
        Some(Command::Status { json }) => {
            return match status(&proxy, json).await {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(e) => {
                    print_error(&e);
                    std::process::ExitCode::FAILURE
                }
            };
        }
        None => {
            let text = cli.text.join(" ");
            call(proxy.call_method("Say", &(text, empty)))
                .await
                .map(|_| ())
        }
    };

    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            print_error(&e);
            std::process::ExitCode::FAILURE
        }
    }
}

/// Await a single D-Bus call, bounding it by [`TIMEOUT`] so a wedged daemon
/// (up but not answering -- mid-restart, or stuck in its own recovery loop)
/// fails fast instead of hanging the caller forever.
async fn call<F, T>(fut: F) -> Result<T, CallError>
where
    F: std::future::Future<Output = zbus::Result<T>>,
{
    match tokio::time::timeout(TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(CallError::Dbus(e)),
        Err(_) => Err(CallError::Timeout),
    }
}

/// Everything that can go wrong making a call once a proxy exists: either
/// the daemon didn't answer in time, or it answered with a D-Bus error.
enum CallError {
    Timeout,
    Dbus(zbus::Error),
}

/// Print a `CallError` the way a person, not a D-Bus client, wants to read
/// it: never a raw `org.freedesktop.DBus.Error.*` wire type on the first
/// line.
fn print_error(e: &CallError) {
    match e {
        CallError::Timeout => eprintln!("say: sayd is not responding"),
        CallError::Dbus(err) => {
            let (human, raw) = describe_dbus_error(err);
            eprintln!("say: {human}");
            if let Some(raw) = raw {
                eprintln!("say: ({raw})");
            }
        }
    }
}

/// Turn a `zbus::Error` into a human-first message, plus optional raw detail
/// worth keeping around.
///
/// D-Bus error replies (`Error::MethodError`) carry a wire-level error name
/// like `org.freedesktop.DBus.Error.ServiceUnknown` alongside a
/// human-written description. Printing the name is the bug this exists to
/// avoid: it reads as an internal detail, not something a person can act on.
fn describe_dbus_error(e: &zbus::Error) -> (String, Option<String>) {
    if let zbus::Error::MethodError(name, detail, _) = e {
        let name = name.as_str();
        if name == "org.freedesktop.DBus.Error.ServiceUnknown"
            || name == "org.freedesktop.DBus.Error.NameHasNoOwner"
        {
            // No process currently owns the bus name: sayd is not running.
            let raw = detail.as_ref().map(|d| format!("{name}: {d}"));
            return ("sayd is not running".to_string(), raw);
        }
        // Any other method error (e.g. a rejected submission) is already
        // written for humans by whoever raised it -- the daemon, in
        // practice. Drop the wire-level error-name prefix and print just
        // that.
        let human = detail.clone().unwrap_or_else(|| name.to_string());
        return (human, None);
    }
    (e.to_string(), None)
}

async fn status(proxy: &zbus::Proxy<'_>, json: bool) -> Result<(), CallError> {
    let state: String = call(proxy.get_property("State")).await?;
    let muted: bool = call(proxy.get_property("Muted")).await.unwrap_or(false);
    let voice: String = call(proxy.get_property("Voice")).await.unwrap_or_default();
    let speed: f64 = call(proxy.get_property("Speed")).await.unwrap_or(1.0);
    let queue: u32 = call(proxy.get_property("QueueLength")).await.unwrap_or(0);
    let remaining: f64 = call(proxy.get_property("RemainingSeconds"))
        .await
        .unwrap_or(0.0);
    let current: String = call(proxy.get_property("CurrentText"))
        .await
        .unwrap_or_default();
    let error: String = call(proxy.get_property("Error")).await.unwrap_or_default();

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
        assert!(
            c.text.is_empty(),
            "a recognised verb must not also be spoken"
        );
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
