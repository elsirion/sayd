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

GTK4 and libadwaita are a third: **build**, not optional, dependencies of the
`sayd` binary itself. The settings window (see [Settings](#settings) below)
runs in-process on the daemon's own glib main loop rather than as a separate
process, so there is no cargo feature that leaves it out and no way to build
`sayd` at all without their development headers present, even on a machine
that will never open the window:

    # Debian/Ubuntu
    apt install libgtk-4-dev libadwaita-1-dev

    # Nix
    # already in flake.nix's devShell -- nothing to add

`sayd`'s `Cargo.toml` pins libadwaita's Rust bindings to their `v1_4`
feature, which is what makes `SpinRow`/`SwitchRow`/`EntryRow` exist in the
bindings at all -- every type newer than 1.0 compiles out otherwise, since
the bindings have no `default` feature to bring them in on their own. That
is a Cargo feature requirement, not a ceiling on the system library: the
*system* libadwaita only needs to be at least 1.4 at build time, and most
current distributions ship considerably newer.

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
5. Settings…, opening the window described in [Settings](#settings) below.
6. Quit.

Volume is absent from the menu, on purpose: `sayd` registers as a named
PipeWire client, so `pavucontrol` (or any per-application mixer) already
controls its volume; duplicating that here would just be two controls
fighting over one knob.

One timing note, since it can look surprising: `State` flips to `speaking`
on submit *before* the utterance text is populated into `current` (the
engine synthesises text in chunks and only knows what it is about to speak
once the first chunk starts). For roughly one synthesis chunk, the menu can
legitimately show "Speaking" with no current utterance yet. It is bounded
and self-correcting, not a bug.

## Settings

`sayd` ships a GTK4/libadwaita settings window rather than asking you to
hand-edit a config file for everyday changes -- open it from the tray's
"Settings…" entry (see [Tray](#tray) above). It is built on demand and
destroyed when closed, so the daemon carries no window, and no GTK
resources, for the vast majority of its life.

The window is a view over one file, `$XDG_CONFIG_HOME/sayd/config.toml`
(falling back to `~/.config/sayd/config.toml`), never a second copy of the
settings. Every change the window makes writes through to that file
immediately -- debounced by 250ms so dragging a spin button does not write
on every tick -- and applies to the next utterance. A hand edit to
`config.toml`, made with an editor or a script while `sayd` is running, is
picked up the same way and without a restart: the daemon watches the file
with inotify, debounced the same 250ms, with its own writes suppressed so a
save the settings window just made is never mistaken for an external edit
and reloaded a second time.

**One caveat.** Changing `model` or `threads` drops the loaded ONNX
session -- every other setting is free. The dropped session is rebuilt on
the next utterance, which then pays a one-off reload of the ~1.27 GB model
(over a second) that no other field's change costs. That drop is deferred to
the moment the utterance already playing actually finishes, rather than
applied the instant the change lands, so switching models or thread counts
mid-article does not cut the current sentence in half.

`idle_unload_secs` behaves the other way around from what its name might
suggest: `0` means the session is *never* unloaded while idle, not that it
is unloaded immediately.

**When to reach for `speed_mode = "stretch"`.** `speed_mode` picks how
`speed` is realised, and does not touch the loaded session at all (unlike
`model`/`threads` above, toggling it is free). The default, `"model"`, hands
`speed` straight to Kokoro's own `speed` input. Measured on `af_heart`, "The
quick brown fox jumps over the lazy dog.": at `speed = 1.3` specifically,
Kokoro renders the leading "The" about 10 dB quieter than at neighbouring
speeds, which sounds like the word being skipped rather than spoken quietly
-- and `speed` there is not even a linear tempo control (1.3 renders at
roughly 1.17x). If a submission is losing its first word or two around a
particular speed, that is this. `"stretch"` synthesizes at `1.0` -- where the
dropout does not happen -- and time-stretches the result (WSOLA) to the
requested factor instead, which keeps the leading word at its normal level
and hits the tempo actually asked for. It has its own artifacts (WSOLA is not
free of them), which is why it is opt-in rather than the default.

The file is equally meant to be edited by hand, so here it is in full, with
every default. Every key is optional -- a file naming only the two you care
about is a complete config, and anything absent falls back to what is shown
here:

```toml
voice = "af_heart"      # any voice pack in <models>/voices, without the .bin
speed = 1.0             # 0.5 to 2.0; outside that it is clamped, with a warning
speed_mode = "model"    # model | stretch; anything else runs model, with a warning
model = "fp32"          # fp32 | fp16 | q8; anything else runs fp32, with a warning
threads = 8             # measured peak; 16 and 24 both regress
idle_unload_secs = 600  # seconds idle before the session is dropped; 0 = never
muted = false
max_chars = 20000       # submissions longer than this are refused

[cleanup]
collapse_whitespace = true
rejoin_hyphenation = true
urls = "link"           # link | domain | keep
strip_markdown = true
drop_code_blocks = true
spell_acronyms = true

[chunking]
target_chars = 400
lookahead_chunks = 2

[notifications]
enabled = false          # the monitor is not even started when false
allow = []               # app_name values, matched case-insensitively
cooldown_secs = 30       # per-application rate-limit window; 0 disables it
speak_app_name = true    # prefix the announcement with the application's name
speak_body = false       # append the notification's body after its summary
```

See [Notifications](#notifications) below for what each of those does.

A malformed file never wedges the daemon, and is not overwritten until you
change something in the window -- or until you press Mute, which writes
through too (see below): `sayd` keeps the settings it is already running,
reports the parse error in the tray menu (as a `Config:` line, separate from
the engine's own errors -- a typo in `config.toml` never stops the daemon
speaking), and picks the file up the moment it parses again.

A file that parses but says something `sayd` cannot do is applied as the
nearest thing it *can* do, and says so in the same place: an unrecognised
`model` runs `fp32` (which is what would have loaded anyway), an unrecognised
`speed_mode` runs `model` the same way, and `speed` outside `0.5`-`2.0` is
clamped, each with a warning naming the value and what is actually being
used. As above, your file is left exactly as you
wrote it -- the corrected value only reaches the disk when you next change a
setting, at which point the window writes the whole config it is running.

Any of those writes -- from the window, from the tray, from `say mute` --
rewrites the whole file in canonical form. A config you maintain by hand
keeps its *values*, but not its comments or its key order, from the first
time something writes it.

Mute is the one control that is both a setting and a transport command.
Muting from the tray, `say mute`, D-Bus or the settings window silences what
is playing *and* writes `muted = true` to the config, so it survives a
restart (spec §6); the same is true of the speed set through MPRIS `Rate`.
Before this they lived only inside the running daemon, and the next config
change of any kind silently undid them.

## Notifications

`sayd` can speak desktop notifications -- "Signal: Alice sent a message." It
sees them by watching the session bus for the `org.freedesktop.Notifications`
`Notify` call, not by owning that name itself, so this is entirely
independent of whichever notification daemon you already run (mako, dunst,
...): it keeps receiving every `Notify` call, keeps displaying its own popups,
and keeps returning its own ids to the calling application, exactly as if
`sayd` were not there. **The other half of that is a real limit, not just a
reassurance**: a notification that never crosses the bus -- an application
drawing its own popup, or a daemon with a private protocol -- is invisible to
`sayd`. There is nothing to fix here; a passive monitor can only see bus
traffic.

**Off by default.** `notifications.enabled = false` out of the box --
narration is a behaviour change to your desktop, and it should be asked for,
not assumed.

### Finding names: the on-ramp

Turning `enabled` on is not enough by itself. With an empty `allow` list,
`sayd` speaks nothing at all -- it needs to be told each application's
`app_name`, and there are two ways to find those.

**The easy way** is the settings window (Settings… from the tray, see
[Settings](#settings) above). Below the **Applications to announce** list
itself, two suggestion groups offer names to add with one click:

- **Seen notifying** -- every application `sayd` has actually watched call
  `Notify` this run, most recent first, each shown with the icon that
  application itself sent. A row here is exactly right: it is the name the
  application really passed on this machine, not a guess, so adding it is
  certain to match. It appears only after that application has notified at
  least once -- there is nothing to show before that -- and it appears while
  the window is open, so triggering a notification from the application you
  are looking for is a way to find its name on the spot.
- **Common applications** -- a short built-in list, offered so there is
  something to click before anything has notified. Unlike a seen entry,
  each one is `sayd`'s guess at what the application calls itself as
  `app_name`. Matching is exact and case-insensitive (no globs, no regex --
  see below), so a wrong guess does not partially work -- it silently
  matches nothing, and the row you just added announces nothing. If that
  happens, the log-based fallback below still has the real name.

Either group disappears when it has nothing to offer -- all curated names
already allowed, say, or nothing seen yet.

A **Seen notifying** row's icon comes from the notification itself, never
from a lookup `sayd` does on its own: what you see is what the application
supplied. Applications supply it in three different places, and `sayd` tries
them in the order most likely to resolve -- the `desktop-entry` hint (an
app-id, which is what every GTK/GNOME application sends), then the
`image-path` hint (what `notify-send -i` sends), then the `app_icon`
argument. **Common applications** rows are not from a notification at all;
their icon is a name `sayd` ships for the row, since nothing has run yet to
supply one.

A row shows a generic placeholder glyph when none of that produced an image:
the application sent no icon in any of the three places (`notify-send` sends
none at all, so its own row is always a placeholder), or your icon theme has
nothing by the name it did send, or the file it pointed at is gone. It never
means the row is broken -- adding the application works exactly the same
either way.

**Without the window**, or for anything Common applications missed, `sayd`
also logs the `app_name` of every notification it declines to speak, once
per distinct name per run:

    info: notification from "Signal" (not in notifications.allow; add it to
    speak these)

That log line is the fallback discovery workflow: enable notifications,
watch `sayd`'s log (or run it in a terminal) while you go about your day,
and copy each name you want spoken into `notifications.allow`. Each name is
logged once, not once per notification, so a chat application does not turn
the log into the flood the allowlist exists to prevent in the first place.

The allowlist matches `app_name` exactly, case-insensitively -- no globs, no
regex. `app_name` is whatever the application passed to `Notify`, which is
also what ends up in the discovery log, in the settings window's Seen
notifying group, and in the spoken announcement's prefix, so all four
always agree on the name.

### Config

```toml
[notifications]
enabled = false          # the monitor is not even started when false
allow = []               # app_name values, matched case-insensitively
cooldown_secs = 30       # per-application rate-limit window; 0 disables it
speak_app_name = true    # prefix the announcement with the application's name
speak_body = false       # append the notification's body after its summary
```

### What gets said

The announcement is built from the notification's summary and (optionally)
its body, composed by two independent switches:

| `speak_app_name` | `speak_body` | Announcement |
|---|---|---|
| true | false | `Signal: Alice sent a message` |
| false | false | `Alice sent a message` |
| true | true | `Signal: Alice sent a message. See you at five` |
| false | true | `Alice sent a message. See you at five` |

`speak_body` defaults to `false`: summaries are written to be read at a
glance, but bodies are frequently several sentences and often just restate
the summary -- an email notification's body can be a whole paragraph, so
reading it out is offered, not assumed.

Bodies may carry a small set of HTML-like tags the freedesktop notification
spec allows (`<b>`, `<i>`, `<u>`, `<a>`, `<img>`, ...); `sayd` strips those
tags and decodes the accompanying entities before speaking, so a body does
not come out as "b Alice b replied".

### Rate limiting

At most one utterance per application per `cooldown_secs`. The first
notification from an application speaks immediately; anything else that
arrives inside that window is counted instead of spoken, and read out as a
single follow-up once the window closes:

    Signal: Alice sent a message        <- spoken immediately
                                         <- 3 more arrive inside the window
    Signal: 3 more notifications        <- spoken once the window closes

**`cooldown_secs = 0` disables rate limiting entirely** -- every notification
speaks, with no coalescing. The setting window's Cooldown row says the same
thing, because `0` is the one value here that does not mean "no wait": it
turns the limiter off.

Notifications are submitted with the `front` queue policy (the same one
`opts.policy = "front"` selects on the D-Bus interface): a notification is
placed ahead of whatever is already queued, but does not interrupt the
utterance currently playing. Mute applies to notifications exactly as it does
to every other source -- a muted daemon accepts and silently discards them,
so nothing piles up to be spoken once you unmute.

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
the same bounds; the clamp is enforced on the way to the config file and
again by the engine, the same one `SetSpeed` enforces on the D-Bus
interface). A rate set here is written to `config.toml` -- see
[Settings](#settings) -- so it is not silently reverted by the next config
change, and it survives a restart. `Metadata` carries a title built from the
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
   actions listed in the [Tray](#tray) section above should all be present,
   along with a "Settings…" entry. Clicking it is covered separately in
   [Verify the settings window](#verify-the-settings-window) below.
6. Click Pause in the menu -- the icon should switch to the paused icon.
   Click it again (now labelled Resume), then click Stop.
7. Run `playerctl -p sayd status` at each of those points -- it should
   agree with what the tray is showing (`Playing`/`Paused`/`Stopped`).
8. Press the media play/pause key (`docs/sway.conf.example` binds it to
   `playerctl -p sayd play-pause`) -- playback should toggle the same way
   the tray's Pause/Resume entry does.

## Verify the settings window

Same reason as the section above: the window is not covered by automated
tests (the design spec lists it under "not tested automatically" on
purpose -- there is no display in a CI or agent environment to test it
against), and reading `crates/sayd/src/settings/window.rs` cannot substitute
for looking at it. Walk this yourself once, after installing:

1. `cargo build --release`, put `sayd` and `say` on `$PATH`, start it (or
   reload sway if `exec sayd` is already in your config).
2. Open the tray menu, click **Settings…** -- the window appears.
3. Change **Voice** to a different installed voice. Type into **Test** and
   press Speak (or hit Enter) -- it speaks in the new voice.
4. `cat ~/.config/sayd/config.toml` -- the new `voice` is already there,
   with no restart and no further action.
5. Move **Speed**, press Speak again -- audibly faster or slower.
6. Set **Speed** to 1.3 and Speak the default Test sentence with **Speed
   mode** on **model** -- listen for "The" at the start; it can render
   noticeably quieter than "quick" right after it (the measured dropout, see
   [Settings](#settings) above). Switch **Speed mode** to **stretch** and
   speak the same sentence again at the same 1.3 -- "The" should be plainly
   audible now, and the sentence should take longer (closer to the tempo
   actually asked for). Changing this row must not pause or reload anything
   -- unlike **Model**/**Threads** below, the change is audible on the very
   next utterance.
7. Speak something long enough to still be playing a few seconds later,
   then, while it plays, change **Model** or **Threads**. The sentence
   already in the air must finish uninterrupted -- no cut, no glitch, no
   voice or pace change mid-sentence. Only the utterance *after* that one
   should show the reload pause (a bit over a second) before it starts.
8. Close the window -- `say status` (or `pgrep -c sayd`) should still show
   the daemon alive and unaffected; closing the settings window must not be
   mistakable for quitting the daemon.
9. Reopen the window -- every value is as you left it.
10. With the daemon running and the window closed, hand-edit
    `~/.config/sayd/config.toml` directly (change `speed`, say) and save.
    Reopen the window -- it shows the edited value, picked up without a
    restart.
11. Rename or move a voice pack's directory out from under a voice
    `config.toml` currently names, then open **Voice** -- that entry must
    show up clearly marked as missing (e.g. "'name' — no voice pack
    installed"), not silently render as, or select, some other installed
    voice instead.
12. At the window's default width, check the **Idle unload** row's
    subtitle -- it is a long sentence ("Seconds of silence before the
    ~1.27 GB session is dropped; 0 never unloads") and should read in full,
    wrapping onto more than one line, rather than being truncated with an
    ellipsis.
13. Restart `sayd` -- every setting from the steps above survives. This is
    M4's stated done-when.

## Verify notifications

Same reason as the two sections above: the monitor talks to a real session
bus and a real notification daemon (mako, dunst, ...), neither of which
exists in a CI or agent environment. The composition, filtering and
rate-limiting logic is unit-tested (`crates/sayd/src/notify/`); what none of
that covers is a real `Notify` call arriving over the bus you actually use.
Walk this yourself once, after installing, with `notify-send` available
(it ships with most notification daemons, or `apt install libnotify-bin`):

1. `cargo build --release`, put `sayd` and `say` on `$PATH`, start it (or
   reload sway if `exec sayd` is already in your config).
2. Open **Settings…**, turn on **Speak notifications** under the
   Notifications group, leave **Applications to announce** empty.
3. Run `notify-send -a "Test App" "hello"`. `-a` sets `app_name` explicitly
   -- notify-send's first positional argument is the *summary*, not the app
   name, and without `-a` most builds send an empty or unhelpful one. Your
   notification daemon still shows the popup as usual, and `sayd` says
   nothing -- watch `sayd`'s log (run it in a terminal, or `journalctl
   --user -u sh.sayd.Sayd -f` under systemd) for a line naming `"Test App"`
   as declined.
4. Add `Test App` to the allowlist (the settings window's "Applications to
   announce" add-row entry, or hand-edit `notifications.allow` in
   `config.toml`).
5. Run `notify-send -a "Test App" "hello again"` -- `sayd` should speak
   "Test App: hello again" this time.
6. Send five in quick succession:
   `for i in $(seq 5); do notify-send -a "Test App" "msg $i"; done`.
   Expect one immediate utterance for the first, then, once the cooldown
   window closes (30s by default -- lower **Cooldown** in the settings
   window first if you would rather not wait), a single coalesced
   "Test App: 4 more notifications".
7. Turn **Speak notifications** off again and run
   `notify-send -a "Test App" "hello"` once more -- the popup still
   appears, but `sayd` stays silent.
8. Open **Settings…**, leave it open, and send one notification from an
   application that is *not* on the allowlist yet --
   `notify-send -a "Another App" -i dialog-information "hi"` works, or use
   whatever step 3 declined. Within a second or so, **Another App** should
   appear in the open window as a row under **Seen notifying**, carrying
   the `dialog-information` icon that call supplied (or the placeholder
   glyph, if you sent no `-i` or your theme has nothing by that name -- see
   [Notifications](#notifications) above). Click its **Add** button -- the
   row should move out of Seen notifying and appear as a new row under
   **Applications to announce** instead.

## Troubleshooting

**`could not read the primary selection: ... -- WAYLAND_DISPLAY is not set`**

`sayd` is running outside the graphical session, so there is no compositor
for it to ask -- a systemd user unit that never received the session
environment, a bare TTY, or an ssh shell. D-Bus, the tray and `say "text"`
all keep working, which is why this shows up only on the selection
keybinds. Start `sayd` from the sway config with `exec sayd`, or import the
environment before the unit starts (see
[`docs/sh.sayd.Sayd.service.example`](docs/sh.sayd.Sayd.service.example)).

**`could not read the primary selection: ... -- nothing is listening on
<path>`**

The environment names a socket, but no compositor answers on it. Usually a
stale `WAYLAND_DISPLAY` left over from an earlier session; compare it
against `ls $XDG_RUNTIME_DIR/wayland-*`.

**`this compositor does not support ... version N`**

The compositor lacks `wlr-data-control`, which is how `sayd` reads the
selection without keyboard focus. sway 1.9 or newer is required for the
primary selection; check with `sway --version`.

## Status

M1 (engine and audio), M2 (D-Bus interface, `say` CLI, selection and
clipboard reading, single-instance handling), M3 (StatusNotifierItem tray,
MPRIS2), M4 (the GTK4/libadwaita settings window, config write-through, and
an inotify reload for hand edits) and M5 (notification narration -- see
[Notifications](#notifications) above) are done. That is the whole of the
original build order from the main design doc's plan: nothing is queued up
after M5, so what comes next is whatever is chosen from here, not a milestone
already on the books.

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
front in their own READMEs. `sayd` itself is stricter still: without GTK4
and libadwaita development headers present (see
[Native dependencies](#native-dependencies)) it will not even compile, so
`cargo publish --dry-run -p sayd` needs them on the publishing machine too,
not just at runtime.

## Licence

MIT, except the vendored misaki lexicons in
[`sayd-misaki-en`](crates/sayd-misaki-en), which are Apache-2.0.
