//! The host updater, as its own service: keeps this bootc host on the newest release of its channel, applied at once or
//! in a daily window, as the app container's Coolify variables say (`HOST_UPDATE_CHANNEL`, `HOST_UPDATE_MODE`,
//! `HOST_UPDATE_WINDOW`). After an update it checks the new host comes up, and rolls back if it doesn't.
//!
//! Nothing here may panic: the release build aborts on a panic, which ends the service (systemd restarts it).
use crate::config::Config;
use crate::docker;
use crate::sway::{self, Json, parse_json};
use std::fs;
use std::io::Write;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const STATE: &str = "/var/lib/timeclock-updater";
/// The last good settings, so a missing app container (mid-deploy, or before the first) changes nothing.
const SETTINGS: &str = "/var/lib/timeclock-updater/host-update";
/// The release being applied: the next start checks the host came up on it.
const PENDING: &str = "/var/lib/timeclock-updater/pending";
/// Left in /run just before the reboot: the reboot splash then says a system update is installing (the plymouth-reboot
/// drop-in, files/usr/lib/systemd/system/plymouth-reboot.service.d/10-timeclock-update.conf).
const UPDATING: &str = "/run/timeclock-host-updating";
/// Releases that failed the check, never applied again; a newer one goes ahead.
const BAD: &str = "/var/lib/timeclock-updater/bad-releases";
const DEFAULT_WINDOW: (u16, u16) = (3 * 60, 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
  Immediate,
  /// Start (minutes after midnight, local time) and length in minutes.
  Window(u16, u16),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
  /// `None` keeps the channel the host already follows.
  pub channel: Option<String>,
  pub mode: Mode,
}

impl Default for Settings {
  /// No settings yet: immediate, so the release that brings them in isn't held for a window (owner).
  fn default() -> Self {
    Self {
      channel: None,
      mode: Mode::Immediate,
    }
  }
}

/// `HH:MM/<n>m`, e.g. `03:00/60m`.
pub fn parse_window(text: &str) -> Option<(u16, u16)> {
  let (at, len) = text.split_once('/')?;
  let (h, m) = at.split_once(':')?;
  let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
  let len: u16 = len.strip_suffix('m')?.parse().ok()?;
  (h < 24 && m < 60 && (1..=1440).contains(&len)).then_some((h * 60 + m, len))
}

/// Settings from the container's `KEY=VALUE` variables. An empty or missing variable means no setting; an invalid one
/// keeps `prev`'s value for it, and is reported.
pub fn settings_from_env(env: &[String], prev: &Settings) -> (Settings, Vec<String>) {
  let var = |key: &str| {
    env
      .iter()
      .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
      .filter(|v| !v.is_empty())
  };
  let mut bad = Vec::new();
  let channel = match var("HOST_UPDATE_CHANNEL") {
    None => None,
    Some(c @ ("latest" | "stable")) => Some(c.to_string()),
    Some(other) => {
      bad.push(format!("HOST_UPDATE_CHANNEL={other} (latest or stable)"));
      prev.channel.clone()
    }
  };
  let prev_window = match prev.mode {
    Mode::Window(s, l) => (s, l),
    Mode::Immediate => DEFAULT_WINDOW,
  };
  let window = match var("HOST_UPDATE_WINDOW") {
    None => prev_window,
    Some(w) => parse_window(w).unwrap_or_else(|| {
      bad.push(format!("HOST_UPDATE_WINDOW={w} (HH:MM/<minutes>m)"));
      prev_window
    }),
  };
  let mode = match var("HOST_UPDATE_MODE") {
    None | Some("immediate") => Mode::Immediate,
    Some("window") => Mode::Window(window.0, window.1),
    Some(other) => {
      bad.push(format!("HOST_UPDATE_MODE={other} (immediate or window)"));
      prev.mode
    }
  };
  (Settings { channel, mode }, bad)
}

fn settings_to_text(s: &Settings) -> String {
  let mode = match s.mode {
    Mode::Immediate => "HOST_UPDATE_MODE=immediate".to_string(),
    Mode::Window(start, len) => {
      format!(
        "HOST_UPDATE_MODE=window\nHOST_UPDATE_WINDOW={:02}:{:02}/{len}m",
        start / 60,
        start % 60
      )
    }
  };
  format!("HOST_UPDATE_CHANNEL={}\n{mode}\n", s.channel.as_deref().unwrap_or(""))
}

