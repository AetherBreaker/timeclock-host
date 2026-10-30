# The clock's host: Fedora CoreOS with the clock's display, remote access and boot splash (PLAN-host-image.md).

# The screen watcher (also the host updater) and the display's files: timeclock-standby at an exact commit, so a build
# is reproducible and the cache follows the commit, not a branch name.
FROM docker.io/library/rust:1 AS standby
ARG STANDBY_REF=bf4ce084a953b8df75388eac8304b8a708b4a3f9
RUN git init -q /s && git -C /s fetch -q --depth 1 https://github.com/AetherBreaker/timeclock-standby.git "$STANDBY_REF" \
  && git -C /s checkout -q FETCH_HEAD \
  && rustup target add x86_64-unknown-linux-musl \
  && cd /s/watcher && cargo build --release --locked --target x86_64-unknown-linux-musl

FROM quay.io/fedora/fedora-coreos:stable
# Links the published package to this repository (and the repository's Actions to it).
LABEL org.opencontainers.image.source=https://github.com/AetherBreaker/timeclock-host

# The browsers are timeclock-standby's kiosk.py on WebKitGTK: cog 0.18's Wayland output needs an EGL call Mesa 26
# dropped (cog #738, #776). plymouth-plugin-label draws the splash's update message.
RUN dnf -y --setopt=install_weak_deps=False install \
    sway sway-config-upstream swaybg mesa-dri-drivers webkitgtk6.0 gtk4 python3-gobject gobject-introspection python3-cairo \
    xdg-desktop-portal xdg-desktop-portal-wlr pipewire wireplumber pipewire-gstreamer \
    plymouth plymouth-plugin-two-step plymouth-plugin-label plymouth-theme-spinner librsvg2-tools python3 perl-interpreter \
    dpkg avahi NetworkManager-wifi NetworkManager-config-connectivity-fedora wpa_supplicant \
    realtek-firmware iwlwifi-mvm-firmware iwlwifi-dvm-firmware mt7xxx-firmware atheros-firmware \
    docker-compose gtk3 libxdo libXfixes alsa-lib libva gstreamer1-plugins-base dejavu-sans-fonts \
  && dnf clean all

