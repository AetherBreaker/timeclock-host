#!/bin/sh
# The display (timeclock-display.service): sway on terminal 7, in the login session the unit opens (spec §3.6). It checks
# in on its terminal first, taking the screen over from the boot splash.
set -eu
OWN=7 # the watcher's display_vt
# Only from the splash's terminal or this one; a console in use (2-6) is waited out, never switched away from (the
# watcher brings an idle console back here).
while :; do
  prev=$(sed 's/^tty//' /sys/class/tty/tty0/active)
  case "$prev" in 1 | "$OWN") break ;; esac
  sleep 1
done
# Graphics mode first (KDSETMODE), so the kernel doesn't repaint terminal 7 as a black text console on the switch: the
# screen keeps its last picture (the splash) until sway draws.
perl -e 'open(C, "<", "/dev/tty$ARGV[0]") or die "tty$ARGV[0]: $!"; ioctl(C, 0x4B3A, 1) or die "KDSETMODE: $!";
  open(T, "<", "/dev/tty0") or die "tty0: $!"; ioctl(T, 0x5606, $ARGV[0] + 0) or die "VT_ACTIVATE: $!";
  ioctl(T, 0x5607, $ARGV[0] + 0) or die "VT_WAITACTIVE: $!"' "$OWN"
# At boot the splash runs on terminal 1 and answers the switch above, so it quits only now (and only here: the unit
# keeps the distribution's own splash quit from running). It leaves terminal 1 in graphics mode with nothing answering switches,
# where the kernel ignores every switch away from it: back to text mode (unseen from here), so later switches work.
# Terminal 1 whatever showed before: someone may have switched to a console during the splash.
if plymouth --ping 2>/dev/null; then
  plymouth quit --retain-splash
  perl -e 'open(C, "<", "/dev/tty1") or die "tty1: $!"; ioctl(C, 0x4B3A, 0) or die "KDSETMODE: $!"'
fi
exec sway -c /usr/libexec/timeclock/sway.conf
