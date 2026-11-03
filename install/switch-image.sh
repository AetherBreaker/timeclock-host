#!/bin/bash
# A new clock's first boot (timeclock-switch-image.service): moves the plain Fedora CoreOS the stick installed onto the
# host image ($1), then restarts into it. Its output reaches the screen too, so the clock never sits silent behind its
# login prompt: the network's state every 15 s while there's no internet, then a line every 15 s while it downloads.
set -uo pipefail
image=$1
echo "Setting up the timeclock system: it downloads, then the clock restarts into it. Leave the clock on."
t=$SECONDS
while true; do
  until curl -s -o /dev/null --max-time 10 https://ghcr.io/v2/; do
    d=$((SECONDS - t))
    printf 'No internet yet (%d min %02d s). Network: %s\n' $((d / 60)) $((d % 60)) "$(nmcli -g DEVICE,TYPE,STATE device |
      awk -F: '$2 == "ethernet" || $2 == "wifi" {printf "%s%s %s %s", s, $1, $2, $3; s = ", "}')"
    sleep 15
  done
  echo "Online: $(ip -4 -br addr | awk '$1 != "lo" {printf "%s%s %s", s, $1, $3; s = ", "}'). Downloading $image..."
  (
    t=$SECONDS
    while sleep 15; do
      d=$((SECONDS - t))
      printf 'Still working (%d min %02d s): %s MB received\n' $((d / 60)) $((d % 60)) \
        "$(awk 'FILENAME !~ "/lo/" {s += $1} END {printf "%d", s / 1e6}' /sys/class/net/*/statistics/rx_bytes)"
    done
  ) &
  beat=$!
  bootc switch "$image"
  ok=$?
  kill "$beat"
  [ "$ok" = 0 ] && break
  echo "The download failed (above). Trying again in 15 s."
  sleep 15
done
touch /var/lib/timeclock-switched
echo "Done. Restarting into the timeclock system."
systemctl reboot
