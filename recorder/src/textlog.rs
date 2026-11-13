//! The clip text log: `<YYYY-MM-DD HH:MM:SS.ffffff> <event>` lines, and a short memory of recent ones for the lead-in.
use crate::clock::{Micros, local_stamp};
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

/// Event lines from the last `span` µs (the clip's `before` window plus a keyframe), kept so a clip that opens late
/// can still log what led up to it.
pub struct Recent {
  span: Micros,
  lines: VecDeque<(Micros, String)>,
}

impl Recent {
  pub fn new(span: Micros) -> Self {
    Self {
      span,
      lines: VecDeque::new(),
    }
  }

  pub fn push(&mut self, at: Micros, line: String) {
    self.lines.push_back((at, line));
    while self.lines.front().is_some_and(|(t, _)| *t < at - self.span) {
      self.lines.pop_front();
    }
  }

  pub fn since(&self, t: Micros) -> Vec<(Micros, String)> {
    self.lines.iter().filter(|(at, _)| *at >= t).cloned().collect()
  }
}

/// Appends one line to `<dir>/<name>.txt`.
pub fn append(dir: &Path, name: &str, at: Micros, event: &str) -> io::Result<()> {
  let mut file = OpenOptions::new()
    .create(true)
    .append(true)
    .open(dir.join(format!("{name}.txt")))?;
  writeln!(file, "{} {event}", local_stamp(at))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testutil::temp_dir;

  #[test]
  fn since_returns_only_lines_at_or_after_t() {
    let mut r = Recent::new(10_000);
    r.push(1_000, "a".into());
    r.push(2_000, "b".into());
    r.push(3_000, "c".into());
    assert_eq!(r.since(2_000), [(2_000, "b".to_string()), (3_000, "c".to_string())]);
  }

  #[test]
  fn lines_older_than_the_span_are_dropped() {
    let mut r = Recent::new(1_000);
    r.push(1_000, "a".into());
    r.push(1_500, "b".into());
    r.push(2_600, "c".into());
    assert_eq!(r.since(0), [(2_600, "c".to_string())]);
  }

  #[test]
  fn append_writes_stamped_lines() {
    let dir = temp_dir("textlog");
    append(&dir, "clip", 1_763_039_702_123_456, "clip start").unwrap();
    append(&dir, "clip", 1_763_039_703_000_000, "key").unwrap();
    let text = std::fs::read_to_string(dir.join("clip.txt")).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert!(lines[0].ends_with(":02.123456 clip start"), "{}", lines[0]);
    assert!(lines[1].ends_with(":03.000000 key"), "{}", lines[1]);
    std::fs::remove_dir_all(dir).unwrap();
  }
}
