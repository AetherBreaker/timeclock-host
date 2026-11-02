"""`poe release`: a host image release, in the shape of devkit's.

Bumps the version (uv version), commits "Bump version to X", tags vX ("Version X"), pushes both, creates the GitHub
release vX with the notes, then waits for the tag's run of publish.yml, which builds the image and pushes :vX and
:latest. devkit's own release can't serve: it counts a release done once its wheel is on the index, and this repository
publishes an image instead.

Usage: poe release [bump ...] ['multi-word notes'] [--force | -f] [--dry-run]
"""

# Standard library imports
import json
import subprocess
import sys
import time

BUMPS = ("major", "minor", "patch", "stable", "alpha", "beta", "rc", "post", "dev")
IMAGE = "ghcr.io/aetherbreaker/timeclock-host"
WORKFLOW = "publish.yml"
RUN_START_WAIT = 180  # seconds for the tag's publish run to appear


def run(*args: str, check: bool = True) -> str:
  """Run a command to its end; its output. A failure ends the release with the command's error."""
  result = subprocess.run(args, capture_output=True, text=True, check=False)
  if check and result.returncode != 0:
    sys.exit(f"{' '.join(args)} failed:\n{(result.stderr or result.stdout).strip()}")
  return result.stdout.strip()


def main() -> None:  # noqa: C901, PLR0912, PLR0915  # one release, step by step
  """Release the host image."""
  bumps, words, force, dry_run = [], [], False, False
  for arg in sys.argv[1:]:
    if arg in {"--force", "-f"}:
      force = True
    elif arg == "--dry-run":
      dry_run = True
    elif arg.startswith("-"):
      sys.exit(f"unknown option {arg}")
    elif arg in BUMPS:
      bumps.append(arg)
    else:
      words.append(arg)
  if len(words) > 1:
    sys.exit(f"release notes go in one quoted argument; got {len(words)}: {words}")
  notes = words[0].strip() if words else ""
  if notes and " " not in notes:
    sys.exit(f"{notes!r} isn't a bump type ({', '.join(BUMPS)}); release notes need more than one word")

  # Pre-flight: a clean main, level with origin, and a version not yet released.
  if (branch := run("git", "rev-parse", "--abbrev-ref", "HEAD")) != "main":
    sys.exit(f"releases come from main, not {branch}")
  if status := run("git", "status", "--porcelain"):
    sys.exit(f"the working tree has changes; commit or stash them first:\n{status}")
  run("git", "fetch", "-q", "origin", "main", "--tags")
  if run("git", "rev-parse", "HEAD") != run("git", "rev-parse", "origin/main"):
    sys.exit("main isn't level with origin/main; pull or push first")
  current = run("uv", "version", "--short")
  bump_args = [a for b in bumps for a in ("--bump", b)]
  version = run("uv", "version", "--short", "--dry-run", *bump_args) if bumps else current
  tag = f"v{version}"
  if run("git", "tag", "--list", tag) or run("git", "ls-remote", "--tags", "origin", f"refs/tags/{tag}"):
    sys.exit(f"{tag} is already tagged; bump the version (poe release patch)")

  print(f"timeclock-host {current} -> {version}" if bumps else f"timeclock-host {version} (current version, no bump)")
  if bumps:
    print(f"  commit 'Bump version to {version}' (pyproject.toml, uv.lock)")
  print(f"  tag {tag} ('Version {version}'), push to origin")
  print(f"  GitHub release {tag}: {notes or '(no notes)'}")
  print(f"  then wait for {WORKFLOW} to push {IMAGE}:{tag} and :latest (clocks on :latest take it)")
  if dry_run:
    return
  if not force and input("Release? [y/N] ").strip().lower() not in {"y", "yes"}:
    sys.exit("not released")

  if bumps:
    run("uv", "version", *bump_args)
    run("git", "add", "pyproject.toml", "uv.lock")
    run("git", "commit", "-q", "-m", f"Bump version to {version}")
  run("git", "tag", "-a", tag, "-m", f"Version {version}")
  if bumps:
    run("git", "push", "-q", "origin", "main")
  run("git", "push", "-q", "origin", tag)
  release_url = run("gh", "release", "create", tag, "--verify-tag", "--title", tag, "--notes", notes)
  print(f"released {tag}: {release_url}")

  # The tag push started publish.yml; find that run and follow it.
  run_id, waited = None, 0
  while run_id is None:
    runs = json.loads(
      run("gh", "run", "list", "--workflow", WORKFLOW, "--event", "push", "--limit", "10", "--json", "databaseId,headBranch")
    )
    run_id = next((r["databaseId"] for r in runs if r["headBranch"] == tag), None)
    if run_id is None:
      if waited >= RUN_START_WAIT:
        sys.exit(f"no {WORKFLOW} run for {tag} started within {RUN_START_WAIT}s; check the repository's Actions page")
      time.sleep(5)
      waited += 5
  print(f"following {WORKFLOW} run {run_id}")
  if subprocess.run(["gh", "run", "watch", str(run_id), "--exit-status", "--interval", "15"], check=False).returncode:
    sys.exit(f"the publish run failed; {tag} is tagged and released, but no image went out (gh run view {run_id} --log-failed)")
  versions = json.loads(run("gh", "api", "users/AetherBreaker/packages/container/timeclock-host/versions"))
  digest = next((v["name"] for v in versions if tag in v["metadata"]["container"]["tags"]), "?")
  print(f"published {IMAGE}:{tag} ({digest}); clocks on :latest take it within a minute or two")


if __name__ == "__main__":
  main()
