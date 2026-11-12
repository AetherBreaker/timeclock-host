//! Keyboard and touch input, read straight from the input devices without grabbing them.
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const LEFT_CTRL: u16 = 29;
const RIGHT_CTRL: u16 = 97;
const LEFT_ALT: u16 = 56;
const RIGHT_ALT: u16 = 100;
/// `struct input_event` on x86_64: a 16-byte timeval, then type (u16), code (u16), value (i32).
const EVENT_SIZE: usize = 24;

/// Ctrl and Alt across every device, so Ctrl+Alt+F<n> is recognized whichever keyboard types it.
#[derive(Debug, Default)]
pub struct KeyTracker {
  ctrl: [bool; 2],
  alt: [bool; 2],
}

impl KeyTracker {
  /// Feed one event; `Some(n)` when it's the press (not autorepeat) of F<n> with Ctrl and Alt held.
  pub fn feed(&mut self, ev_type: u16, code: u16, value: i32) -> Option<u16> {
    if ev_type != EV_KEY {
      return None;
    }
    let down = value != 0; // 1 press, 2 autorepeat, 0 release
    match code {
      LEFT_CTRL => self.ctrl[0] = down,
      RIGHT_CTRL => self.ctrl[1] = down,
      LEFT_ALT => self.alt[0] = down,
      RIGHT_ALT => self.alt[1] = down,
      _ if value == 1 && self.ctrl.contains(&true) && self.alt.contains(&true) => {
        return match code {
          59..=68 => Some(code - 58), // F1–F10
          87 => Some(11),
          88 => Some(12),
          _ => None,
        };
      }
      _ => {}
    }
    None
  }
}

/// Every `/dev/input/event*`, read-only and non-blocking. Reading doesn't grab: the displays still get every event.
pub struct Inputs {
  devices: Vec<(PathBuf, File)>,
  keys: KeyTracker,
}

impl Default for Inputs {
  fn default() -> Self {
    let mut inputs = Self {
      devices: Vec::new(),
      keys: KeyTracker::default(),
    };
    inputs.rescan();
    inputs
  }
}

impl Inputs {
  /// Open devices that appeared since the last scan; a keyboard that drops off USB comes back as a new one.
  pub fn rescan(&mut self) {
    let Ok(dir) = fs::read_dir("/dev/input") else { return };
    for path in dir.flatten().map(|e| e.path()) {
      let is_event = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("event"));
      if !is_event || self.devices.iter().any(|(p, _)| *p == path) {
        continue;
      }
      if let Ok(f) = File::options().read(true).custom_flags(libc::O_NONBLOCK).open(&path) {
        self.devices.push((path, f));
      }
    }
  }

  pub fn fds(&self) -> Vec<RawFd> {
    self.devices.iter().map(|(_, f)| f.as_raw_fd()).collect()
  }

  /// Read everything waiting: whether any of it was key, pointer or touch input, and the last Ctrl+Alt+F<n>.
  /// A device that fails to read (unplugged) is dropped, and the modifiers reset with it.
  pub fn drain(&mut self) -> (bool, Option<u16>) {
    let (mut input, mut vt_key, mut lost) = (false, None, false);
    let mut buf = [0u8; EVENT_SIZE * 64];
    let keys = &mut self.keys;
    self.devices.retain_mut(|(_, f)| {
      let keep = loop {
        match f.read(&mut buf) {
          Ok(0) => break false,
          Ok(n) => {
            for ev in buf[..n].as_chunks::<EVENT_SIZE>().0 {
              let ev_type = u16::from_ne_bytes([ev[16], ev[17]]);
              let code = u16::from_ne_bytes([ev[18], ev[19]]);
              let value = i32::from_ne_bytes([ev[20], ev[21], ev[22], ev[23]]);
              input |= matches!(ev_type, EV_KEY | EV_REL | EV_ABS);
              vt_key = keys.feed(ev_type, code, value).or(vt_key);
            }
          }
          Err(e) if e.kind() == ErrorKind::WouldBlock => break true,
          Err(e) if e.kind() == ErrorKind::Interrupted => {}
          Err(_) => break false,
        }
      };
      lost |= !keep;
      keep
    });
    if lost {
      *keys = KeyTracker::default();
    }
    (input, vt_key)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const EV_KEY: u16 = 1;
  const PRESS: i32 = 1;
  const RELEASE: i32 = 0;

  fn press_combo(k: &mut KeyTracker, ctrl: u16, alt: u16, f: u16) -> Option<u16> {
    k.feed(EV_KEY, ctrl, PRESS);
    k.feed(EV_KEY, alt, PRESS);
    let got = k.feed(EV_KEY, f, PRESS);
    k.feed(EV_KEY, f, RELEASE);
    k.feed(EV_KEY, alt, RELEASE);
    k.feed(EV_KEY, ctrl, RELEASE);
    got
  }

  #[test]
  fn ctrl_alt_f2_reports_2() {
    assert_eq!(press_combo(&mut KeyTracker::default(), 29, 56, 60), Some(2));
  }

  #[test]
  fn f2_without_alt_reports_nothing() {
    let mut k = KeyTracker::default();
    k.feed(EV_KEY, 29, PRESS);
    assert_eq!(k.feed(EV_KEY, 60, PRESS), None);
    let mut k = KeyTracker::default();
    assert_eq!(k.feed(EV_KEY, 60, PRESS), None);
  }

  #[test]
  fn right_ctrl_right_alt_count() {
    assert_eq!(press_combo(&mut KeyTracker::default(), 97, 100, 65), Some(7));
  }

  #[test]
  fn release_resets_modifiers() {
    let mut k = KeyTracker::default();
    press_combo(&mut k, 29, 56, 59);
    assert_eq!(k.feed(EV_KEY, 60, PRESS), None);
  }

  #[test]
  fn f11_and_f12_map_to_11_12() {
    assert_eq!(press_combo(&mut KeyTracker::default(), 29, 56, 87), Some(11));
    assert_eq!(press_combo(&mut KeyTracker::default(), 29, 56, 88), Some(12));
    assert_eq!(press_combo(&mut KeyTracker::default(), 29, 56, 68), Some(10));
  }

  #[test]
  fn autorepeat_is_not_a_new_press() {
    let mut k = KeyTracker::default();
    k.feed(EV_KEY, 29, PRESS);
    k.feed(EV_KEY, 56, PRESS);
    assert_eq!(k.feed(EV_KEY, 60, PRESS), Some(2));
    assert_eq!(k.feed(EV_KEY, 60, 2), None);
  }
}
