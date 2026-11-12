#!/bin/sh
# The clock browser (timeclock-clock.service): the kiosk on the clock app, hidden by sway until the watcher shows it (spec
# §3.3). It starts on clock-start.html, which moves to the clock once the app answers; a failed load goes back there.
# Invisible either way: the clock window is hidden whenever the clock isn't live.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
CLOCK_URL=${CLOCK_URL:-http://127.0.0.1:8080/?kiosk}
export XDG_RUNTIME_DIR=/run/user/0 GDK_BACKEND=wayland GTK_A11Y=none
for s in "$XDG_RUNTIME_DIR"/wayland-*; do [ -S "$s" ] && export WAYLAND_DISPLAY="${s##*/}"; done
start="file://$HERE/clock-start.html?url=$(printf %s "$CLOCK_URL" | sed 's/%/%25/g; s/&/%26/g; s/?/%3F/g')"
exec python3 "$HERE/kiosk.py" local.timeclock.Clock "$start"