/// What `bootc status --json` says, as far as updating needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Host {
  /// The image the host follows.
  pub image: String,
  pub booted: String,
  /// A staged release: its digest, and whether it was downloaded only (applied by `--from-downloaded`, not any reboot).
  pub staged: Option<(String, bool)>,
  /// A newer release seen by the last `bootc upgrade --check`.
  pub cached: Option<String>,
  pub has_rollback: bool,
}

fn at<'a>(j: &'a Json, path: &[&str]) -> Option<&'a Json> {
  path.iter().try_fold(j, |j, k| j.get(k))
}

fn str_at(j: &Json, path: &[&str]) -> Option<String> {
  match at(j, path)? {
    Json::Str(s) => Some(s.clone()),
    _ => None,
  }
}

pub fn parse_status(text: &str) -> Option<Host> {
  let j = parse_json(text)?;
  let staged = at(&j, &["status", "staged"]).filter(|s| matches!(s, Json::Object(_)));
  Some(Host {
    image: str_at(&j, &["spec", "image", "image"]).unwrap_or_default(),
    booted: str_at(&j, &["status", "booted", "image", "imageDigest"])?,
    staged: staged.and_then(|s| {
      let digest = str_at(s, &["image", "imageDigest"])?;
      Some((digest, at(s, &["downloadOnly"]) == Some(&Json::Bool(true))))
    }),
    cached: str_at(&j, &["status", "booted", "cachedUpdate", "imageDigest"]),
    has_rollback: matches!(at(&j, &["status", "rollback"]), Some(Json::Object(_))),
  })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
  Stay,
  /// Follow another channel's image (downloaded and staged; applied as a staged release is).
  Switch {
    image: String,
    download_only: bool,
  },
  /// Ask the registry for a newer release (`bootc upgrade --check`), then decide again.
  Check,
  /// Download and stage the newer release.
  Stage {
    download_only: bool,
  },
  /// Reboot into the staged release.
  Apply {
    from_downloaded: bool,
  },
}

/// Whether `now` (minutes after midnight) falls in the window starting at `start` for `len` minutes; it may cross
/// midnight.
pub fn in_window(now: u16, start: u16, len: u16) -> bool {
  (now + 1440 - start % 1440) % 1440 < len
}

/// The next step for the updater. `checked`: the registry was asked this round. `may_reboot`: nobody has touched the
/// screen lately and no Coolify deploy is running. `now`: local minutes after midnight.
pub fn next_step(
  host: &Host,
  s: &Settings,
  image_base: &str,
  bad: &[String],
  checked: bool,
  may_reboot: bool,
  now: u16,
) -> Step {
  let window = matches!(s.mode, Mode::Window(..));
  if let Some(channel) = &s.channel {
    let want = format!("{image_base}:{channel}");
    if host.image != want {
      return Step::Switch {
        image: want,
        download_only: window,
      };
    }
  }
  if let Some((digest, download_only)) = &host.staged {
    let time = match s.mode {
      Mode::Immediate => true,
      Mode::Window(start, len) => in_window(now, start, len),
    };
    return if time && may_reboot && !bad.contains(digest) {
      Step::Apply {
        from_downloaded: *download_only,
      }
    } else {
      Step::Stay
    };
  }
  if !checked {
    return Step::Check;
  }
  match &host.cached {
    Some(c) if *c != host.booted && !bad.contains(c) => Step::Stage { download_only: window },
    _ => Step::Stay,
  }
}

/// Run a command to its end; its output, or why it failed.
fn run(program: &str, args: &[&str]) -> Result<String, String> {
  let out = Command::new(program)
    .args(args)
    .output()
    .map_err(|e| format!("{program}: {e}"))?;
  if out.status.success() {
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
  } else {
    Err(format!(
      "{program} {}: {}{}",
      args.join(" "),
      String::from_utf8_lossy(&out.stdout).trim(),
      String::from_utf8_lossy(&out.stderr).trim()
    ))
  }
}

fn host_status() -> Result<Host, String> {
  let text = run("bootc", &["status", "--json"])?;
  parse_status(&text).ok_or_else(|| "bootc status: unreadable".to_string())
}

