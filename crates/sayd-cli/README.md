# sayd-cli

`say`, the control CLI for the [sayd](https://crates.io/crates/sayd) text-to-speech
daemon. It talks to `sh.sayd.Sayd1` over the session bus and does nothing else --
no engine, no audio, no phonemizer.

```
say "text"        say selection      say clipboard
say pause         say resume         say stop
say next          say skip           say clear
say mute          say unmute         say status [--json]
```

With no subcommand, the arguments are spoken as text:

```sh
say hello there
```

A word that happens to match a subcommand name is treated as that subcommand --
`say stop` stops the daemon rather than speaking the word "stop". Use `--` to
speak it anyway:

```sh
say -- stop
```

`say status --json` prints machine-readable state for scripts and status bars.

## Why this crate exists

`say` depends on `zbus` and `clap` alone -- not `sayd-core`, not `sayd-kokoro`,
not `sayd-g2p`, not `cpal`, not `ort`. Agent narration and shell scripts fork
this binary every 15-30 seconds, so startup time is a feature: it has no
inference stack to link and starts in about a millisecond. Wire types are
duplicated as plain D-Bus signatures rather than shared with the daemon; that
duplication is deliberate.

## Requirements

Needs the [`sayd`](https://crates.io/crates/sayd) daemon running and reachable
on the session bus. Without it, every command fails fast with a message naming
the problem rather than hanging.

Part of [sayd](https://crates.io/crates/sayd), a local speech daemon for Wayland.
