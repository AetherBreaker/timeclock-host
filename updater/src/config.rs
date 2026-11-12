//! `/etc/timeclock/updater.conf`: `key = value` lines and `#` comments; every key is optional.
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
  /// sway's control socket: the new host is good once it answers.
  pub sway_socket: PathBuf,
  /// The clock window's Wayland app id.
  pub clock_app_id: String,
  pub helper_image: String,
  pub docker_socket: PathBuf,
  /// The host image (a channel is a tag on it), the app's compose service (its container's variables set the updates),
  /// and the timings.
  pub host_image: String,
  pub app_service: String,
  pub update_check_every: Duration,
  pub update_idle: Duration,
  pub update_health_wait: Duration,
}

impl Default for Config {
  fn default() -> Self {
    Self {
      sway_socket: "/run/timeclock-display/sway.sock".into(),
      clock_app_id: "local.timeclock.Clock".into(),
      docker_socket: "/var/run/docker.sock".into(),
      helper_image: "coollabsio/coolify-helper".into(),
      host_image: "ghcr.io/aetherbreaker/timeclock-host".into(),
      app_service: "timeclock-system".into(),
      update_check_every: Duration::from_secs(60),
      update_idle: Duration::from_secs(60),
      update_health_wait: Duration::from_secs(600),
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
    let secs = || {
      value
        .parse::<u64>()
        .map_err(|_| format!("line {n}: {key}: not a whole number: {value}"))
    };
    match key {
      "sway_socket" => c.sway_socket = value.into(),
      "clock_app_id" => c.clock_app_id = value.into(),
      "helper_image" => c.helper_image = value.into(),
      "docker_socket" => c.docker_socket = value.into(),
      "host_image" => c.host_image = value.into(),
      "app_service" => c.app_service = value.into(),
      "update_check_secs" => c.update_check_every = Duration::from_secs(secs()?.max(10)),
      "update_idle_secs" => c.update_idle = Duration::from_secs(secs()?),
      "update_health_secs" => c.update_health_wait = Duration::from_secs(secs()?),
      _ => return Err(format!("line {n}: unknown key {key}")),
    }
  }
  Ok(c)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;
  use std::time::Duration;

  #[test]
  fn defaults_when_empty() {
    let c = parse_config("").unwrap();
    assert_eq!(c, Config::default());
    assert_eq!(c.host_image, "ghcr.io/aetherbreaker/timeclock-host");
    assert_eq!(c.app_service, "timeclock-system");
    assert_eq!(c.update_check_every, Duration::from_secs(60));
    assert_eq!(c.update_idle, Duration::from_secs(60));
    assert_eq!(c.update_health_wait, Duration::from_secs(600));
    assert_eq!(c.sway_socket, PathBuf::from("/run/timeclock-display/sway.sock"));
    assert_eq!(c.clock_app_id, "local.timeclock.Clock");
    assert_eq!(c.docker_socket, PathBuf::from("/var/run/docker.sock"));
    assert_eq!(c.helper_image, "coollabsio/coolify-helper");
  }

  #[test]
  fn parses_every_key() {
    let c = parse_config(
      "# comment\n\nhost_image = x/host:beta  # trailing\napp_service = app\nupdate_check_secs = 30\n\
       update_idle_secs = 5\nupdate_health_secs = 90\nsway_socket = /run/s.sock\nclock_app_id = x.Clock\n\
       docker_socket = /run/docker.sock\nhelper_image = x/helper\n",
    )
    .unwrap();
    assert_eq!(c.host_image, "x/host:beta");
    assert_eq!(c.app_service, "app");
    assert_eq!(c.update_check_every, Duration::from_secs(30));
    assert_eq!(c.update_idle, Duration::from_secs(5));
    assert_eq!(c.update_health_wait, Duration::from_secs(90));
    assert_eq!(c.sway_socket, PathBuf::from("/run/s.sock"));
    assert_eq!(c.clock_app_id, "x.Clock");
    assert_eq!(c.docker_socket, PathBuf::from("/run/docker.sock"));
    assert_eq!(c.helper_image, "x/helper");
  }

  #[test]
  fn check_interval_has_a_floor() {
    let c = parse_config("update_check_secs = 1").unwrap();
    assert_eq!(c.update_check_every, Duration::from_secs(10));
  }

  #[test]
  fn rejects_unknown_key() {
    assert!(parse_config("hostimage = x").unwrap_err().contains("hostimage"));
    // The watcher's keys aren't this program's.
    assert!(parse_config("flip = app-stopped").is_err());
  }

  #[test]
  fn rejects_bad_number_and_missing_equals() {
    assert!(parse_config("update_idle_secs = soon").is_err());
    assert!(parse_config("update_idle_secs 60").is_err());
  }
}
