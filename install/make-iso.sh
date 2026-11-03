#!/bin/bash
# Build the clocks' install stick, out/timeclock-install.iso: Fedora CoreOS's live ISO with the clock installer
# (install/installer.sh), which asks the clock's name and number on screen and installs onto its internal drives. One
# stick for every clock. It holds tcadmin's password hash and the wifi network's password, so it stays in out/
# (gitignored). Write it with Rufus (DD image mode) or balenaEtcher.
#
# Usage: TC_PASSWORD=... [WIFI_SSID=... WIFI_PASSWORD=...] install/make-iso.sh
#   TC_PASSWORD               tcadmin's console password (SSH takes the workstation's key only)
#   WIFI_SSID, WIFI_PASSWORD  the wifi network every clock joins beside its cable (unset: none)
#   CHANNEL                   the host image channel a new clock starts on: stable (default) or latest
#   TCADMIN_KEY               tcadmin's SSH key (default ~/.ssh/timeclock_dev.pub)
#   OUT, LIVE_KARGS           another output file and extra live kernel arguments (the VM test's answers)
set -euo pipefail
cd "$(dirname "$0")/.."
: "${TC_PASSWORD:?TC_PASSWORD, the console password for tcadmin, is required}"
CHANNEL=${CHANNEL:-stable}
case "$CHANNEL" in stable | latest) ;; *) echo "make-iso: CHANNEL is stable or latest" >&2; exit 1 ;; esac
TCADMIN_KEY=${TCADMIN_KEY:-$HOME/.ssh/timeclock_dev.pub}
OUT=${OUT:-out/timeclock-install.iso}
IMAGE=ghcr.io/aetherbreaker/timeclock-host:$CHANNEL
W=$(pwd -W 2>/dev/null || pwd)
work=work/iso
rm -rf "$work"
mkdir -p "$work" out/iso

# Fedora CoreOS's current live ISO, checked against Fedora's signature by coreos-installer.
MSYS_NO_PATHCONV=1 docker run --rm -v "$W:/r" quay.io/coreos/coreos-installer:release \
  download -s stable -p metal -f iso -C /r/out/iso >/dev/null
iso=$(cd out/iso && ls fedora-coreos-*-live-iso.x86_64.iso | sort -V | tail -n1)

sed 's/\r$//' install/coolify.pub >"$work/coolify.pub"
sed 's/\r$//' "$TCADMIN_KEY" >"$work/tcadmin.pub"
sed 's/\r$//' install/installer.sh >"$work/installer.sh"
sed 's/\r$//' files/usr/libexec/timeclock/boot-entries >"$work/boot-entries"
sed 's/\r$//' install/switch-image.sh >"$work/switch-image"
hash=$(printf %s "$TC_PASSWORD" | MSYS_NO_PATHCONV=1 docker run --rm -i --entrypoint openssl \
  quay.io/fedora/fedora-coreos:stable passwd -6 -stdin)
wifi=""
if [ -n "${WIFI_SSID:-}" ]; then
  : "${WIFI_PASSWORD:?WIFI_PASSWORD is required with WIFI_SSID}"
  # A keyfile reads a backslash as an escape and ends a value at a newline. No interface or MAC: any wifi device joins.
  case "$WIFI_SSID$WIFI_PASSWORD" in *\\* | *$'\n'*) echo "make-iso: no backslash or newline in the wifi settings" >&2; exit 1 ;; esac
  # The profile's uuid from a Linux kernel: Git Bash on the workstation has neither /proc's nor uuidgen.
  uuid=$(MSYS_NO_PATHCONV=1 docker run --rm --entrypoint cat quay.io/fedora/fedora-coreos:stable /proc/sys/kernel/random/uuid)
  printf '[connection]\nid=%s\nuuid=%s\ntype=wifi\nautoconnect=true\n\n[wifi]\nmode=infrastructure\nssid=%s\n\n' \
    "$WIFI_SSID" "$uuid" "$WIFI_SSID" >"$work/wifi.nmconnection"
  printf '[wifi-security]\nkey-mgmt=wpa-psk\npsk=%s\n\n[ipv4]\nmethod=auto\n\n[ipv6]\naddr-gen-mode=default\nmethod=auto\n' \
    "$WIFI_PASSWORD" >>"$work/wifi.nmconnection"
  wifi="    - path: /etc/NetworkManager/system-connections/wifi.nmconnection
      mode: 0600
      contents:
        local: wifi.nmconnection"
fi

