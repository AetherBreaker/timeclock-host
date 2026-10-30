#!/bin/bash
# The clock installer, on the install stick's first screen (timeclock-install.service on terminal 1 of Fedora CoreOS's
# live system). Asks the clock's name and number, finds the internal drives (two are mirrored, RAID1; one runs alone),
# erases them once confirmed, installs Fedora CoreOS with the clock's settings, and restarts into it; its first boot
# switches to the timeclock-host image. Answers on the kernel command line (timeclock.name=, timeclock.number=, the VM
# test) are used without asking, and without the confirmation.
set -euo pipefail
D=/etc/timeclock-install
arg() { tr ' ' '\n' </proc/cmdline | sed -n "s/^timeclock\.$1=//p" | head -n1; }
valid_name() { [[ $1 =~ ^[a-z0-9]([a-z0-9-]{0,44}[a-z0-9])?$ ]]; }
valid_number() { [[ $1 =~ ^[1-9][0-9]?$ ]]; }

clear
echo "Timeclock install"
echo
name=$(arg name)
number=$(arg number)
unattended=false
if valid_name "$name" && valid_number "$number"; then
  unattended=true
else
  until valid_name "$name"; do
    read -rp "Clock name (lowercase letters, digits and dashes, e.g. office): " name
  done
  until valid_number "$number"; do
    read -rp "Clock number (1-99, different for every clock): " number
  done
fi

# The internal drives: whole disks that aren't removable, on USB (the stick is) or memory (zram), at most two.
mapfile -t disks < <(lsblk -dnpo NAME,TYPE,TRAN,RM |
  awk '$2 == "disk" && $3 != "usb" && $NF == "0" && $1 !~ /zram/ {print $1}' | head -n2)
[ ${#disks[@]} -gt 0 ] || { echo "No internal drive found. Nothing was changed."; exec sleep infinity; }
# Each by a stable name for the installed system's first boot, where the drive order may differ.
ids=()
for d in "${disks[@]}"; do
  id=$(find /dev/disk/by-id -maxdepth 1 -lname "*/${d##*/}" ! -name 'wwn-*' | sort | head -n1)
  [ -n "$id" ] || id=$(find /dev/disk/by-id -maxdepth 1 -lname "*/${d##*/}" | sort | head -n1)
  ids+=("${id:-$d}")
done

echo
echo "Clock: timeclock-system-$name, number $number"
echo "Drives to erase:"
lsblk -dno NAME,SIZE,MODEL "${disks[@]}" | sed 's/^/  /'
[ ${#disks[@]} -eq 2 ] && echo "(mirrored: either drive boots the clock alone)" || echo "(one drive: no mirror)"
if ! $unattended; then
  echo
  read -rp "Erase these drives and install? Type yes: " answer
  [ "$answer" = yes ] || { echo "Nothing was changed. Restart to begin again."; exec sleep infinity; }
fi

echo
echo "Erasing the drives' old systems..."
for md in /dev/md/* /dev/md[0-9]*; do [ -b "$md" ] && mdadm --stop "$md" >/dev/null 2>&1 || true; done
for d in "${disks[@]}"; do
  for p in $(lsblk -lnpo NAME "$d" | tail -n +2); do
    mdadm --zero-superblock "$p" >/dev/null 2>&1 || true
    wipefs -aq "$p" || true
  done
  wipefs -aq "$d"
done

# The clock's settings: the template for its drive count, with its name, number and drives filled in.
if [ ${#disks[@]} -eq 2 ]; then
  sed -e "s|/dev/disk/by-id/TIMECLOCK-DISK-1|${ids[0]}|g" -e "s|/dev/disk/by-id/TIMECLOCK-DISK-2|${ids[1]}|g" \
    "$D/clock-mirror.ign" >/tmp/clock.ign
else
  cp "$D/clock-single.ign" /tmp/clock.ign
fi
sed -i -e "s/TIMECLOCKNAME/$name/g" -e "s/TIMECLOCKNUMBER/$number/g" /tmp/clock.ign

echo "Installing (a few minutes)..."
coreos-installer install "${ids[0]}" --ignition-file /tmp/clock.ign
# A firmware boot entry for the new drive, first in the boot order: this HP tries the network on a drive with none, and
# a stick left in would install again.
"$D/boot-entries"

echo
echo "Installed. Remove the stick: the clock restarts in 15 seconds, sets itself up, and restarts once more into the"
echo "timeclock system (a few minutes, with internet). Then run new_clock.py from the workstation."
sleep 15
systemctl reboot