# /opt and /usr/local are links into /var on this base, which updates never touch: the display's files go to
# /usr/libexec/timeclock-standby (labelled as programs, so SELinux runs its scripts as an unconfined service) and the
# watcher to /usr/bin, where timeclock-standby's units expect them.
COPY --from=standby /s/watcher/target/x86_64-unknown-linux-musl/release/timeclock-watcher /usr/bin/timeclock-watcher
COPY --from=standby /s /tmp/standby
RUN set -eu; cd /tmp/standby; S=/usr/libexec/timeclock-standby; install -d "$S"; \
  install -m 644 status/status.py status/standby.html status/standby.css status/fallback.html display/clock-start.html \
    display/sway.conf "$S"/; \
  install -m 755 display/display.sh display/standby.sh display/clock.sh display/remote.sh display/kiosk.py "$S"/; \
  install -D -m 644 display/xdpw.conf /etc/xdg/xdg-desktop-portal-wlr/config; \
  for u in pipewire.service pipewire.socket wireplumber.service; do \
    install -D -m 644 systemd/pipewire-root.conf "/usr/lib/systemd/user/$u.d/timeclock-root.conf"; done; \
  install -D -m 644 systemd/plymouth-reboot-update.conf \
    /usr/lib/systemd/system/plymouth-reboot.service.d/10-timeclock-update.conf; \
  install -m 644 systemd/timeclock-display.pam /etc/pam.d/timeclock-display; \
  install -m 644 systemd/*.service /usr/lib/systemd/system/; \
  install -d /etc/timeclock; install -m 644 config/watcher.conf config/standby.conf /etc/timeclock/; \
  install -m 644 config/timeclock-tmout.sh /etc/profile.d/timeclock-tmout.sh; \
  T=/usr/share/plymouth/themes/timeclock; install -d "$T"; cp /usr/share/plymouth/themes/spinner/*.png "$T"/; \
  install -m 644 plymouth/timeclock.plymouth "$T"/; \
  cd /; rm -rf /tmp/standby

COPY files/ /

# Build contexts from Windows arrive with every file executable: config files back to 644.
RUN set -eu; \
  find /usr/lib/systemd/system/timeclock-* /usr/lib/systemd/system/docker.service.d /usr/lib/tmpfiles.d \
    /usr/lib/bootc/kargs.d /usr/lib/sysctl.d /usr/lib/modprobe.d /usr/lib/NetworkManager/conf.d /etc/ssh/sshd_config.d \
    /etc/coolify-tunnel /etc/sway/config.d /usr/share/timeclock -type f -exec chmod 644 {} +; \
  chmod 700 /etc/coolify-tunnel; chmod 755 /usr/bin/coolify-tunnel-setup /usr/libexec/timeclock/*

# The logo: the splash's image, sway's background (sway.conf) and the standby page's start page (fallback.html).
RUN set -eu; logo=/usr/share/timeclock/branding/SFT-Logo-June-2022-color.svg; \
  rsvg-convert -h 140 "$logo" > /usr/share/plymouth/themes/timeclock/watermark.png; \
  install -m 644 "$logo" /usr/libexec/timeclock-standby/logo.svg; \
  plymouth-set-default-theme timeclock; \
  kver=$(ls /usr/lib/modules); img=/usr/lib/modules/$kver/initramfs.img; \
  lsinitrd -m "$img" | sed -n '/^dracut modules:/,/^=/p' | tr ' ' '\n' | sort > /tmp/before; \
  # dracut copies /root, a link to /var/roothome, which only exists on an installed system. The splash brings every
  # graphics driver along; AMD's and NVIDIA's (with their firmware) made it ~290 MB, past /boot's 384 MB beside the
  # rollback's. Clocks have Intel graphics. The size check fails the build before a clock can't install it.
  mkdir -p /var/roothome; \
  dracut -f --reproducible --omit-drivers "amdgpu radeon nouveau nvidia nvidia_drm nvidia_modeset" "$img" "$kver"; \
  rmdir /var/roothome; ls -la "$img"; [ "$(stat -c %s "$img")" -lt 140000000 ]; \
  lsinitrd -m "$img" | sed -n '/^dracut modules:/,/^=/p' | tr ' ' '\n' | sort > /tmp/after; \
  echo "initramfs modules lost:"; comm -23 /tmp/before /tmp/after; echo "gained:"; comm -13 /tmp/before /tmp/after; \
  ! comm -23 /tmp/before /tmp/after | grep -q .; rm /tmp/before /tmp/after

# Coolify keeps its files in /data/coolify and the app's folder is /data/timeclock_system_files: /data is a link into
# /var. RustDesk's client stays out of this public image (its custom.txt holds the password): remote.sh unpacks it from
# the data folder into /var/lib/sweetfiresupportdesk, and these links give it its usual paths.
RUN set -eu; ln -s var/data /data; \
  ln -sf ../usr/share/zoneinfo/America/New_York /etc/localtime; \
  ln -s /var/lib/sweetfiresupportdesk/usr/share/sweetfiresupportdesk /usr/share/sweetfiresupportdesk; \
  ln -s /usr/share/sweetfiresupportdesk/sweetfiresupportdesk /usr/bin/sweetfiresupportdesk; \
  systemctl mask getty@tty1.service sleep.target suspend.target hibernate.target hybrid-sleep.target \
    zincati.service bootc-fetch-apply-updates.timer; \
  systemctl enable docker.service avahi-daemon.service timeclock-watcher.service timeclock-status.service \
    timeclock-display.service timeclock-standby.service timeclock-clock.service timeclock-remote.service \
    timeclock-tty1.service timeclock-update-splash.service sweetfiresupportdesk.service timeclock-grub-hidden.service \
    timeclock-coolify-network.service timeclock-boot-entries.service; \
  bootc container lint
