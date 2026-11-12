#!/bin/sh
# Remote access (timeclock-remote.service at boot): RustDesk's package from the clock's data folder, kept out of the
# public image since its custom.txt holds the password. Unpacked into /var/lib/sweetfiresupportdesk when it isn't yet or
# its version differs; its settings, ID included, stay in the data folder. No package, no remote access.
set -eu
DIR=/data/timeclock_system_files/remote
PKG=$DIR/sweetfiresupportdesk.deb
DEST=/var/lib/sweetfiresupportdesk
[ -f "$PKG" ] || { echo "remote.sh: no $PKG; running without remote access"; exit 0; }
install -d -m 700 "$DIR/config" /root/.config
if [ ! -L /root/.config/sweetfiresupportdesk ]; then
  rm -rf /root/.config/sweetfiresupportdesk
  ln -s "$DIR/config" /root/.config/sweetfiresupportdesk
fi
want=$(dpkg-deb -f "$PKG" Version)
[ "$(cat "$DEST/.version" 2>/dev/null)" != "$want" ] || exit 0
rm -rf "$DEST.new"
dpkg-deb -x "$PKG" "$DEST.new"
# SELinux: systemd starts only files labelled as programs, and /var/lib's label isn't one.
chcon -R -t bin_t "$DEST.new"
echo "$want" >"$DEST.new/.version"
rm -rf "$DEST"
mv "$DEST.new" "$DEST"
echo "remote.sh: unpacked $want"
