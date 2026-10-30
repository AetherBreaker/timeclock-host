#!/bin/bash
# Build a clock's install stick, out/<clock>.iso: Fedora CoreOS's live ISO, set to wipe the clock's two drives, install
# onto them mirrored (RAID1: root, /boot, an EFI partition on each) with the clock's identity from private/<clock>/,
# and move onto the timeclock-host image at first boot. Booting the stick installs at once, without asking: pull it out
# when the clock reboots.
#
# Usage: install/make-iso.sh <clock>, after install/fetch-iso.sh. private/<clock>/ holds clock.conf (HOSTNAME, DISK1,
# DISK2 as /dev/disk/by-id paths), etc/timeclock/clock.env, coolify.pub, tcadmin.pub and password.hash; optionally
# etc/ssh/ssh_host_* and etc/coolify-tunnel/key* (a reinstalled clock keeps its SSH identity and tunnel), and
# etc/NetworkManager/system-connections/*.nmconnection.
set -euo pipefail
cd "$(dirname "$0")/.."
clock=$1
dir=private/$clock
# shellcheck source=/dev/null
. "$dir/clock.conf"
IMAGE=${IMAGE:-ghcr.io/aetherbreaker/timeclock-host:latest}
W=$(pwd -W 2>/dev/null || pwd)
# The firmware boot entries script: right after the install (--post-install), at first boot, and in the host image.
sed 's/\r$//' files/usr/libexec/timeclock/boot-entries >"$dir/boot-entries"

files() { # path mode local-path, for each local file that exists
  while [ $# -gt 0 ]; do
    if [ -f "$dir/$3" ]; then
      printf '    - path: %s\n      mode: %s\n      contents:\n        local: %s\n' "$1" "$2" "$3"
    fi
    shift 3
  done
}

{
  cat <<EOF
variant: fcos
version: 1.6.0
boot_device:
  layout: x86_64
  mirror:
    devices:
      - $DISK1
      - $DISK2
passwd:
  users:
    - name: root
      ssh_authorized_keys_local: [coolify.pub]
    - name: tcadmin
      password_hash: $(cat "$dir/password.hash")
      ssh_authorized_keys_local: [tcadmin.pub]
      groups: [sudo, wheel, adm, systemd-journal]
storage:
  directories:
    - path: /etc/coolify-tunnel
      mode: 0700
  files:
    - path: /etc/hostname
      mode: 0644
      contents:
        inline: $HOSTNAME
EOF
  files /etc/timeclock/clock.env 0600 etc/timeclock/clock.env \
    /etc/ssh/ssh_host_ed25519_key 0600 etc/ssh/ssh_host_ed25519_key \
    /etc/ssh/ssh_host_ed25519_key.pub 0644 etc/ssh/ssh_host_ed25519_key.pub \
    /etc/ssh/ssh_host_ecdsa_key 0600 etc/ssh/ssh_host_ecdsa_key \
    /etc/ssh/ssh_host_ecdsa_key.pub 0644 etc/ssh/ssh_host_ecdsa_key.pub \
    /etc/ssh/ssh_host_rsa_key 0600 etc/ssh/ssh_host_rsa_key \
    /etc/ssh/ssh_host_rsa_key.pub 0644 etc/ssh/ssh_host_rsa_key.pub \
    /etc/coolify-tunnel/key 0600 etc/coolify-tunnel/key \
    /etc/coolify-tunnel/key.pub 0644 etc/coolify-tunnel/key.pub \
    /etc/coolify-tunnel/known_hosts 0644 etc/coolify-tunnel/known_hosts \
    /etc/systemd/system/coolify-tunnel.service 0644 etc/systemd/system/coolify-tunnel.service
  for nm in "$dir"/etc/NetworkManager/system-connections/*.nmconnection; do
    [ -f "$nm" ] || continue
    files "/etc/NetworkManager/system-connections/${nm##*/}" 0600 "etc/NetworkManager/system-connections/${nm##*/}"
  done
  files /etc/timeclock/boot-entries 0755 boot-entries
  if [ -f "$dir/etc/systemd/system/coolify-tunnel.service" ]; then
    printf '  links:\n    - path: /etc/systemd/system/multi-user.target.wants/coolify-tunnel.service\n'
    printf '      target: /etc/systemd/system/coolify-tunnel.service\n'
  fi
  cat <<EOF
systemd:
  units:
    # Firmware boot entries for both drives' EFI partitions once the first boot has mirrored them (the installer's entry
    # is for the first drive's partition before that). The host image runs its own copy from then on.
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
    - name: timeclock-switch-image.service
      enabled: true
      contents: |
        [Unit]
        Description=Move this new install onto the clock's host image, then reboot into it
        ConditionPathExists=!/var/lib/timeclock-switched
        Wants=network-online.target
        After=network-online.target

        [Service]
        Type=oneshot
        ExecStart=/usr/bin/bootc switch $IMAGE
        ExecStart=/usr/bin/touch /var/lib/timeclock-switched
        ExecStart=/usr/bin/systemctl reboot
        Restart=on-failure
        RestartSec=30

        [Install]
        WantedBy=multi-user.target
EOF
} >"$dir/config.bu"

# Before the install, in the live system: stop and erase the drives' old RAID arrays and signatures, and drop firmware
# boot entries for internal drives (an HP keeps trying to restore a dead one instead of booting). USB entries stay.
cat >"$dir/pre-install.sh" <<EOF
#!/bin/bash
set -x
for md in /dev/md/* /dev/md[0-9]*; do [ -b "\$md" ] && mdadm --stop "\$md"; done
for d in $DISK1 $DISK2; do
  for p in \$(lsblk -lnpo NAME "\$d" | tail -n +2); do mdadm --zero-superblock "\$p"; wipefs -af "\$p"; done
  wipefs -af "\$d"
done
if command -v efibootmgr >/dev/null; then
  efibootmgr -v | grep -v 'USB(' | sed -n 's/^Boot\([0-9A-Fa-f]\{4\}\)\*\{0,1\} .*HD([0-9]*,GPT,.*/\1/p' |
    while read -r n; do efibootmgr -q -b "\$n" -B; done
fi
exit 0
EOF

MSYS_NO_PATHCONV=1 docker run --rm -v "$W/$dir:/d" -w /d quay.io/coreos/butane:release --strict --files-dir /d config.bu \
  >"$dir/config.ign"
iso=$(cd out/iso && ls fedora-coreos-*-live-iso.x86_64.iso | tail -n1)
rm -f "out/$clock.iso"
MSYS_NO_PATHCONV=1 docker run --rm -v "$W/$dir:/d" -v "$W/out:/out" quay.io/coreos/coreos-installer:release \
  iso customize --dest-device "$DISK1" --dest-ignition /d/config.ign --pre-install /d/pre-install.sh \
  --post-install /d/boot-entries -o "/out/$clock.iso" "/out/iso/$iso"
echo "make-iso: out/$clock.iso (installs onto $DISK1 and $DISK2 mirrored, then switches to $IMAGE)"
