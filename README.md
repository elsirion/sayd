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

## Tray

`sayd` registers a [StatusNotifierItem](https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/)
so its icon and menu show up in any host that implements the tray side of
that spec -- waybar's `tray` module, or any other panel. See
[`docs/waybar.jsonc.example`](docs/waybar.jsonc.example) for the waybar
`tray` module configuration (it is generic -- waybar's tray renders every
registered item, not just sayd's).

This needs a StatusNotifierWatcher running. **A bare sway config without
waybar (or another host) has none, and that is not an error** -- `sayd`
logs it once at startup and keeps serving the D-Bus control interface and
MPRIS without a tray icon:

    info: could not register the tray: failed to register to the
    StatusNotifierWatcher: ...; continuing without a tray icon

The icon reflects state:

| State | Icon |
|---|---|
| Idle | `audio-speakers-symbolic` |
| Speaking | `media-playback-start-symbolic` |
| Paused | `media-playback-pause-symbolic` |
| Error | `dialog-error-symbolic` |

**Muted takes precedence over all of the above** -- while muted the icon is
always `audio-volume-muted-symbolic`, regardless of state. These are stock
freedesktop icon names, themed by the host, with no install step; `sayd`
does not ship its own icons into `hicolor` since there is no installer yet.

The tooltip shows the current utterance (truncated) and its estimated
remaining time, or "Nothing playing" when idle.

The menu, top to bottom:

1. A status block (disabled entries): any standing error first, then the
   current utterance and its remaining-time estimate (or "Idle"/"Speaking"
   while nothing has populated into `current` yet -- see below), then up to
   five pending queue entries with a "… and N more pending" line if there
   are more.
2. Transport: Pause/Resume, Skip sentence, Next, Stop, Clear queue.
3. Speak selection, Speak clipboard -- the same actions the sway keybinds
   trigger.
4. Mute, shown as a checkmark.
5. Quit.

**"Settings…" is deliberately absent.** The window it would open is M4,
not yet built, and a menu entry that does nothing would be worse than no
entry at all -- there is nothing to configure through the tray yet. Volume
is absent too, on purpose: `sayd` registers as a named PipeWire client, so
`pavucontrol` (or any per-application mixer) already controls its volume;
duplicating that in the menu would just be two controls fighting over one
knob.

One timing note, since it can look surprising: `State` flips to `speaking`
on submit *before* the utterance text is populated into `current` (the
engine synthesises text in chunks and only knows what it is about to speak
once the first chunk starts). For roughly one synthesis chunk, the menu can
legitimately show "Speaking" with no current utterance yet. It is bounded
and self-correcting, not a bug.

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
than rejected.

`Say`/`SaySelection`/`SayClipboard` return one of three things:

| Return | Meaning |
|---|---|
| a positive id | queued; `Cancel` will accept it |
| `0` | accepted, nothing queued -- muted, or empty after cleanup |
| `4294967295` (`u32::MAX`) | queued, but the id could not be confirmed in time |

**Expect the last one routinely under a burst of submissions**, not as an
exotic edge case. The engine synthesises a whole chunk per step, taking
several seconds, and a call arriving mid-chunk waits for it; rather than
block the caller, the daemon acknowledges the submission without its id. The
text *is* queued and will play. What is lost is the ability to `Cancel` that
particular utterance by id — `Cancel(4294967295)` is a harmless no-op. If you
need ids reliably, submit one utterance at a time and wait for the previous
`CurrentId` to change.

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

## MPRIS

`sayd` also registers `org.mpris.MediaPlayer2.sayd` on the session bus, so
media keys, `playerctl` and waybar's `mpris` module all work against it as
player `sayd`:

    playerctl -p sayd play-pause
    playerctl -p sayd status
    playerctl -p sayd metadata

Like the tray, MPRIS registration failing (a `NameHasOwner`/policy issue on
the bus, say) is logged once and is not fatal -- `sayd` carries on serving
the control interface without it.

What works: `PlayPause`, `Pause`, `Play` (resume), `Stop`, `Next` (the same
skip-to-next-queued-utterance as `Command::Next`), `Quit`, and the `Rate`
property, which genuinely changes playback speed -- reading it back after a
`SetRate` (or after `say status`) reflects the new speed on the next
utterance, clamped to `[0.5, 2.0]` (`MinimumRate`/`MaximumRate` advertise
the same bounds; the engine enforces the clamp, the same one `SetSpeed`
enforces on the D-Bus interface). `Metadata` carries a title built from the
current utterance's text and a per-utterance `mpris:trackid` so it changes
between utterances, per spec, instead of holding one placeholder id
throughout.

What is a deliberate no-op: `Previous`, `Seek` and `SetPosition` do
nothing, and are advertised as such via `CanSeek: false` and
`CanGoPrevious: false` rather than silently failing. An utterance is
synthesised chunk by chunk as it plays, with no addressable buffer to seek
within or rewind into -- there is no "position" for `Seek`/`SetPosition` to
mean anything about, and no previous track to return to once its audio has
been discarded. `playerctl -p sayd previous` correctly reports "No player
could handle this command" rather than doing nothing silently, because it
already respects `CanGoPrevious`.

Volume is likewise not wired to anything real (`Volume` always reads `1.0`
and `SetVolume` is a no-op) for the same reason given in the Tray section:
`sayd` is a named PipeWire client, so PipeWire-level volume control already
exists and does not need duplicating here.

Applying a command sent through MPRIS (or the D-Bus interface, or `say`)
can take up to the length of one synthesis chunk to visibly land -- a few
seconds on real hardware -- because the engine thread is single-threaded
and a chunk, once started, runs to completion before the next queued
command is picked up. This is not specific to MPRIS; it is the same
latency the D-Bus interface's `Say`/`SaySelection`/`SayClipboard` timeout
note above describes, seen from the control side instead of the submit
side.

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

## Verify the tray and media keys

The tray and MPRIS logic is unit-tested (see `crates/sayd/src/tray.rs` and
`crates/sayd/src/mpris.rs`), and the MPRIS/`playerctl` wiring was exercised
end to end against a private D-Bus session as part of building this. What
none of that can cover is waybar actually rendering the icon and menu --
that needs a real panel on a real Wayland session, which does not exist in
a CI or agent environment. Walk this yourself once, after installing:

1. `cargo build --release`, put `sayd` and `say` on `$PATH`.
2. Add the `tray` and `mpris` modules from
   [`docs/waybar.jsonc.example`](docs/waybar.jsonc.example) to your waybar
   config, and reload waybar.
3. Start `sayd` (or reload sway if `exec sayd` is already in your config)
   -- the icon should appear in the tray within a second or two.
4. Speak something long enough to watch -- the icon should change to the
   speaking icon, and hovering it should show a tooltip with the text and
   an estimated remaining time.
5. Open the tray menu -- the current utterance, any pending queue entries
   (up to five, with a count of the rest), and the transport/selection/mute
   actions listed in the [Tray](#tray) section above should all be present.
   There should be no "Settings…" entry.
6. Click Pause in the menu -- the icon should switch to the paused icon.
   Click it again (now labelled Resume), then click Stop.
7. Run `playerctl -p sayd status` at each of those points -- it should
   agree with what the tray is showing (`Playing`/`Paused`/`Stopped`).
8. Press the media play/pause key (`docs/sway.conf.example` binds it to
   `playerctl -p sayd play-pause`) -- playback should toggle the same way
   the tray's Pause/Resume entry does.

## Status

M1 (engine and audio), M2 (D-Bus interface, `say` CLI, selection and
clipboard reading, single-instance handling) and M3 (StatusNotifierItem
tray, MPRIS2) are done. A settings window (M4) is what remains -- there is
currently no GUI way to change the default voice, speed or other config;
that happens through `sayd-core`'s config file or per-submission overrides
(`say --voice`/`--speed`, or the D-Bus `opts` argument) instead.

## Publishing

The workspace version lives in one place — `[workspace.package]` in the root
`Cargo.toml` — and the internal crates are declared once in
`[workspace.dependencies]` with a matching version. Bump both together; a
published crate cannot depend on a bare path, so they must not drift.

Crates must be published bottom-up, because each dry-run resolves its
dependencies against the real index:

```sh
cargo publish -p sayd-misaki-en
cargo publish -p sayd-g2p        # needs sayd-misaki-en on the index
cargo publish -p sayd-kokoro
cargo publish -p sayd-core
cargo publish -p sayd            # needs sayd-core, sayd-g2p, sayd-kokoro
cargo publish -p sayd-cli
```

`cargo publish --dry-run` for a dependent crate will fail until its
dependencies are actually on the index — `failed to select a version for the
requirement` is expected at that stage, not a packaging error.

Note that `sayd-g2p` will not build for anyone without espeak-ng, and
`sayd-kokoro` will build but not run without ONNX Runtime; both say so up
front in their own READMEs.

## Licence

MIT, except the vendored misaki lexicons in
[`sayd-misaki-en`](crates/sayd-misaki-en), which are Apache-2.0.
