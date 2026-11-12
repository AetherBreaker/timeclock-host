//! The host updater, as its own service: keeps this bootc host on the newest release of its channel, applied at once or
//! in a daily window, as the app container's Coolify variables say (`HOST_UPDATE_CHANNEL`, `HOST_UPDATE_MODE`,
//! `HOST_UPDATE_WINDOW`). After an update it checks the new host comes up, and rolls back if it doesn't.
//!
//! Nothing here may panic: the release build aborts on a panic, which ends the service (systemd restarts it).
use crate::config::Config;
use crate::docker;
use crate::sway::{self, Json, parse_json};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
  (h < 24 && m < 60 && (1..=1440).contains(&len)).then(|| (h * 60 + m, len))
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
  /// A newer release seen by the last `bootc upgrade --check`, as the booted release's record has it.
  pub cached: Option<String>,
  pub has_rollback: bool,
  /// The rollback release's digest, and the newer release its record names.
  pub rollback: Option<String>,
  pub rollback_cached: Option<String>,
}

impl Host {
  /// The newer release the last check found. A check records it on the newest downloaded release, and status shows
  /// each release's own record: after a rollback the newest download is the bad release in the rollback slot, so its
  /// record is the current one (the booted release's stays on the bad release for good).
  pub fn cached_update(&self, bad: &[String]) -> Option<&str> {
    match &self.rollback {
      Some(r) if bad.contains(r) => self.rollback_cached.as_deref(),
      _ => self.cached.as_deref(),
    }
  }
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
    rollback: str_at(&j, &["status", "rollback", "image", "imageDigest"]),
    rollback_cached: str_at(&j, &["status", "rollback", "cachedUpdate", "imageDigest"]),
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
  match host.cached_update(bad) {
    Some(c) if c != host.booted && !bad.iter().any(|b| b == c) => Step::Stage { download_only: window },
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

/// The download's progress for the clock app, in the app's status folder.
const PROGRESS_FILE: &str = "host-update.json";

/// Bytes done and total of an image pull from one line of `bootc --progress-fd` output (JSON lines; format read from
/// bootc 1.17.1, crates/lib/src/progress_jsonl.rs: `{"type":"ProgressBytes",...,"bytes":N,"bytesTotal":M,...}`). Other
/// event types and unreadable lines are `None`.
pub fn parse_progress(line: &str) -> Option<(u64, u64)> {
  let j = parse_json(line)?;
  if j.get("type") != Some(&Json::Str("ProgressBytes".into())) {
    return None;
  }
  // `sway::Json` keeps no number values, so read them from the text. The task's own `bytes` precedes `subtasks`, whose
  // entries carry the same keys, and `"bytes":` can't match `"bytesCached":`.
  let num = |key: &str| {
    let digits: String = line
      .split_once(key)?
      .1
      .chars()
      .take_while(char::is_ascii_digit)
      .collect();
    digits.parse::<u64>().ok()
  };
  Some((num(r#""bytes":"#)?, num(r#""bytesTotal":"#)?))
}

/// The contents of `host-update.json`; `at` is unix seconds.
pub fn progress_json(downloaded: bool, done: u64, total: u64, mode: Mode, at: u64) -> String {
  let state = if downloaded { "downloaded" } else { "downloading" };
  let mode = match mode {
    Mode::Immediate => r#""immediate","window_start":null"#.to_string(),
    Mode::Window(start, _) => format!(r#""window","window_start":"{:02}:{:02}""#, start / 60, start % 60),
  };
  format!(r#"{{"state":"{state}","done":{done},"total":{total},"mode":{mode},"at":{at}}}"#)
}

/// Whether a bootc that doesn't know `--progress-fd` (clap's "unexpected argument") refused the command.
pub fn refused_progress_fd(stderr: &str) -> bool {
  stderr.contains("unexpected argument '--progress-fd'")
}

/// Replace `<status_dir>/host-update.json` atomically (temp file, rename), mode 0644. `Ok` and nothing written when
/// the folder doesn't exist: no app to show it to.
pub fn write_progress(status_dir: &Path, json: &str) -> io::Result<()> {
  if !status_dir.is_dir() {
    return Ok(());
  }
  let tmp = status_dir.join(".host-update.json.tmp");
  let result = (|| {
    let mut f = fs::OpenOptions::new()
      .write(true)
      .create(true)
      .truncate(true)
      .mode(0o644)
      .open(&tmp)?;
    f.write_all(json.as_bytes())?;
    f.set_permissions(fs::Permissions::from_mode(0o644))?;
    fs::rename(&tmp, status_dir.join(PROGRESS_FILE))
  })();
  if result.is_err() {
    let _ = fs::remove_file(&tmp);
  }
  result
}

/// `bootc <args>` (`args[0]` is `upgrade` or `switch`) with `--progress-fd 3` on a pipe, keeping the progress file
/// current while it runs: `downloading` at most once a second, `downloaded` once on success, removed on failure. A bootc
/// too old for the flag is rerun without it. The state is rewritten at least every minute while bootc talks (the app
/// drops a `downloading` file untouched for 5), and the final byte event is never throttled away.
fn run_with_progress(program: &str, cfg: &Config, mode: Mode, args: &[&str]) -> Result<String, String> {
  let file = cfg.status_dir.join(PROGRESS_FILE);
  let Some((sub, rest)) = args.split_first() else {
    return run(program, args);
  };
  let mut fds = [0 as libc::c_int; 2];
  // SAFETY: `fds` holds the two ints pipe2 fills in. O_CLOEXEC keeps both ends out of the child except the dup2'd 3.
  if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
    return run(program, args);
  }
  // SAFETY: pipe2 succeeded, so these are open fds nothing else owns; OwnedFd closes them on every path out.
  let (read_end, write_end) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
  let wfd = write_end.as_raw_fd();
  let mut cmd = Command::new(program);
  cmd
    .arg(sub)
    .args(["--progress-fd", "3"])
    .args(rest)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
  // SAFETY: the closure runs between fork and exec and only calls async-signal-safe dup2/fcntl.
  unsafe {
    cmd.pre_exec(move || {
      // dup2 clears close-on-exec on the copy; if the pipe already is fd 3 clear it by hand.
      let ok = if wfd == 3 {
        libc::fcntl(3, libc::F_SETFD, 0) != -1
      } else {
        libc::dup2(wfd, 3) != -1
      };
      if ok { Ok(()) } else { Err(io::Error::last_os_error()) }
    });
  }
  let mut child = match cmd.spawn() {
    Ok(c) => c,
    Err(e) => {
      let _ = fs::remove_file(&file);
      return Err(format!("{program}: {e}"));
    }
  };
  drop(write_end);
  let drain = |pipe: Option<Box<dyn Read + Send>>| match pipe {
    None => Ok(None),
    Some(mut p) => thread::Builder::new()
      .spawn(move || {
        let mut buf = Vec::new();
        let _ = p.read_to_end(&mut buf);
        buf
      })
      .map(Some),
  };
  let (so, se) = (child.stdout.take(), child.stderr.take());
  let (out, err) = match (drain(so.map(|p| Box::new(p) as _)), drain(se.map(|p| Box::new(p) as _))) {
    (Ok(out), Ok(err)) => (out, err),
    _ => {
      // No thread to drain with: end the command (a drain thread that did start ends at EOF) and run it plainly.
      let _ = child.kill();
      let _ = child.wait();
      let _ = fs::remove_file(&file);
      return run(program, args);
    }
  };
  let now = || SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
  let mut last = (0, 0);
  let mut written: Option<Instant> = None;
  let mut lines = BufReader::new(fs::File::from(read_end));
  let mut line = Vec::new();
  loop {
    line.clear();
    if !matches!(lines.read_until(b'\n', &mut line), Ok(n) if n > 0) {
      break;
    }
    let parsed = parse_progress(&String::from_utf8_lossy(&line));
    if let Some(p) = parsed {
      last = p;
    }
    let age = written.map(|t| t.elapsed());
    let byte_event_due = parsed.is_some() && (last.0 >= last.1 || age.is_none_or(|a| a >= Duration::from_secs(1)));
    let stale = age.is_some_and(|a| a >= Duration::from_secs(60));
    if last.1 > 0 && (byte_event_due || stale) {
      let _ = write_progress(&cfg.status_dir, &progress_json(false, last.0, last.1, mode, now()));
      written = Some(Instant::now());
    }
  }
  // After a read error bootc may still write: drain so it can't block.
  let _ = io::copy(&mut lines, &mut io::sink());
  let status = child.wait();
  let join = |h: Option<thread::JoinHandle<Vec<u8>>>| h.and_then(|h| h.join().ok()).unwrap_or_default();
  let (stdout, stderr) = (join(out), join(err));
  let (stdout, stderr) = (String::from_utf8_lossy(&stdout), String::from_utf8_lossy(&stderr));
  match status {
    Ok(s) if s.success() => {
      let _ = write_progress(&cfg.status_dir, &progress_json(true, last.1, last.1, mode, now()));
      Ok(stdout.into_owned())
    }
    status => {
      let _ = fs::remove_file(&file);
      if status.is_ok() && refused_progress_fd(&stderr) {
        return run(program, args);
      }
      let why = status.map_or_else(|e| e.to_string(), |s| s.to_string());
      Err(format!(
        "{program} {}: {why}: {}{}",
        args.join(" "),
        stdout.trim(),
        stderr.trim()
      ))
    }
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
  // Last boot's file (kept through its restart): gone before the app can show it.
  let _ = fs::remove_file(cfg.status_dir.join(PROGRESS_FILE));
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
      Ok((Step::Stay | Step::Check, host)) => {
        // A `downloaded` file with nothing (usable) staged would have the app promise a restart forever.
        if host.staged.as_ref().is_none_or(|(digest, _)| bad.contains(digest)) {
          let _ = fs::remove_file(cfg.status_dir.join(PROGRESS_FILE));
        }
        Ok(())
      }
      Ok((Step::Switch { image, download_only }, _)) => {
        eprintln!("host update: following {image}");
        let mut args = vec!["switch", "--quiet"];
        if download_only {
          args.push("--download-only");
        }
        args.push(&image);
        run_with_progress("bootc", &cfg, settings.mode, &args).map(drop)
      }
      Ok((Step::Stage { download_only }, host)) => {
        eprintln!("host update: downloading {}", host.cached_update(&bad).unwrap_or("?"));
        let args: &[&str] = if download_only {
          &["upgrade", "--quiet", "--download-only"]
        } else {
          &["upgrade", "--quiet"]
        };
        run_with_progress("bootc", &cfg, settings.mode, args).map(drop)
      }
      Ok((Step::Apply { from_downloaded }, host)) => {
        let digest = host.staged.map(|(d, _)| d).unwrap_or_default();
        eprintln!("host update: restarting into {digest}");
        // The progress file stays, so the clock's toast does until the screen goes dark. The next boot's updater
        // removes it as it starts, before Docker starts the app (the unit is ordered Before=docker.service).
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
    "rollback":{"image":{"imageDigest":"sha256:000"},"cachedUpdate":{"imageDigest":"sha256:ccc"}},
    "rollbackQueued":false,"type":"bootcHost"}}"#;

  #[test]
  fn reads_bootc_status() {
    let h = parse_status(STATUS).unwrap();
    assert_eq!(h.image, "ghcr.io/aetherbreaker/timeclock-host:latest");
    assert_eq!(h.booted, "sha256:aaa");
    assert_eq!(h.cached.as_deref(), Some("sha256:bbb"));
    assert_eq!(h.staged, None);
    assert!(h.has_rollback);
    assert_eq!(h.rollback.as_deref(), Some("sha256:000"));
    assert_eq!(h.rollback_cached.as_deref(), Some("sha256:ccc"));
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

  /// The dev clock on 2026-11-12: rolled back from sha256:bad, the booted release's record still names sha256:bad
  /// while the check's finding (sha256:ccc) sits on the rolled-back release.
  #[test]
  fn after_a_rollback_a_newer_release_is_still_taken() {
    let s = Settings::default();
    let bad = ["sha256:bad".to_string()];
    let rolled_back = Host {
      cached: Some("sha256:bad".into()),
      has_rollback: true,
      rollback: Some("sha256:bad".into()),
      rollback_cached: Some("sha256:ccc".into()),
      ..host()
    };
    assert_eq!(rolled_back.cached_update(&bad), Some("sha256:ccc"));
    assert_eq!(
      next_step(&rolled_back, &s, BASE, &bad, true, true, 600),
      Step::Stage { download_only: false }
    );
    let nothing_newer = Host {
      rollback_cached: None,
      ..rolled_back.clone()
    };
    assert_eq!(next_step(&nothing_newer, &s, BASE, &bad, true, true, 600), Step::Stay);
    // A good release in the rollback slot (an ordinary update) keeps the booted release's record.
    let updated = Host {
      cached: None,
      has_rollback: true,
      rollback: Some("sha256:old".into()),
      rollback_cached: Some("sha256:zzz".into()),
      ..host()
    };
    assert_eq!(next_step(&updated, &s, BASE, &bad, true, true, 600), Step::Stay);
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

  // bootc writes canonical JSON: keys sorted, so `bytes` and `bytesTotal` precede `subtasks` (whose entries repeat them).
  const BYTES_LINE: &str = r#"{"bytes":312000000,"bytesCached":0,"bytesTotal":740000000,"description":"Pulling Image","id":"sha256:bbb","steps":3,"stepsCached":0,"stepsTotal":9,"subtasks":[{"bytes":5,"bytesCached":0,"bytesTotal":6,"description":"x","id":"l1","subtask":"layer"}],"task":"pulling","type":"ProgressBytes"}"#;

  #[test]
  fn reads_bootc_progress_lines() {
    assert_eq!(parse_progress(BYTES_LINE), Some((312_000_000, 740_000_000)));
    let steps = r#"{"type":"ProgressSteps","task":"staging","description":"d","id":"i","stepsCached":0,"steps":1,"stepsTotal":3,"subtasks":[]}"#;
    assert_eq!(parse_progress(steps), None);
    assert_eq!(parse_progress(r#"{"type":"Start","version":"0.1.0"}"#), None);
    assert_eq!(parse_progress("not json"), None);
    assert_eq!(parse_progress(""), None);
  }

  #[test]
  fn progress_file_text() {
    assert_eq!(
      progress_json(false, 312_000_000, 740_000_000, Mode::Window(180, 60), 1_762_905_600),
      r#"{"state":"downloading","done":312000000,"total":740000000,"mode":"window","window_start":"03:00","at":1762905600}"#
    );
    assert_eq!(
      progress_json(true, 5, 5, Mode::Immediate, 7),
      r#"{"state":"downloaded","done":5,"total":5,"mode":"immediate","window_start":null,"at":7}"#
    );
  }

  #[test]
  fn spots_an_old_bootc_refusing_the_progress_flag() {
    assert!(refused_progress_fd(
      "error: unexpected argument '--progress-fd' found\n\nUsage: bootc upgrade"
    ));
    assert!(!refused_progress_fd("error: pulling image: registry unreachable"));
  }

  #[test]
  fn writes_the_progress_file_only_where_there_is_a_folder() {
    use std::os::unix::fs::PermissionsExt;
    let base = std::env::temp_dir().join(format!("tc-progress-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let dir = base.join("status");
    assert!(write_progress(&dir, "{}").is_ok());
    assert!(!dir.exists());
    fs::create_dir_all(&dir).unwrap();
    write_progress(&dir, "one").unwrap();
    write_progress(&dir, "two").unwrap();
    let names: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names, vec![std::ffi::OsString::from("host-update.json")]);
    let file = dir.join("host-update.json");
    assert_eq!(fs::read_to_string(&file).unwrap(), "two");
    assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o644);
    let _ = fs::remove_dir_all(&base);
  }

  #[test]
  fn a_window_hour_past_1092_is_no_window() {
    assert_eq!(parse_window("1093:00/60m"), None);
  }

  /// A fake `bootc` (a shell script) under a fresh folder, plus a config whose status folder is inside it. Tests that
  /// write and then exec scripts hold `SCRIPTS`: a fork in another test thread would otherwise carry the script's
  /// still-open write fd into its child and fail the exec with ETXTBSY.
  fn fake_bootc(name: &str, body: &str) -> (std::path::PathBuf, Config) {
    use std::os::unix::fs::PermissionsExt;
    let base = std::env::temp_dir().join(format!("tc-fake-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(base.join("status")).unwrap();
    let script = base.join("bootc");
    fs::write(
      &script,
      format!("#!/bin/sh\nSTATUS={}\n{body}\n", base.join("status").display()),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = Config {
      status_dir: base.join("status"),
      ..Config::default()
    };
    (base, cfg)
  }

  static SCRIPTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

  const PULL: &str = r#"echo '{"type":"Start","version":"0.1.0"}' >&3
echo '{"bytes":5,"bytesTotal":9,"type":"ProgressBytes"}' >&3
echo '{"bytes":9,"bytesTotal":9,"type":"ProgressBytes"}' >&3
echo '{"steps":1,"stepsTotal":2,"type":"ProgressSteps"}' >&3"#;

  #[test]
  fn progress_runs_end_to_end_and_ends_downloaded() {
    let _guard = SCRIPTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // The snapshot is taken while the script still runs: the 100% line (within a second of the 5/9 write) must be there.
    // The script waits up to 5 s for it rather than a fixed pause, which a loaded build machine outran.
    let (base, cfg) = fake_bootc(
      "ok",
      &format!(
        "{PULL}\nfor _ in $(seq 50); do grep -q '\"done\":9' \"$STATUS/host-update.json\" && break; sleep 0.1; done\n\
         cp \"$STATUS/host-update.json\" \"$STATUS/../snap\"\necho \"$@\"\necho warn >&2"
      ),
    );
    let program = base.join("bootc");
    let out = run_with_progress(
      program.to_str().unwrap(),
      &cfg,
      Mode::Immediate,
      &["upgrade", "--quiet"],
    )
    .unwrap();
    assert_eq!(out, "upgrade --progress-fd 3 --quiet\n");
    let snap = fs::read_to_string(base.join("snap")).unwrap();
    assert!(snap.contains(r#""state":"downloading","done":9,"total":9"#), "{snap}");
    let last = fs::read_to_string(cfg.status_dir.join(PROGRESS_FILE)).unwrap();
    assert!(last.contains(r#""state":"downloaded","done":9,"total":9"#), "{last}");
    let _ = fs::remove_dir_all(&base);
  }

  #[test]
  fn a_failed_command_leaves_no_progress_file() {
    let _guard = SCRIPTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (base, cfg) = fake_bootc("fail", &format!("{PULL}\necho boom >&2\nexit 1"));
    let program = base.join("bootc");
    let err = run_with_progress(program.to_str().unwrap(), &cfg, Mode::Immediate, &["upgrade"]).unwrap_err();
    assert!(err.contains("boom"), "{err}");
    assert!(!cfg.status_dir.join(PROGRESS_FILE).exists());
    // A spawn failure removes a leftover file too.
    write_progress(&cfg.status_dir, "old").unwrap();
    let missing = base.join("no-such-bootc");
    assert!(run_with_progress(missing.to_str().unwrap(), &cfg, Mode::Immediate, &["upgrade"]).is_err());
    assert!(!cfg.status_dir.join(PROGRESS_FILE).exists());
    let _ = fs::remove_dir_all(&base);
  }

  #[test]
  fn a_bootc_refusing_the_flag_is_rerun_without_it() {
    let _guard = SCRIPTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (base, cfg) = fake_bootc(
      "old",
      "case \"$*\" in *--progress-fd*) echo \"error: unexpected argument '--progress-fd' found\" >&2; exit 2;; esac\necho \"ran $*\"",
    );
    let program = base.join("bootc");
    let out = run_with_progress(
      program.to_str().unwrap(),
      &cfg,
      Mode::Immediate,
      &["upgrade", "--quiet"],
    )
    .unwrap();
    assert_eq!(out, "ran upgrade --quiet\n");
    assert!(!cfg.status_dir.join(PROGRESS_FILE).exists());
    let _ = fs::remove_dir_all(&base);
  }
}
