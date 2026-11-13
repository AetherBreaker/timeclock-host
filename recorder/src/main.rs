//! The recorder's process. For now it only reads its settings.
use timeclock_recorder::config::{Config, parse_config};

const CONFIG: &str = "/etc/timeclock/recorder.conf";

fn main() {
  let cfg = match std::fs::read_to_string(CONFIG) {
    Ok(text) => parse_config(&text).unwrap_or_else(|e| {
      eprintln!("{CONFIG}: {e}; using the defaults");
      Config::default()
    }),
    Err(_) => Config::default(),
  };
  eprintln!(
    "timeclock-recorder: settings read, recording to {}",
    cfg.recordings_dir.display()
  );
}
