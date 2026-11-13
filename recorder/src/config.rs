//! `/etc/timeclock/recorder.conf`: `key = value` lines and `#` comments; every key is optional.
use crate::clock::Micros;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
  pub recordings_dir: PathBuf,
  /// The app's status folder (created by the app; the recorder never creates it).
  pub status_dir: PathBuf,
  pub sway_socket: PathBuf,
  pub wayland_socket: PathBuf,
  /// The clock window's Wayland app id.
  pub clock_app_id: String,
  /// Footage kept from before an interaction, and after it.
  pub before: Micros,
  pub after: Micros,
  pub max_fps: u32,
  /// A freeze is this many identical frames spanning at least `freeze`.
  pub freeze_frames: u32,
  pub freeze: Micros,
  /// The recordings folder's size limit, in bytes (the file's `cap_gb` is decimal gigabytes).
  pub cap_bytes: u64,
  pub keyframe: Micros,
  pub max_clip: Micros,
}

impl Default for Config {
  fn default() -> Self {
    Self {
      recordings_dir: "/data/timeclock_system_files/recordings".into(),
      status_dir: "/data/timeclock_system_files/status".into(),
      sway_socket: "/run/timeclock-display/sway.sock".into(),
      wayland_socket: "/run/user/0/wayland-1".into(),
      clock_app_id: "local.timeclock.Clock".into(),
      before: 15_000_000,
      after: 15_000_000,
      max_fps: 30,
      freeze_frames: 15,
      freeze: 1_000_000,
      cap_bytes: 40_000_000_000,
      keyframe: 2_000_000,
      max_clip: 600_000_000,
    }
  }
}

/// Parse the config file's text; an unknown key, a bad value or a line without `=` is an error naming the line.
pub fn parse_config(text: &str) -> Result<Config, String> {
  let mut c = Config::default();
  for (i, raw) in text.lines().enumerate() {
    let line = raw.split('#').next().unwrap_or("").trim();
    if line.is_empty() {
      continue;
    }
    let n = i + 1;
    let Some((key, value)) = line.split_once('=') else {
      return Err(format!("line {n}: expected `key = value`"));
    };
    let (key, value) = (key.trim(), value.trim());
    let whole = || {
      value
        .parse::<u32>()
        .map_err(|_| format!("line {n}: {key}: not a whole number: {value}"))
    };
    let micros = |secs: u32| i64::from(secs) * 1_000_000;
    match key {
      "recordings_dir" => c.recordings_dir = value.into(),
      "status_dir" => c.status_dir = value.into(),
      "sway_socket" => c.sway_socket = value.into(),
      "wayland_socket" => c.wayland_socket = value.into(),
      "clock_app_id" => c.clock_app_id = value.into(),
      "before_secs" => c.before = micros(whole()?),
      "after_secs" => c.after = micros(whole()?),
      "max_fps" => c.max_fps = whole()?,
      "freeze_frames" => c.freeze_frames = whole()?,
      "freeze_secs" => c.freeze = micros(whole()?),
      "cap_gb" => c.cap_bytes = u64::from(whole()?) * 1_000_000_000,
      "keyframe_secs" => c.keyframe = micros(whole()?),
      "max_clip_secs" => c.max_clip = micros(whole()?),
      _ => return Err(format!("line {n}: unknown key {key}")),
    }
  }
  Ok(c)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn defaults_when_empty() {
    let c = parse_config("").unwrap();
    assert_eq!(c, Config::default());
    assert_eq!(
      c.recordings_dir,
      PathBuf::from("/data/timeclock_system_files/recordings")
    );
    assert_eq!(c.status_dir, PathBuf::from("/data/timeclock_system_files/status"));
    assert_eq!(c.sway_socket, PathBuf::from("/run/timeclock-display/sway.sock"));
    assert_eq!(c.wayland_socket, PathBuf::from("/run/user/0/wayland-1"));
    assert_eq!(c.clock_app_id, "local.timeclock.Clock");
    assert_eq!((c.before, c.after), (15_000_000, 15_000_000));
    assert_eq!((c.max_fps, c.freeze_frames, c.freeze), (30, 15, 1_000_000));
    assert_eq!(
      (c.cap_bytes, c.keyframe, c.max_clip),
      (40_000_000_000, 2_000_000, 600_000_000)
    );
  }

  #[test]
  fn parses_every_key() {
    let c = parse_config(
      "# comment\n\nrecordings_dir = /r  # trailing\nstatus_dir = /s\nsway_socket = /sw\nwayland_socket = /wl\n\
       clock_app_id = x.Clock\nbefore_secs = 3\nafter_secs = 4\nmax_fps = 10\nfreeze_frames = 5\nfreeze_secs = 2\n\
       cap_gb = 7\nkeyframe_secs = 1\nmax_clip_secs = 60\n",
    )
    .unwrap();
    assert_eq!(c.recordings_dir, PathBuf::from("/r"));
    assert_eq!(c.status_dir, PathBuf::from("/s"));
    assert_eq!(c.sway_socket, PathBuf::from("/sw"));
    assert_eq!(c.wayland_socket, PathBuf::from("/wl"));
    assert_eq!(c.clock_app_id, "x.Clock");
    assert_eq!((c.before, c.after), (3_000_000, 4_000_000));
    assert_eq!((c.max_fps, c.freeze_frames, c.freeze), (10, 5, 2_000_000));
    assert_eq!(
      (c.cap_bytes, c.keyframe, c.max_clip),
      (7_000_000_000, 1_000_000, 60_000_000)
    );
  }

  #[test]
  fn errors_name_the_line() {
    assert!(
      parse_config("max_fps = 30\nbogus = 1")
        .unwrap_err()
        .starts_with("line 2: unknown key bogus")
    );
    assert!(
      parse_config("\nmax_fps = fast")
        .unwrap_err()
        .starts_with("line 2: max_fps: not a whole number")
    );
    assert!(parse_config("# x\n# y\nmax_fps 30").unwrap_err().starts_with("line 3:"));
  }
}