# The clock's first-boot settings (Ignition), one per drive count. The installer fills in the name, number and drives.
clock_config() {
  printf 'variant: fcos\nversion: 1.6.0\n'
  if [ "$1" = mirror ]; then
    printf 'boot_device:\n  layout: x86_64\n  mirror:\n    devices:\n'
    printf '      - /dev/disk/by-id/TIMECLOCK-DISK-1\n      - /dev/disk/by-id/TIMECLOCK-DISK-2\n'
  fi
  cat <<EOF
passwd:
  users:
    # Coolify logs in as root by its key; tcadmin by the workstation's key, or at the console by password.
    - name: root
      ssh_authorized_keys_local: [coolify.pub]
    - name: tcadmin
      password_hash: $hash
      ssh_authorized_keys_local: [tcadmin.pub]
      groups: [sudo, wheel, adm, systemd-journal]
storage:
  files:
    - path: /etc/hostname
      mode: 0644
      contents:
        inline: timeclock-system-TIMECLOCKNAME
    - path: /etc/timeclock/clock.env
      mode: 0600
      contents:
        inline: |
          INSTANCE_ID=TIMECLOCKNAME
          CLOCK_NUMBER=TIMECLOCKNUMBER
    - path: /etc/timeclock/boot-entries
      mode: 0755
      contents:
        local: boot-entries
    - path: /etc/timeclock/switch-image
      mode: 0755
      contents:
        local: switch-image
$wifi
systemd:
  units:
    # Firmware boot entries for both drives' EFI partitions once the first boot has mirrored them (the installer's
    # entry is for the first drive's, before that). The host image runs its own copy from then on.
    - name: timeclock-boot-entries-install.service
      enabled: true
      contents: |
        [Unit]
        Description=Firmware boot entries for each drive's EFI partition
        ConditionPathExists=!/usr/libexec/timeclock/boot-entries

        [Service]
        Type=oneshot
        ExecStart=/etc/timeclock/boot-entries

        [Install]
        WantedBy=multi-user.target
    # It waits for the internet itself, saying so on screen, and runs past the end of boot (exec, not oneshot) so the
    # login prompt and its messages show instead of systemd's "a start job is running".
    - name: timeclock-switch-image.service
      enabled: true
      contents: |
        [Unit]
        Description=Move this new install onto the clock's host image, then reboot into it
        ConditionPathExists=!/var/lib/timeclock-switched
        After=NetworkManager.service

        [Service]
        Type=exec
        ExecStart=/etc/timeclock/switch-image $IMAGE
        StandardOutput=journal+console
        SyslogIdentifier=timeclock-setup

        [Install]
        WantedBy=multi-user.target
EOF
}
butane() {
  MSYS_NO_PATHCONV=1 docker run --rm -i -v "$W/$work:/d" -w /d quay.io/coreos/butane:release --strict --files-dir /d
}
for variant in mirror single; do
  clock_config "$variant" | butane >"$work/clock-$variant.ign"
  grep -q TIMECLOCKNAME "$work/clock-$variant.ign" && grep -q TIMECLOCKNUMBER "$work/clock-$variant.ign" ||
    { echo "make-iso: the clock's placeholders didn't survive Butane" >&2; exit 1; }
done
grep -q TIMECLOCK-DISK-2 "$work/clock-mirror.ign" || { echo "make-iso: the drive placeholders didn't survive" >&2; exit 1; }

# The live system: the installer on terminal 1, with the clock's settings beside it.
butane >"$work/live.ign" <<'EOF'
variant: fcos
version: 1.6.0
storage:
  files:
    - path: /etc/timeclock-install/installer.sh
      mode: 0755
      contents:
        local: installer.sh
    - path: /etc/timeclock-install/boot-entries
      mode: 0755
      contents:
        local: boot-entries
    - path: /etc/timeclock-install/clock-mirror.ign
      mode: 0600
      contents:
        local: clock-mirror.ign
    - path: /etc/timeclock-install/clock-single.ign
      mode: 0600
      contents:
        local: clock-single.ign
systemd:
  units:
    - name: timeclock-install.service
      enabled: true
      contents: |
        [Unit]
        Description=Timeclock install: asks the clock's name and number, installs onto its internal drives
        After=systemd-user-sessions.service
        Conflicts=getty@tty1.service

        [Service]
        Type=simple
        ExecStart=/bin/bash /etc/timeclock-install/installer.sh
        StandardInput=tty
        StandardOutput=tty
        StandardError=tty
        TTYPath=/dev/tty1
        TTYReset=yes
        TTYVHangup=yes

        [Install]
        WantedBy=multi-user.target
EOF

kargs=(--live-karg-append quiet --live-karg-append loglevel=3)
for k in ${LIVE_KARGS:-}; do kargs+=(--live-karg-append "$k"); done
rm -f "$OUT"
MSYS_NO_PATHCONV=1 docker run --rm -v "$W:/r" quay.io/coreos/coreos-installer:release \
  iso customize --live-ignition "/r/$work/live.ign" "${kargs[@]}" -o "/r/$OUT" "/r/out/iso/$iso"
# GRUB starts the live system at once instead of a 5-second menu with one entry: the same length, patched in place, as
# coreos-installer patches the kernel arguments in this file.
off=$(grep -abo 'set timeout=5' "$OUT" | cut -d: -f1)
[ "$(echo "$off" | wc -w)" = 1 ] || { echo "make-iso: GRUB's 'set timeout=5' not found exactly once" >&2; exit 1; }
printf 0 | dd of="$OUT" bs=1 seek=$((off + 12)) conv=notrunc status=none
# The stick's EFI partition is named EFI-SYSTEM, like a clock's own: the first boot copies the partition by that name
# onto the mirrored ones (Fedora CoreOS's transposefs), and took the stick's when it was left in. Renamed in place, the
# same length, in its boot sector and its root directory's volume label.
offs=$(grep -abo 'EFI-SYSTEM' "$OUT" | cut -d: -f1)
[ "$(echo "$offs" | wc -w)" = 2 ] || { echo "make-iso: the EFI partition's name not found exactly twice" >&2; exit 1; }
for o in $offs; do printf TC-INSTALL | dd of="$OUT" bs=1 seek="$o" conv=notrunc status=none; done
rm -rf "$work"
echo "make-iso: $OUT (new clocks start on $IMAGE)"
