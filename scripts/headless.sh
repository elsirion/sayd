#!/usr/bin/env bash
# Run a command against a throwaway Wayland compositor, so GTK tests that
# call `present()` open their windows somewhere nobody is looking.
#
#     scripts/headless.sh cargo test -p sayd --bin sayd settings::window -- --test-threads=1
#
# Why this exists rather than a documented `sway &` incantation: the obvious
# one is wrong in a way that is invisible until you watch your own screen.
# A headless sway picks the next free `wayland-N` in the *session's*
# XDG_RUNTIME_DIR, so hardcoding `WAYLAND_DISPLAY=wayland-1` -- as this
# repo's own doc comment used to -- names whichever compositor got there
# first, which on a developer's machine is the one running their desktop.
# The tests then present real windows onto the real screen, having started a
# headless compositor that sits idle beside them.
#
# So: give the nested compositor an XDG_RUNTIME_DIR of its own. Inside a
# fresh directory `wayland-1` is unambiguous, cannot collide with a running
# session, and disappears with the directory. DISPLAY is unset as well, or
# GTK4 falls back to X11 and lands on :0 -- the same failure by another
# route.
set -euo pipefail

command -v sway >/dev/null || {
    echo "headless.sh: sway is not on PATH; it hosts the tests' windows" >&2
    exit 127
}
[ $# -gt 0 ] || {
    echo "usage: headless.sh COMMAND [ARGS...]" >&2
    exit 64
}

rt="$(mktemp -d)"
chmod 700 "$rt" # a runtime dir must be private or the compositor refuses it
sway_pid=""
cleanup() {
    [ -n "$sway_pid" ] && kill "$sway_pid" 2>/dev/null
    wait "$sway_pid" 2>/dev/null
    rm -rf "$rt"
}
trap cleanup EXIT

# Nothing that wants input devices, an output, or a status bar: the windows
# only have to exist and answer, never to be seen.
printf 'default_border none\n' >"$rt/sway.conf"

XDG_RUNTIME_DIR="$rt" WLR_BACKENDS=headless WLR_RENDERER=pixman \
    sway -c "$rt/sway.conf" >"$rt/sway.log" 2>&1 &
sway_pid=$!

for _ in $(seq 1 100); do
    [ -S "$rt/wayland-1" ] && break
    kill -0 "$sway_pid" 2>/dev/null || break
    sleep 0.1
done
[ -S "$rt/wayland-1" ] || {
    echo "headless.sh: sway did not come up; its log follows" >&2
    cat "$rt/sway.log" >&2
    exit 1
}

# `-u DISPLAY` is load-bearing, not tidiness: with it set, a GTK4 build with
# X11 support will happily use it instead of the socket above.
env -u DISPLAY XDG_RUNTIME_DIR="$rt" WAYLAND_DISPLAY=wayland-1 "$@"
