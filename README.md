# sayd

Local text-to-speech for sway/Wayland. Select text, press a key, hear it.
Kokoro-82M runs locally via ONNX Runtime with the misaki-en G2P frontend --
no network access at runtime, nothing leaves your machine.

`sayd` is the resident daemon: it owns the speech engine and the audio
device, and serves the `sh.sayd.Sayd1` interface on the session bus. `say`
is the control CLI that drives it -- speak text, speak the selection, pause,
skip, ask for status.

## Build

    nix develop
    cargo build --release

Put `target/release/sayd` and `target/release/say` on `$PATH`.

### Native dependencies

Both `sayd-kokoro` (ONNX Runtime, loaded at runtime) and `sayd-g2p`
(espeak-ng, linked at build time) need native libraries this repository does
not ship. `nix develop` sets up everything needed to build and run under Nix;
building elsewhere means following
[`crates/sayd-kokoro/README.md`](crates/sayd-kokoro/README.md) and
[`crates/sayd-g2p/README.md`](crates/sayd-g2p/README.md) instead.

## Models

    ./scripts/fetch-models.sh

Downloads Kokoro-82M ONNX weights and voice packs into `models/`. `sayd`
looks for them in `$XDG_DATA_HOME/sayd/models` (falling back to
`~/.local/share/sayd/models`), or in `./models` if neither exists. Set
`SAYD_MODELS_DIR` to point it somewhere else entirely.

## sway setup

Add [`docs/sway.conf.example`](docs/sway.conf.example) to
`~/.config/sway/config`:

    exec sayd

    bindsym $mod+Shift+s exec say selection
    bindsym $mod+Shift+v exec say clipboard
    bindsym $mod+Shift+x exec say stop
    bindsym $mod+Shift+p exec say play-pause

`sayd` reads the PRIMARY selection itself through `wlr-data-control`, so
there is no `$(...)` anywhere in the keybinds -- no selected text ever
passes through a shell, and nothing can mangle the quoting.

`sayd` is single-instance: if the bus name is already taken, a second
`sayd` invocation forwards its command-line text (if any) to the running
daemon and exits instead of erroring. That is what makes `exec sayd` safe
to leave in a sway config that gets reloaded -- reloading does not spawn a
second daemon or kill the one already running.

Prefer systemd to manage the daemon's lifetime instead? See
[`docs/sh.sayd.Sayd.service.example`](docs/sh.sayd.Sayd.service.example).

## `say`, the control CLI

    say "text"        say selection      say clipboard
    say pause         say resume         say play-pause
    say stop          say next           say skip
    say clear         say mute           say unmute
    say status [--json]

With no subcommand, the arguments are spoken as text:

    say hello there

A word that happens to match a subcommand name is treated as that
subcommand -- `say stop` stops the daemon rather than speaking the word
"stop". Use `--` to speak it anyway:

    say -- stop

`say status --json` prints machine-readable state, for scripts and status
bars:

    {"state":"idle","muted":false,"voice":"af_heart","speed":1,
     "queue_length":0,"remaining_seconds":0.00,"current_text":"","error":""}

## D-Bus interface

Bus name `sh.sayd.Sayd`, object path `/sh/sayd/Sayd`, interface
`sh.sayd.Sayd1`.

| Method | Args | Returns |
|---|---|---|
| `Say` | `text: s, opts: a{sv}` | `id: u` |
| `SaySelection` | `opts: a{sv}` | `id: u` |
| `SayClipboard` | `opts: a{sv}` | `id: u` |
| `Pause` / `Resume` / `PlayPause` | -- | -- |
| `Stop` | -- | -- |
| `Next` / `SkipSentence` | -- | -- |
| `ClearQueue` | -- | -- |
| `Cancel` | `id: u` | -- |
| `SetMuted` | `muted: b` | -- |
| `Quit` | -- | -- |

`opts` accepts `policy` (`"enqueue"`/`"interrupt"`/`"replace"`/`"front"`),
`voice` and `speed`; unknown keys and unparseable values are ignored rather
than rejected. `Say`/`SaySelection`/`SayClipboard` return the queued
utterance id, or `0` if nothing was queued (muted, or empty after cleanup).

| Property | Type | Meaning |
|---|---|---|
| `State` | `s` | `"idle"` / `"speaking"` / `"paused"` / `"error"` |
| `Muted` | `b` | |
| `Voice` | `s` | |
| `Speed` | `d` | |
| `QueueLength` | `u` | |
| `RemainingSeconds` | `d` | |
| `CurrentText` | `s` | |
| `CurrentId` | `u` | `0` when nothing is playing |
| `Error` | `s` | empty unless `State` is `"error"` |

## Environment variables

- `SAYD_MODELS_DIR` -- overrides where model weights and voice packs are
  found, instead of the XDG/`./models` search above.
- `SAYD_NO_AUDIO=1` -- substitutes a sink that accepts and discards every
  sample instead of opening a real audio device, so `sh.sayd.Sayd1` can
  still be introspected, called and polled on a machine with no audio (no
  `/dev/snd`, PulseAudio refusing to start, CI). **This is a testing aid,
  not a supported way to run `sayd`** -- there is no audio output in this
  mode, and utterances finish instantly since nothing paces playback.

The daemon also reacquires the audio device automatically after a failure
(device unplugged, PulseAudio/PipeWire restart), retrying every couple of
seconds until it succeeds -- no restart needed.

## Verify your install

This is the acceptance check for a working setup. It needs a sway session
with a real audio device, so it cannot be run as part of this repository's
own test suite -- walk it yourself after installing:

1. `cargo build --release`, put `sayd` and `say` on `$PATH`.
2. Add the lines from `docs/sway.conf.example` to your sway config, then
   reload (`$mod+Shift+c`).
3. Select text in any window, press `$mod+Shift+s` -- it should speak.
4. Run `say status` while it speaks -- expect `state: speaking` and a
   non-zero `remaining` figure.
5. Press `$mod+Shift+x` -- it should stop immediately.
6. Copy text, press `$mod+Shift+v` -- it should speak the clipboard.
7. Run `say "hello from the terminal"` -- it should speak.
8. Reload the sway config again -- `pgrep -c sayd` should still report `1`.

## Status

M1 (engine and audio) and M2 (D-Bus interface, `say` CLI, selection and
clipboard reading, single-instance handling) are done. The tray icon,
MPRIS integration and a settings window are still to come.

## Licence

MIT, except the vendored misaki lexicons in
[`sayd-misaki-en`](crates/sayd-misaki-en), which are Apache-2.0.
