# timeclock-host

The timeclock clocks' host operating system, as one bootc image: Fedora CoreOS with the clock's display (sway and the
kiosk browser), the standby screen and screen watcher from
[timeclock-standby](https://github.com/AetherBreaker/timeclock-standby), remote access, a quiet boot behind the logo
splash, Docker for the clock app (deployed by Coolify), and the clock's network and tunnel settings.

Published as `ghcr.io/aetherbreaker/timeclock-host`. Clocks follow a channel: `:latest` (every release) or `:stable`
(releases promoted by hand). The watcher keeps the host on its channel, reading these variables of the clock's Coolify
application:

| Variable | Values | Empty or unset |
|---|---|---|
| `HOST_UPDATE_CHANNEL` | `latest`, `stable` | the channel the clock already follows |
| `HOST_UPDATE_MODE` | `immediate`, `window` | `immediate` |
| `HOST_UPDATE_WINDOW` | `HH:MM/<minutes>m`, local time, e.g. `03:00/60m` | `03:00/60m` |

An update is applied only when nobody has touched the screen for a minute and no Coolify deploy is running; the splash
says a system update is installing. If the new host's display, Docker or app isn't up within ten minutes, it rolls back
and never tries that release again.

## Build

```sh
docker build -t ghcr.io/aetherbreaker/timeclock-host:latest -f Containerfile .
```

`STANDBY_REF` (a timeclock-standby commit) picks the watcher and display files. The build fails if the early-boot image
outgrows `/boot`.

## Install a clock

`install/make-iso.sh <clock>` builds an install stick from Fedora CoreOS's live ISO and `private/<clock>/` (the clock's
name and number, drives, keys, wifi; see the script). The stick wipes the clock's two drives without asking, mirrors
them (RAID1), and the first boot switches to this image.
