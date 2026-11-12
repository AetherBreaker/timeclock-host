# shellcheck shell=sh
# /etc/profile.d/timeclock-tmout.sh: a console login on the clock's text terminals logs out after 5 minutes idle, so
# nobody finds one left open with Ctrl+Alt+F2. SSH sessions aren't affected.
case "$(tty 2>/dev/null)" in
  /dev/tty[0-9]*)
    TMOUT=300
    readonly TMOUT
    export TMOUT
    ;;
esac
