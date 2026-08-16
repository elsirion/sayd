//! `say` -- control the sayd daemon.
//!
//! Deliberately depends on zbus and clap alone. Agent narration forks this
//! every 15-30 seconds and the caller never waits for playback, so startup
//! time matters more than sharing types with the daemon.

use std::collections::HashMap;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use zbus::zvariant::{OwnedValue, Str};

const BUS_NAME: &str = "sh.sayd.Sayd";
const OBJECT_PATH: &str = "/sh/sayd/Sayd";
const IFACE: &str = "sh.sayd.Sayd1";

/// How long any single D-Bus interaction -- connecting to the session bus,
/// resolving the daemon's name, or an ordinary method/property call -- may
/// block before this instance gives up and reports a timeout instead of
/// hanging.
///
/// A couple of seconds is generous for a local session-bus round trip, and
/// this binary is forked every 15-30 seconds by agent narration, so failing
/// fast matters more than tolerating a slow daemon.
///
/// **One interaction is deliberately not bounded by this: a `Say` carrying
/// `reword`.** See [`call_submission`]. `--reword` is answered inline --
/// `Say` returns the utterance id and the daemon allocates that id from the
/// text, so it cannot hand one back and rewrite afterwards -- so such a call
/// legitimately takes as long as the daemon's `reword.timeout_ms`, and that
/// setting has no ceiling: someone running a local model may have set it to
/// thirty seconds. This binary imports no crate of this workspace and reads
/// no config, so it cannot know the number. Bounding the call at any
/// constant would mean reporting a daemon that is working exactly as
/// configured as not responding.
///
/// What that costs is smaller than it sounds, because this constant still
/// bounds the two interactions that come first. **A daemon that is not
/// running -- the common failure by a wide margin -- still fails in 3 s**,
/// at name resolution, before any method is called. What is left is a daemon
/// that is up, owns its name, and is wedged part-way through a `Say`: rarer,
/// and no longer something this binary can put a number on.
///
/// End to end, `--reword`'s inline wait is exercised by
/// `sayd::dbus::tests::a_reword_against_a_silent_provider_answers_with_a_spoken_utterance`.
const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Parser, Debug)]
#[command(
    name = "say",
    version,
    about = "Speak text through the sayd daemon",
    long_about = "Speak text through the sayd daemon.\n\n\
                  With no subcommand, the arguments are spoken as text. A word \
                  that happens to match a subcommand name is treated as that \
                  subcommand -- use `say -- stop` to speak it instead."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Queueing policy for this submission -- how it interacts with
    /// whatever is already playing or queued. Meaningless outside a
    /// submission (bare text, `selection`, `clipboard`); ignored elsewhere.
    /// Must come before the text/subcommand it applies to.
    #[arg(long, global = true, value_enum)]
    policy: Option<PolicyArg>,

    /// Voice to speak this submission with, overriding the daemon's
    /// default. Meaningless outside a submission; ignored elsewhere. Must
    /// come before the text/subcommand it applies to.
    #[arg(long, global = true)]
    voice: Option<String>,

    /// Playback speed multiplier for this submission, overriding the
    /// daemon's default. Meaningless outside a submission; ignored
    /// elsewhere. Must come before the text/subcommand it applies to.
    #[arg(long, global = true)]
    speed: Option<f64>,

    /// Rewrite this submission into something written for the ear before
    /// speaking it -- "Alice: dinner?" becomes "Alice is asking about
    /// dinner". Needs a configured `[reword]` endpoint and a daemon built
    /// with `--features reword`; without either, the text is spoken as
    /// written. Does *not* require `[reword] enabled = true`, which only
    /// governs whether notifications are rewritten automatically.
    /// Meaningless outside a submission; ignored elsewhere. Must come
    /// before the text/subcommand it applies to.
    ///
    /// There is no `--no-reword`: nothing makes CLI submissions rewrite by
    /// default, so there is nothing to negate.
    #[arg(long, global = true)]
    reword: bool,

    /// Text to speak. Use `--` first if it begins with a subcommand name.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    text: Vec<String>,
}

