//! Clip open/split/close rules.
use crate::clock::Micros;

#[derive(Debug, PartialEq, Eq)]
pub enum ClipCmd {
  Open { at: Micros },
  Close,
  Split,
}

/// Whether frames should be captured: a sleeping screen is skipped unless a clip is still open.
pub fn capturing(window_shown: bool, asleep: bool, clip_open: bool) -> bool {
  window_shown && (!asleep || clip_open)
}

pub struct ClipTracker {
  after: Micros,
  max_clip: Micros,
  open: bool,
  /// Start of the current segment of footage; `None` until `opened` reports it (also stops a repeated `Split`).
  footage_start: Option<Micros>,
  last: Micros,
  freezing: bool,
}

impl ClipTracker {
  pub fn new(after: Micros, max_clip: Micros) -> Self {
    Self {
      after,
      max_clip,
      open: false,
      footage_start: None,
      last: 0,
      freezing: false,
    }
  }

  pub fn is_open(&self) -> bool {
    self.open
  }

  pub fn interaction(&mut self, at: Micros) -> Option<ClipCmd> {
    self.last = self.last.max(at);
    if self.open {
      return None;
    }
    self.open = true;
    self.footage_start = None;
    self.freezing = false;
    self.last = at;
    Some(ClipCmd::Open { at })
  }

  /// Motion extends an open clip but never opens one.
  pub fn activity(&mut self, at: Micros) {
    if self.open {
      self.last = self.last.max(at);
    }
  }

  /// A freeze holds the clip open; the `after` countdown restarts when it ends.
  pub fn freeze(&mut self, active: bool, at: Micros) {
    self.freezing = active;
    self.activity(at);
  }

  /// The clip's real start (pre-roll included), after `Open` or `Split`.
  pub fn opened(&mut self, footage_start: Micros) {
    self.footage_start = Some(footage_start);
  }

  pub fn poll(&mut self, now: Micros) -> Option<ClipCmd> {
    if !self.open {
      return None;
    }
    if !self.freezing && now - self.last >= self.after {
      self.open = false;
      return Some(ClipCmd::Close);
    }
    if self.footage_start.is_some_and(|s| now - s >= self.max_clip) {
      self.footage_start = None;
      return Some(ClipCmd::Split);
    }
    None
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const S: Micros = 1_000_000;

  fn open_tracker() -> ClipTracker {
    let mut t = ClipTracker::new(15 * S, 600 * S);
    assert_eq!(t.interaction(0), Some(ClipCmd::Open { at: 0 }));
    t.opened(-15 * S);
    t
  }

  #[test]
  fn second_interaction_extends() {
    let mut t = open_tracker();
    assert_eq!(t.interaction(10 * S), None);
    assert_eq!(t.poll(24 * S), None);
    assert_eq!(t.poll(25 * S), Some(ClipCmd::Close));
    assert!(!t.is_open());
  }

  #[test]
  fn quiet_closes_after_15s() {
    let mut t = open_tracker();
    assert_eq!(t.poll(15 * S - 1), None);
    assert_eq!(t.poll(15 * S), Some(ClipCmd::Close));
  }

  #[test]
  fn freeze_holds_the_clip() {
    let mut t = open_tracker();
    t.freeze(true, 5 * S);
    assert_eq!(t.poll(100 * S), None);
    t.freeze(false, 100 * S);
    assert_eq!(t.poll(115 * S - 1), None);
    assert_eq!(t.poll(115 * S), Some(ClipCmd::Close));
  }

  #[test]
  fn motion_extends_but_never_opens() {
    let mut t = ClipTracker::new(15 * S, 600 * S);
    t.activity(0);
    assert!(!t.is_open());
    assert_eq!(t.poll(100 * S), None);
    let mut t = open_tracker();
    t.activity(10 * S);
    assert_eq!(t.poll(24 * S), None);
    assert_eq!(t.poll(25 * S), Some(ClipCmd::Close));
  }

  #[test]
  fn splits_at_max_clip_and_stays_open() {
    let mut t = open_tracker();
    t.freeze(true, 0);
    assert_eq!(t.poll(584 * S), None);
    assert_eq!(t.poll(585 * S), Some(ClipCmd::Split));
    assert_eq!(t.poll(586 * S), None); // no repeat before opened()
    assert!(t.is_open());
    t.opened(585 * S);
    assert_eq!(t.poll(1184 * S), None);
    assert_eq!(t.poll(1185 * S), Some(ClipCmd::Split));
  }

  #[test]
  fn capturing_truth_table() {
    for w in [false, true] {
      for a in [false, true] {
        for c in [false, true] {
          assert_eq!(capturing(w, a, c), w && (!a || c), "{w} {a} {c}");
        }
      }
    }
  }
}