/// The app container (found by its compose service label, as the status app finds it): its variables and whether it
/// runs. `None` when Docker doesn't answer or there's no such container.
fn app_container(cfg: &Config) -> Option<(Vec<String>, bool)> {
  let filter = format!(
    "%7B%22label%22%3A%5B%22com.docker.compose.service%3D{}%22%5D%7D",
    cfg.app_service
  );
  let list =
    parse_json(&docker::docker_get(&cfg.docker_socket, &format!("/containers/json?all=1&filters={filter}")).ok()?)?;
  let Json::Array(items) = list else { return None };
  let pick = items
    .iter()
    .find(|c| c.get("State") == Some(&Json::Str("running".into())))
    .or(items.first())?;
  let id = str_at(pick, &["Id"])?;
  let detail = parse_json(&docker::docker_get(&cfg.docker_socket, &format!("/containers/{id}/json")).ok()?)?;
  let env = match at(&detail, &["Config", "Env"]) {
    Some(Json::Array(vars)) => vars
      .iter()
      .filter_map(|v| match v {
        Json::Str(s) => Some(s.clone()),
        _ => None,
      })
      .collect(),
    _ => Vec::new(),
  };
  Some((env, at(&detail, &["State", "Running"]) == Some(&Json::Bool(true))))
}

/// Local time, minutes after midnight.
fn local_minutes() -> u16 {
  // SAFETY: time() with a null pointer only returns; localtime_r writes the tm we own.
  let mut tm: libc::tm = unsafe { std::mem::zeroed() };
  let t = unsafe { libc::time(std::ptr::null_mut()) };
  if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
    return 0;
  }
  (tm.tm_hour * 60 + tm.tm_min).clamp(0, 1439) as u16
}

fn read_lines(path: &str) -> Vec<String> {
  fs::read_to_string(path)
    .unwrap_or_default()
    .lines()
    .map(str::to_string)
    .filter(|l| !l.is_empty())
    .collect()
}

/// After an update: the new host is good once sway and Docker answer and the app container (if there is one) runs,
/// within `health_wait`. Otherwise the release is marked bad and the host rolls back.
fn check_after_update(cfg: &Config, host: &Host) {
  let Some(pending) = read_lines(PENDING).into_iter().next() else {
    return;
  };
  if host.booted != pending {
    eprintln!(
      "host update: {pending} didn't take; the host still runs {}; not trying it again",
      host.booted
    );
    let _ = fs::OpenOptions::new()
      .create(true)
      .append(true)
      .open(BAD)
      .and_then(|mut f| writeln!(f, "{pending}"));
    let _ = fs::remove_file(PENDING);
    return;
  }
  let deadline = Instant::now() + cfg.update_health_wait;
  loop {
    let display = sway::clock_window(&cfg.sway_socket, &cfg.clock_app_id).is_ok();
    let docker = docker::docker_get(&cfg.docker_socket, "/_ping").is_ok();
    let app = app_container(cfg).is_none_or(|(_, running)| running);
    if display && docker && app {
      eprintln!("host update: {pending} is up");
      let _ = fs::remove_file(PENDING);
      return;
    }
    if Instant::now() >= deadline {
      eprintln!("host update: {pending} isn't up (display {display}, Docker {docker}, app {app}); rolling back");
      let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(BAD)
        .and_then(|mut f| writeln!(f, "{pending}"));
      let _ = fs::remove_file(PENDING);
      if host.has_rollback {
        match run("bootc", &["rollback"]).and_then(|_| run("systemctl", &["reboot"])) {
          Ok(_) => {}
          Err(e) => eprintln!("host update: rollback failed: {e}"),
        }
      }
      return;
    }
    thread::sleep(Duration::from_secs(10));
  }
}

