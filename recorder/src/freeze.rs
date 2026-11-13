//! Freeze detection from the heartbeat block's beat (flipped ~4 times a second by the clock app).
use crate::clock::Micros;

#[derive(Debug, PartialEq, Eq)]
pub enum FreezeChange {
  /// `at` is the last frame where the beat changed.
  Started { at: Micros },
  /// `at` is the frame where the beat changed again; `length = at - Started.at`.
  Ended { at: Micros, length: Micros },
}

/// Frames repeat the same beat while the page is frozen. Armed only by the first change since `reset`, so a
/// page that never starts beating (or an old app without the block) is never reported frozen.
pub struct FreezeDetector {
  frames: u32,
  secs: Micros,
  beat: Option<u32>,
  armed: bool,
  last_change: Micros,
  same_frames: u32,
  frozen: bool,
}

impl FreezeDetector {
  pub fn new(frames: u32, secs: Micros) -> Self {
    Self {
      frames,
      secs,
      beat: None,
      armed: false,
      last_change: 0,
      same_frames: 0,
      frozen: false,
    }
  }

  /// The window was shown again: forget everything.
  pub fn reset(&mut self) {
    *self = Self::new(self.frames, self.secs);
  }

  pub fn frozen(&self) -> bool {
    self.frozen
  }

  pub fn on_frame(&mut self, at: Micros, beat: u32) -> Option<FreezeChange> {
    let previous = self.beat.replace(beat)?; // the first frame only sets the baseline
    if previous != beat {
      let ended = self.frozen.then(|| FreezeChange::Ended {
        at,
        length: at - self.last_change,
      });
      self.armed = true;
      self.frozen = false;
      self.last_change = at;
      self.same_frames = 0;
      return ended;
    }
    if self.armed && !self.frozen {
      self.same_frames += 1;
      if self.same_frames >= self.frames {
        self.frozen = true;
        return Some(FreezeChange::Started { at: self.last_change });
      }
    }
    None
  }

  /// Time-based fallback for when no frames arrive (an idle screen yields none).
  pub fn on_tick(&mut self, now: Micros) -> Option<FreezeChange> {
    if self.armed && !self.frozen && now - self.last_change >= self.secs {
      self.frozen = true;
      return Some(FreezeChange::Started { at: self.last_change });
    }
    None
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const FRAME: Micros = 33_333;
  const SEC: Micros = 1_000_000;

  fn armed() -> FreezeDetector {
    let mut d = FreezeDetector::new(15, SEC);
    assert_eq!(d.on_frame(0, 0), None);
    assert_eq!(d.on_frame(FRAME, 1), None); // first change arms it
    d
  }

  #[test]
  fn never_changed_never_freezes() {
    let mut d = FreezeDetector::new(15, SEC);
    for i in 0..400 {
      assert_eq!(d.on_frame(i * FRAME, 0), None);
    }
    assert_eq!(d.on_tick(10 * SEC), None);
    assert!(!d.frozen());
  }

  #[test]
  fn normal_heartbeat_never_freezes() {
    let mut d = FreezeDetector::new(15, SEC);
    for i in 0..300 {
      assert_eq!(d.on_frame(i * FRAME, (i / 8) as u32 % 2), None);
    }
    assert!(!d.frozen());
  }

  #[test]
  fn fifteen_same_frames_start_a_freeze() {
    let mut d = armed();
    for i in 2..16 {
      assert_eq!(d.on_frame(i * FRAME, 1), None);
    }
    assert_eq!(d.on_frame(16 * FRAME, 1), Some(FreezeChange::Started { at: FRAME }));
    assert!(d.frozen());
  }

  #[test]
  fn fourteen_same_frames_do_not() {
    let mut d = armed();
    for i in 2..16 {
      assert_eq!(d.on_frame(i * FRAME, 1), None);
    }
    assert!(!d.frozen());
  }

  #[test]
  fn tick_starts_and_a_new_beat_ends() {
    let mut d = armed();
    assert_eq!(d.on_tick(FRAME + SEC - 1), None);
    assert_eq!(d.on_tick(FRAME + SEC), Some(FreezeChange::Started { at: FRAME }));
    assert_eq!(d.on_tick(FRAME + 2 * SEC), None);
    assert_eq!(
      d.on_frame(FRAME + 3 * SEC, 0),
      Some(FreezeChange::Ended {
        at: FRAME + 3 * SEC,
        length: 3 * SEC
      })
    );
    assert!(!d.frozen());
  }

  #[test]
  fn reset_disarms() {
    let mut d = armed();
    d.reset();
    assert_eq!(d.on_tick(10 * SEC), None);
  }
}