/// The `policy` values the daemon's `SayOpts` understands. Validated by
/// clap rather than passed through as a bare string, so a typo is a clap
/// error naming the bad value, not a silent fall-back to the default
/// policy.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum PolicyArg {
    Enqueue,
    Interrupt,
    Replace,
    Front,
}

impl PolicyArg {
    /// The wire string the daemon's `say_opts_from` matches on.
    fn as_wire_str(self) -> &'static str {
        match self {
            PolicyArg::Enqueue => "enqueue",
            PolicyArg::Interrupt => "interrupt",
            PolicyArg::Replace => "replace",
            PolicyArg::Front => "front",
        }
    }
}

#[derive(Subcommand, Debug)]
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

    // Built once: `--policy`/`--voice`/`--speed`/`--reword` matter only to
    // the three submission paths (bare text, `selection`, `clipboard`), all
    // of which share this dict. Every other command ignores it.
    let opts = say_opts(&cli);
    // The three submission methods below are the only ones the daemon may
    // answer late by design, and only when `--reword` asked it to. See
    // [`call_submission`].
    let reword = cli.reword;
    let result = match cli.command {
        Some(Command::Selection) => {
            call_submission(reword, proxy.call_method("SaySelection", &(opts,))).await
        }
        Some(Command::Clipboard) => {
            call_submission(reword, proxy.call_method("SayClipboard", &(opts,))).await
        }
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
            call_submission(reword, proxy.call_method("Say", &(text, opts))).await
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

/// Build the D-Bus `opts` dict for a submission from the parsed CLI's
/// `--policy`/`--voice`/`--speed`/`--reword`. Only keys the caller set are
/// present -- the daemon's own `say_opts_from` already treats an absent key
/// as "use the default," so there is no reason to send one explicitly.
fn say_opts(cli: &Cli) -> HashMap<String, OwnedValue> {
    let mut opts = HashMap::new();
    if let Some(policy) = cli.policy {
        opts.insert(
            "policy".to_string(),
            OwnedValue::from(Str::from(policy.as_wire_str())),
        );
    }
    if let Some(voice) = &cli.voice {
        opts.insert(
            "voice".to_string(),
            OwnedValue::from(Str::from(voice.as_str())),
        );
    }
    if let Some(speed) = cli.speed {
        opts.insert("speed".to_string(), OwnedValue::from(speed));
    }
    // Only when asked. A daemon that predates the key ignores it, and a
    // `say hello` without the flag is byte-identical on the wire to what it
    // was before the flag existed.
    if cli.reword {
        opts.insert("reword".to_string(), OwnedValue::from(true));
    }
    opts
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

/// Await one of the three submission calls -- `Say`, `SaySelection`,
/// `SayClipboard` -- bounded by [`TIMEOUT`] as everything else is, *unless*
/// this submission carries `--reword`, in which case it is not bounded here
/// at all.
///
/// The daemon answers a rewording submission only once the rewrite has
/// finished or its own `reword.timeout_ms` has elapsed, and that setting has
/// no ceiling (`sayd_core::config::REWORD_TIMEOUT_MIN_MS` says why). The
/// deadline that ends this wait is therefore the daemon's, deliberately:
/// it is the only party that knows the number. See [`TIMEOUT`] for what
/// that costs and what still fails fast.
///
/// All three, not only `Say`: `SaySelection` and `SayClipboard` reach the
/// same inline rewrite through `SaydIface::say_read`, so `say --reword
/// selection` waits on exactly the same deadline `say --reword "..."` does.
///
/// "Not bounded here" is meant literally, and it is worth stating because
/// the obvious way to get it wrong is to bound it somewhere else by
/// accident. zbus applies a client-side reply timeout only when the
/// connection was built with one (`connection::Builder::method_timeout`);
/// `Connection::session()` leaves it unset, so nothing under this `.await`
/// caps the wait -- checked against zbus 5.19, whose `Connection::call_method`
/// wraps the reply future in a timeout only for `Some(_)`. Setting one for
/// the sake of a number would put back exactly the ceiling this milestone
/// removed, one layer down and invisible.
async fn call_submission<F, T>(reword: bool, fut: F) -> Result<(), CallError>
where
    F: std::future::Future<Output = zbus::Result<T>>,
{
    if reword {
        return fut.await.map(|_| ()).map_err(CallError::Dbus);
    }
    call(fut).await.map(|_| ())
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

/// Read every `sh.sayd.Sayd1` property in a single `GetAll`, rather than one
/// `Get` per property.
///
/// The daemon's `Snapshot` (see `sayd-core::engine`) is assembled
/// atomically, but the old per-property reads each ran as its own D-Bus
/// round trip; a state transition landing between two of those round trips
/// produced genuinely inconsistent output -- e.g. `State="speaking"`
/// alongside `CurrentId=0`, or vice versa. A single `GetAll` reads one
/// snapshot's worth of properties in one reply, so the tearing is gone by
/// construction.
async fn status(proxy: &zbus::Proxy<'_>, json: bool) -> Result<(), CallError> {
    let props = call(zbus::fdo::PropertiesProxy::new(
        proxy.connection(),
        BUS_NAME,
        OBJECT_PATH,
    ))
    .await?;

    // `IFACE` is a fixed, already-valid interface name -- the same constant
    // used to build `proxy` itself in `main` -- so this cannot fail on any
    // real invocation.
    let iface = zbus::names::InterfaceName::from_static_str(IFACE)
        .expect("IFACE is a valid interface name");
    let map: HashMap<String, OwnedValue> =
        call(async { props.get_all(iface).await.map_err(zbus::Error::from) }).await?;

    let state = prop_string(&map, "State");
    let muted = prop_bool(&map, "Muted", false);
    let voice = prop_string(&map, "Voice");
    let speed = prop_f64(&map, "Speed", 1.0);
    let queue = prop_u32(&map, "QueueLength", 0);
    let remaining = prop_f64(&map, "RemainingSeconds", 0.0);
    let current = prop_string(&map, "CurrentText");
    let current_id = prop_u32(&map, "CurrentId", 0);
    let error = prop_string(&map, "Error");

    if json {
        // Hand-built so this crate need not depend on serde.
        println!(
            "{{\"state\":\"{}\",\"muted\":{},\"voice\":\"{}\",\"speed\":{},\
             \"queue_length\":{},\"remaining_seconds\":{:.2},\
             \"current_text\":\"{}\",\"current_id\":{},\"error\":\"{}\"}}",
            escape(&state),
            muted,
            escape(&voice),
            speed,
            queue,
            remaining,
            escape(&current),
            current_id,
            escape(&error),
        );
    } else {
        println!("state:     {state}{}", if muted { " (muted)" } else { "" });
        println!("voice:     {voice} at {speed:.2}x");
        println!("queue:     {queue} pending, {remaining:.1}s remaining");
        if current_id != 0 {
            println!("current:   id {current_id}");
        }
        if !current.is_empty() {
            println!("speaking:  {current}");
        }
        if !error.is_empty() {
            println!("error:     {error}");
        }
    }
    Ok(())
}

/// Extract a `String` property from a `GetAll` reply, defaulting to empty
/// if the key is missing or holds an unexpected type -- a daemon reply
/// should never make `status` fail on one odd field when the rest parsed
/// fine.
fn prop_string(map: &HashMap<String, OwnedValue>, key: &str) -> String {
    map.get(key)
        .and_then(|v| String::try_from(v.clone()).ok())
        .unwrap_or_default()
}

fn prop_bool(map: &HashMap<String, OwnedValue>, key: &str, default: bool) -> bool {
    map.get(key)
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(default)
}

fn prop_f64(map: &HashMap<String, OwnedValue>, key: &str, default: f64) -> f64 {
    map.get(key)
        .and_then(|v| f64::try_from(v).ok())
        .unwrap_or(default)
}

fn prop_u32(map: &HashMap<String, OwnedValue>, key: &str, default: u32) -> u32 {
    map.get(key)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(default)
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
    fn speed_before_bare_text_is_parsed_and_the_rest_is_spoken() {
        let c = Cli::try_parse_from(["say", "--speed", "1.5", "hello", "world"]).expect("parses");
        assert!(c.command.is_none());
        assert_eq!(c.speed, Some(1.5));
        assert_eq!(c.text.join(" "), "hello world");
    }

    #[test]
    fn voice_and_policy_before_a_subcommand_are_both_parsed() {
        let c = Cli::try_parse_from([
            "say",
            "--voice",
            "bf_emma",
            "--policy",
            "replace",
            "selection",
        ])
        .expect("parses");
        assert!(matches!(c.command, Some(Command::Selection)));
        assert_eq!(c.voice.as_deref(), Some("bf_emma"));
        assert!(matches!(c.policy, Some(PolicyArg::Replace)));
    }

    #[test]
    fn options_after_a_subcommand_name_are_also_parsed() {
        // `global = true` makes `--voice`/`--policy`/`--speed` valid on
        // either side of a subcommand name, unlike bare text (see the next
        // test): `Selection`/`Clipboard` have no trailing var-arg positional
        // to swallow the flag as a word instead.
        let c = Cli::try_parse_from(["say", "clipboard", "--speed", "0.8"]).expect("parses");
        assert!(matches!(c.command, Some(Command::Clipboard)));
        assert_eq!(c.speed, Some(0.8));
    }

    #[test]
    fn an_option_after_bare_text_has_begun_is_spoken_rather_than_parsed() {
        // Documents the one grammar limitation: `trailing_var_arg` +
        // `allow_hyphen_values` means once the bare-text positional starts
        // consuming, nothing after it is recognised as a flag any more --
        // so `--speed` must precede the text it applies to, not follow it.
        let c = Cli::try_parse_from(["say", "hello", "--speed", "1.5"]).expect("parses");
        assert!(c.command.is_none());
        assert_eq!(c.speed, None);
        assert_eq!(c.text.join(" "), "hello --speed 1.5");
    }

    #[test]
    fn an_invalid_policy_is_rejected_by_clap() {
        let err = Cli::try_parse_from(["say", "--policy", "nonsense", "hello"])
            .expect_err("an unknown policy value must not silently parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    /// `--reword` is global, like `--policy`/`--voice`/`--speed`, so it
    /// works on bare text and on the two reading subcommands alike --
    /// `say --reword selection` needs no code of its own, because
    /// selection and clipboard reads go through the same submission path.
    #[test]
    fn reword_is_global_and_reaches_every_submission() {
        let c = Cli::try_parse_from(["say", "--reword", "hello", "world"]).expect("parses");
        assert!(c.reword);
        assert_eq!(c.text.join(" "), "hello world");
        assert!(say_opts(&c).contains_key("reword"));

        let c = Cli::try_parse_from(["say", "--reword", "selection"]).expect("parses");
        assert!(c.reword);
        assert!(matches!(c.command, Some(Command::Selection)));

        let c = Cli::try_parse_from(["say", "clipboard", "--reword"]).expect("parses");
        assert!(c.reword);
    }

    /// Only keys the caller actually set are sent, so a plain `say hello`
    /// against an old daemon is byte-identical on the wire to what it was
    /// before this flag existed.
    #[test]
    fn no_reword_key_is_sent_unless_it_was_asked_for() {
        let c = Cli::try_parse_from(["say", "hello"]).expect("parses");
        assert!(!c.reword);
        assert!(!say_opts(&c).contains_key("reword"));
    }

    /// The key the daemon's `wants_reword` reads is a *boolean* `reword`.
    /// The two are in different binaries with no shared type between them,
    /// so the wire contract is worth stating on both sides: a `Str` here
    /// would be silently ignored by the daemon and the flag would do
    /// nothing at all.
    #[test]
    fn the_reword_key_goes_on_the_wire_as_a_boolean_true() {
        let c = Cli::try_parse_from(["say", "--reword", "hello"]).expect("parses");
        let opts = say_opts(&c);
        let v = opts.get("reword").expect("the key is present");
        assert!(
            v.downcast_ref::<bool>()
                .expect("a boolean variant, which is what the daemon downcasts to"),
            "the flag sends `true`, and the daemon reads it with \
             `downcast_ref::<bool>`"
        );
    }

    #[test]
    fn no_arguments_parses_but_carries_nothing_to_do() {
        let c = Cli::try_parse_from(["say"]).expect("parses");
        assert!(c.command.is_none() && c.text.is_empty());
    }
}
