# /// script
# requires-python = ">=3.14"
# dependencies = ["cryptography>=46"]
# ///
"""Check what the clocks' Coolify deploys depend on; exits 1 if anything is off.

Run it after updating Coolify by hand (its auto-update is off), and when a clock deploy fails within seconds. Coolify
changed under us twice, each found only at the next release: 4.5.3 began enforcing compose's required variables, and
4.7.0 began running ssh with -F /dev/null, which skipped the `Host *-tunnel` rule the clocks' server names relied on.
The checks:
1. Coolify's auto-update is off, and its email alerts (failed deploys, unreachable servers) are on;
2. coolify-tunnel is a Dockerfile application (Coolify discards a compose file's network aliases) with its port, its
   sysctl and every clock server's <hostname>-tunnel among its network aliases, and the running container has them;
3. Coolify reaches every clock: each server, validated now, is reachable and usable;
4. every clock application has each variable timeclock-system's docker/compose.yaml requires (${VAR:?}), a production
   clock also PINGKEY (its heartbeat; the Pushover pair is the owner's to add), and a production clock deploys only on
   docker/compose.yaml changes.

  uv run install/check_coolify.py

Needs what new_clock.py needs for Coolify: COOLIFY_API_TOKEN, `ssh Coolify_Server`, and the timeclock-system checkout
beside this one.
"""

# Standard library imports
import json
import re
import sys
import time
from datetime import UTC, datetime

# First party imports
from new_clock import WORKSPACE, api, run

COMPOSE = WORKSPACE / "timeclock-system/docker/compose.yaml"
SQL = "docker exec -i coolify-db psql -U coolify -d coolify -X -q -t -A -c '{}'"
failures: list[str] = []


def check(ok: bool, what: str) -> None:
  """Print one line of the checklist, recording a failure."""
  print(f"  {'ok  ' if ok else 'FAIL'} {what}")
  if not ok:
    failures.append(what)


def main() -> None:  # noqa: D103 - the module says what it checks
  print(run("ssh", "Coolify_Server", "docker inspect coolify --format '{{.Config.Image}}'").strip())
  auto_update = run("ssh", "Coolify_Server", SQL.format("SELECT is_auto_update_enabled FROM instance_settings")).strip()
  check(auto_update == "f", "auto-update is off")
  email = run(
    "ssh", "Coolify_Server", SQL.format("SELECT smtp_enabled OR resend_enabled FROM email_notification_settings WHERE team_id = 0")
  )
  check(email.strip() == "t", "email alerts are on (Root Team)")

  servers = [s for s in api("GET", "/servers") if s["name"].startswith("timeclock-system-")]  # pyright: ignore[reportGeneralTypeIssues]
  apps = api("GET", "/applications")
  tunnel_uuid = next(a["uuid"] for a in apps if a["name"] == "coolify-tunnel")  # pyright: ignore[reportGeneralTypeIssues]
  tunnel = api("GET", f"/applications/{tunnel_uuid}")
  assert isinstance(tunnel, dict)
  print("coolify-tunnel")
  check(tunnel["build_pack"] == "dockerfile", "a Dockerfile application")
  check("2223:22" in (tunnel["ports_mappings"] or ""), "publishes 2223:22")
  check("net.ipv4.tcp_retries2=8" in (tunnel["custom_docker_run_options"] or ""), "sets net.ipv4.tcp_retries2=8")
  names = {s["ip"] for s in servers}
  missing = names - set((tunnel["custom_network_aliases"] or "").split(","))
  check(not missing, "network aliases name every clock server" + (f", not {', '.join(sorted(missing))}" if missing else ""))
  inspect = "docker inspect $(docker ps -q --filter name={}) --format '{{{{json .NetworkSettings.Networks.coolify.Aliases}}}}'"
  missing = names - set(json.loads(run("ssh", "Coolify_Server", inspect.format(tunnel_uuid)) or "[]"))
  check(not missing, "the running container has them" + (f", not {', '.join(sorted(missing))}" if missing else ""))

  # Validation is a queued job: wait for each server's settings to change after asking, so a stale result can't pass.
  print("Clock servers")
  asked = datetime.now(UTC)
  for s in servers:
    api("POST", f"/servers/{s['uuid']}/validate")
  pending = {s["uuid"]: s["name"] for s in servers}
  deadline = time.monotonic() + 120
  while pending and time.monotonic() < deadline:
    time.sleep(5)
    for uuid, name in list(pending.items()):
      settings = api("GET", f"/servers/{uuid}")["settings"]  # pyright: ignore[reportIndexIssue]
      if datetime.fromisoformat(settings["updated_at"]) > asked:
        check(settings["is_reachable"] and settings["is_usable"], f"{name} is reachable and usable")
        del pending[uuid]
  for name in pending.values():
    check(ok=False, what=f"{name} validated within 2 minutes")

  print("Clock applications")
  required = set(re.findall(r"\$\{(\w+):\?", COMPOSE.read_text(encoding="utf-8")))
  for app in sorted((a for a in apps if a["name"].startswith("timeclock-system-")), key=lambda a: a["name"]):  # pyright: ignore[reportGeneralTypeIssues]
    envs = {e["key"]: e["value"] for e in api("GET", f"/applications/{app['uuid']}/envs") if not e["is_preview"]}  # pyright: ignore[reportGeneralTypeIssues]
    production = envs.get("TIMECLOCK_DB_NAME") == "timeclock"
    empty = sorted(k for k in required | ({"PINGKEY"} if production else set()) if not (envs.get(k) or "").strip())
    check(not empty, f"{app['name']} has every required variable" + (f", not {', '.join(empty)}" if empty else ""))
    if production:
      check(app["watch_paths"] == "docker/compose.yaml", f"{app['name']} deploys only on docker/compose.yaml changes")

  print("all ok" if not failures else f"{len(failures)} failed")
  sys.exit(1 if failures else 0)


if __name__ == "__main__":
  main()
