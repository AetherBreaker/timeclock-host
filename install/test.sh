#!/bin/bash
# The install stick's test, in a QEMU VM under UEFI with two blank drives and the stick a USB disk left in throughout,
# as on a clock: the installer runs unattended (the name and number on the kernel command line), and over SSH the first
# boot must show the clock's identity, both drives mirrored with the drive's own EFI files (not the stick's), and a
# firmware boot entry for each; then the switch to the host image (logging its progress), its services, and a boot from the second drive alone. No KVM in Docker Desktop, so QEMU emulates the CPU: it takes an hour or more. Logs and disks in work/vm.
#
# Usage: TC_PASSWORD=... install/test.sh (CHANNEL, default latest: the release under test)
set -euo pipefail
cd "$(dirname "$0")/.."
: "${TC_PASSWORD:?TC_PASSWORD, the console password for tcadmin, is required}"
export CHANNEL=${CHANNEL:-latest}
W=$(pwd -W 2>/dev/null || pwd)
vm=work/vm
rm -rf "$vm"
mkdir -p "$vm"
OUT=$vm/test.iso LIVE_KARGS="timeclock.name=vmtest timeclock.number=99" WIFI_SSID='' bash install/make-iso.sh
cp "$HOME/.ssh/timeclock_dev" "$vm/id"

cat >"$vm/run.sh" <<'EOF'
#!/bin/bash
# In a debian:trixie container: QEMU, OVMF, and SSH to the VM on localhost:2222.
set -euo pipefail
apt-get update -qq && apt-get install -y -qq qemu-system-x86 qemu-utils ovmf openssh-client >/dev/null
cd /vm
chmod 600 id
cp /usr/share/OVMF/OVMF_VARS_4M.fd vars.fd
qemu-img create -q -f qcow2 disk1.qcow2 40G
qemu-img create -q -f qcow2 disk2.qcow2 40G
qemu() { # extra drive arguments
  timeout 4h qemu-system-x86_64 -machine q35 -accel tcg,thread=multi -cpu max -smp 4 -m 4096 \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd -drive if=pflash,format=raw,file=vars.fd \
    -netdev user,id=n,hostfwd=tcp:127.0.0.1:2222-:22 -device virtio-net-pci,netdev=n \
    -display none -serial file:serial-$1.log "${@:2}" &
  echo $! >qemu.pid
}
ssh_vm() { ssh -i id -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  -o ConnectTimeout=10 -p 2222 tcadmin@127.0.0.1 "$@"; }
wait_ssh() { # until the VM answers and $1 holds, or $2 minutes
  for _ in $(seq 1 $(($2 * 2))); do ssh_vm "$1" >/dev/null 2>&1 && return 0; sleep 30; done
  echo "FAIL: timed out waiting for: $1"; exit 1
}
d1='-drive file=disk1.qcow2,if=none,id=d1 -device virtio-blk-pci,drive=d1,serial=TCDISK1'
d2='-drive file=disk2.qcow2,if=none,id=d2 -device virtio-blk-pci,drive=d2,serial=TCDISK2'
# A USB disk, not a CD: Linux sees a USB stick's EFI partition, as on a clock, and a CD's not.
iso='-device qemu-xhci,id=xhci -drive file=test.iso,format=raw,if=none,id=stick -device usb-storage,bus=xhci.0,drive=stick'

echo "== install and first boot ($(date +%T))"
# shellcheck disable=SC2086 # several arguments each
qemu install $d1 $d2 $iso
wait_ssh "test -f /etc/timeclock/clock.env" 120
ssh_vm 'hostname; sudo cat /etc/timeclock/clock.env; cat /proc/mdstat; sudo efibootmgr | grep -i timeclock || true'
ssh_vm 'test "$(hostname)" = timeclock-system-vmtest && sudo grep -qx CLOCK_NUMBER=99 /etc/timeclock/clock.env' ||
  { echo "FAIL: identity"; exit 1; }
ssh_vm 'test "$(grep -c "^md.*raid1" /proc/mdstat)" -ge 2' || { echo "FAIL: not mirrored"; exit 1; }
ssh_vm 'sudo mkdir -p /run/esp; for p in esp-1 esp-2; do sudo mount -o ro /dev/disk/by-partlabel/$p /run/esp || exit 1
  sudo test -f /run/esp/EFI/fedora/shimx64.efi; r=$?; sudo umount /run/esp; [ $r = 0 ] || exit 1; done' ||
  { echo "FAIL: the mirrored EFI partitions don't hold the drive's EFI files (the stick's?)"; exit 1; }

echo "== switch to the host image ($(date +%T))"
wait_ssh "sudo bootc status --json | grep -q timeclock-host" 180
wait_ssh "sudo bootc status --format=humanreadable | grep -A1 Booted | grep -q timeclock-host" 30
sleep 120
ssh_vm 'sudo bootc status --format=humanreadable | head -3; sudo efibootmgr | grep -i timeclock; systemctl --failed --no-pager --plain
  for u in timeclock-watcher timeclock-status timeclock-display docker coolify-tunnel timeclock-boot-entries; do
    printf "%s=%s " $u "$(systemctl is-active $u)"; done; echo'
ssh_vm 'test "$(sudo efibootmgr | grep -c "Timeclock (")" -ge 2' || { echo "FAIL: a boot entry per drive"; exit 1; }
ssh_vm 'systemctl is-active -q timeclock-watcher docker' || { echo "FAIL: services"; exit 1; }
ssh_vm 'sudo journalctl -t timeclock-setup -o cat --no-pager | grep -v "^Still working" | tail -n 5'
ssh_vm 'sudo journalctl -t timeclock-setup -o cat | grep -q "^Done. Restarting"' ||
  { echo "FAIL: the switch didn't log its progress"; exit 1; }

echo "== the second drive alone ($(date +%T))"
ssh_vm 'sudo systemctl poweroff' || true
sleep 60
kill "$(cat qemu.pid)" 2>/dev/null || true
sleep 5
# shellcheck disable=SC2086
qemu second $d2
wait_ssh "true" 60
ssh_vm 'cat /proc/mdstat; sudo bootc status --format=humanreadable | grep -A1 Booted'
ssh_vm 'sudo bootc status --format=humanreadable | grep -A1 Booted | grep -q timeclock-host' ||
  { echo "FAIL: the second drive alone"; exit 1; }
kill "$(cat qemu.pid)" 2>/dev/null || true
echo "PASS ($(date +%T))"
EOF

MSYS_NO_PATHCONV=1 docker run --rm -v "$W/$vm:/vm" debian:trixie bash /vm/run.sh 2>&1 | tee "$vm/test.log"
grep -q '^PASS' "$vm/test.log"