/// The updater, run on the main thread. `last_input`: milliseconds after `start` of the last keyboard or touch input
/// (`u64::MAX`: none).
pub fn run_updater(cfg: Config, start: Instant, last_input: Arc<AtomicU64>) {
  let _ = fs::create_dir_all(STATE);
  let mut settings = {
    let lines = read_lines(SETTINGS);
    settings_from_env(&lines, &Settings::default()).0
  };
  match host_status() {
    Ok(host) => check_after_update(&cfg, &host),
    Err(e) => eprintln!("host update: {e}"),
  }
  loop {
    thread::sleep(cfg.update_check_every);
    if let Some((env, _)) = app_container(&cfg) {
      let (next, bad) = settings_from_env(&env, &settings);
      for b in bad {
        eprintln!("host update: ignoring {b}");
      }
      if next != settings {
        eprintln!("host update: settings {next:?}");
        let _ = fs::write(SETTINGS, settings_to_text(&next));
        settings = next;
      }
    }
    let idle_ms = match last_input.load(Ordering::Relaxed) {
      u64::MAX => u64::MAX,
      t => (start.elapsed().as_millis() as u64).saturating_sub(t),
    };
    let deploying = docker::helper_running(&cfg.docker_socket, &cfg.helper_image);
    let may_reboot = idle_ms >= cfg.update_idle.as_millis() as u64 && !deploying;
    let bad = read_lines(BAD);
    let mut checked = false;
    let step = loop {
      let host = match host_status() {
        Ok(h) => h,
        Err(e) => break Err(e),
      };
      let step = next_step(
        &host,
        &settings,
        &cfg.host_image,
        &bad,
        checked,
        may_reboot,
        local_minutes(),
      );
      if step != Step::Check {
        break Ok((step, host));
      }
      if let Err(e) = run("bootc", &["upgrade", "--check", "--quiet"]) {
        break Err(e);
      }
      checked = true;
    };
    let result = match step {
      Err(e) => Err(e),
      Ok((Step::Stay | Step::Check, _)) => Ok(()),
      Ok((Step::Switch { image, download_only }, _)) => {
        eprintln!("host update: following {image}");
        let mut args = vec!["switch", "--quiet"];
        if download_only {
          args.push("--download-only");
        }
        args.push(&image);
        run("bootc", &args).map(drop)
      }
      Ok((Step::Stage { download_only }, host)) => {
        eprintln!("host update: downloading {}", host.cached.as_deref().unwrap_or("?"));
        let args: &[&str] = if download_only {
          &["upgrade", "--quiet", "--download-only"]
        } else {
          &["upgrade", "--quiet"]
        };
        run("bootc", args).map(drop)
      }
      Ok((Step::Apply { from_downloaded }, host)) => {
        let digest = host.staged.map(|(d, _)| d).unwrap_or_default();
        eprintln!("host update: restarting into {digest}");
        let _ = fs::write(PENDING, format!("{digest}\n"));
        let _ = fs::write(UPDATING, format!("{digest}\n"));
        if from_downloaded {
          run("bootc", &["upgrade", "--from-downloaded", "--apply"]).map(drop)
        } else {
          run("systemctl", &["reboot"]).map(drop)
        }
      }
    };
    if let Err(e) = result {
      eprintln!("host update: {e}");
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn env(vars: &[&str]) -> Vec<String> {
    vars.iter().map(|v| v.to_string()).collect()
  }

  #[test]
  fn no_settings_is_immediate_on_the_current_channel() {
    let (s, bad) = settings_from_env(&env(&["PATH=/bin", "HOST_UPDATE_MODE="]), &Settings::default());
    assert_eq!(s, Settings::default());
    assert!(bad.is_empty());
  }

  #[test]
  fn reads_every_setting() {
    let (s, bad) = settings_from_env(
      &env(&[
        "HOST_UPDATE_CHANNEL=stable",
        "HOST_UPDATE_MODE=window",
        "HOST_UPDATE_WINDOW=23:30/90m",
      ]),
      &Settings::default(),
    );
    assert!(bad.is_empty());
    assert_eq!(s.channel.as_deref(), Some("stable"));
    assert_eq!(s.mode, Mode::Window(23 * 60 + 30, 90));
  }

  #[test]
  fn window_mode_without_a_window_uses_three_am() {
    let (s, _) = settings_from_env(&env(&["HOST_UPDATE_MODE=window"]), &Settings::default());
    assert_eq!(s.mode, Mode::Window(180, 60));
  }

  #[test]
  fn invalid_values_keep_the_last_good_ones() {
    let prev = Settings {
      channel: Some("stable".into()),
      mode: Mode::Window(120, 30),
    };
    let (s, bad) = settings_from_env(
      &env(&[
        "HOST_UPDATE_CHANNEL=evil.example/image",
        "HOST_UPDATE_MODE=sometimes",
        "HOST_UPDATE_WINDOW=25:00/60m",
      ]),
      &prev,
    );
    assert_eq!(s, prev);
    assert_eq!(bad.len(), 3);
  }

  #[test]
  fn settings_survive_their_file() {
    let s = Settings {
      channel: Some("latest".into()),
      mode: Mode::Window(185, 45),
    };
    let lines: Vec<String> = settings_to_text(&s).lines().map(str::to_string).collect();
    assert_eq!(settings_from_env(&lines, &Settings::default()).0, s);
    let none = settings_to_text(&Settings::default());
    assert_eq!(
      settings_from_env(&none.lines().map(str::to_string).collect::<Vec<_>>(), &s).0,
      Settings::default()
    );
  }

  #[test]
  fn windows_may_cross_midnight() {
    assert!(in_window(180, 180, 60));
    assert!(!in_window(240, 180, 60));
    assert!(!in_window(179, 180, 60));
    assert!(in_window(10, 23 * 60 + 30, 60));
    assert!(!in_window(40, 23 * 60 + 30, 60));
  }

  const STATUS: &str = r#"{"apiVersion":"org.containers.bootc/v1","kind":"BootcHost","metadata":{"name":"host"},
    "spec":{"bootOrder":"default","image":{"image":"ghcr.io/aetherbreaker/timeclock-host:latest","transport":"registry"}},
    "status":{"staged":null,"booted":{"image":{"image":{"image":"ghcr.io/aetherbreaker/timeclock-host:latest",
    "transport":"registry"},"version":"44","timestamp":null,"imageDigest":"sha256:aaa","architecture":"amd64"},
    "cachedUpdate":{"image":{"image":"ghcr.io/aetherbreaker/timeclock-host:latest","transport":"registry"},
    "imageDigest":"sha256:bbb"},"incompatible":false,"pinned":false,"store":"ostreeContainer"},
    "rollback":{"image":{"imageDigest":"sha256:000"}},"rollbackQueued":false,"type":"bootcHost"}}"#;

  #[test]
  fn reads_bootc_status() {
    let h = parse_status(STATUS).unwrap();
    assert_eq!(h.image, "ghcr.io/aetherbreaker/timeclock-host:latest");
    assert_eq!(h.booted, "sha256:aaa");
    assert_eq!(h.cached.as_deref(), Some("sha256:bbb"));
    assert_eq!(h.staged, None);
    assert!(h.has_rollback);
    let staged = STATUS.replace(
      r#""staged":null"#,
      r#""staged":{"image":{"imageDigest":"sha256:bbb"},"downloadOnly":true}"#,
    );
    assert_eq!(parse_status(&staged).unwrap().staged, Some(("sha256:bbb".into(), true)));
  }

  const BASE: &str = "ghcr.io/aetherbreaker/timeclock-host";

  fn host() -> Host {
    Host {
      image: format!("{BASE}:latest"),
      booted: "sha256:aaa".into(),
      ..Default::default()
    }
  }

  #[test]
  fn checks_then_stages_a_newer_release() {
    let s = Settings::default();
    assert_eq!(next_step(&host(), &s, BASE, &[], false, true, 600), Step::Check);
    assert_eq!(next_step(&host(), &s, BASE, &[], true, true, 600), Step::Stay);
    let newer = Host {
      cached: Some("sha256:bbb".into()),
      ..host()
    };
    assert_eq!(
      next_step(&newer, &s, BASE, &[], true, true, 600),
      Step::Stage { download_only: false }
    );
    let window = Settings {
      mode: Mode::Window(180, 60),
      ..s
    };
    assert_eq!(
      next_step(&newer, &window, BASE, &[], true, true, 600),
      Step::Stage { download_only: true }
    );
    assert_eq!(
      next_step(
        &newer,
        &Settings::default(),
        BASE,
        &["sha256:bbb".into()],
        true,
        true,
        600
      ),
      Step::Stay
    );
  }

  #[test]
  fn immediate_applies_when_nobody_is_using_the_clock() {
    let staged = Host {
      staged: Some(("sha256:bbb".into(), false)),
      ..host()
    };
    let s = Settings::default();
    assert_eq!(
      next_step(&staged, &s, BASE, &[], false, true, 600),
      Step::Apply { from_downloaded: false }
    );
    assert_eq!(next_step(&staged, &s, BASE, &[], false, false, 600), Step::Stay);
  }

  #[test]
  fn window_applies_only_in_the_window() {
    let staged = Host {
      staged: Some(("sha256:bbb".into(), true)),
      ..host()
    };
    let s = Settings {
      channel: None,
      mode: Mode::Window(180, 60),
    };
    assert_eq!(next_step(&staged, &s, BASE, &[], false, true, 600), Step::Stay);
    assert_eq!(
      next_step(&staged, &s, BASE, &[], false, true, 200),
      Step::Apply { from_downloaded: true }
    );
    assert_eq!(next_step(&staged, &s, BASE, &[], false, false, 200), Step::Stay);
  }

  #[test]
  fn a_new_channel_is_followed_first() {
    let s = Settings {
      channel: Some("stable".into()),
      mode: Mode::Immediate,
    };
    assert_eq!(
      next_step(&host(), &s, BASE, &[], false, true, 600),
      Step::Switch {
        image: format!("{BASE}:stable"),
        download_only: false
      }
    );
  }
}
