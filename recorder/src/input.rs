//! Input devices turned into interactions: touches, clicks, key presses and pointer motion. Read-only, never grabbed.
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use crate::clock::Micros;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const BTN_MISC: u16 = 0x100;
const BTN_LEFT: u16 = 0x110;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const INPUT_PROP_DIRECT: u16 = 1;
/// Contact slots tracked per touchscreen; a higher slot number is ignored.
const SLOTS: usize = 16;
/// `struct input_event` on x86_64: a 16-byte timeval, then type (u16), code (u16), value (i32).
const EVENT_SIZE: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
  Touchscreen,
  AbsPointer,
  RelPointer,
  Keyboard,
  Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
  Touch { at: Micros, x: i32, y: i32 },
  Click { at: Micros, pos: Option<(i32, i32)> },
  Key { at: Micros },
  Motion { at: Micros },
}

fn has(bitmap: &[u8], bit: u16) -> bool {
  bitmap.get(bit as usize / 8).is_some_and(|b| b & (1 << (bit % 8)) != 0)
}

/// Sort a device by its capability bitmaps, as `EVIOCGPROP` / `EVIOCGBIT` return them. Pointers are checked before
/// keyboards, since a mouse's buttons are keys too; any key below `BTN_MISC` makes a keyboard, so a lone power
/// button counts.
pub fn classify(props: &[u8], ev: &[u8], key: &[u8], abs: &[u8], rel: &[u8]) -> Kind {
  let (has_key, has_abs, has_rel) = (has(ev, EV_KEY), has(ev, EV_ABS), has(ev, EV_REL));
  if has_abs && has(props, INPUT_PROP_DIRECT) && has(abs, ABS_MT_POSITION_X) {
    Kind::Touchscreen
  } else if has_key && has_abs && has(abs, ABS_X) && has(key, BTN_LEFT) {
    Kind::AbsPointer
  } else if has_key && has_rel && has(rel, 0) && has(key, BTN_LEFT) {
    Kind::RelPointer
  } else if has_key && (0..BTN_MISC).any(|k| has(key, k)) {
    Kind::Keyboard
  } else {
    Kind::Other
  }
}

/// Per-device event state. A fresh one per opened device, so a replugged touchscreen starts clean.
#[derive(Debug)]
pub struct Decoder {
  kind: Kind,
  x_range: (i32, i32),
  y_range: (i32, i32),
  area: (u32, u32),
  slot: usize,
  /// Latest raw position per slot (touch) or in slot 0 (absolute pointer).
  raw: [(Option<i32>, Option<i32>); SLOTS],
  /// Slots whose contact began since the last report; emitted one per `SYN_REPORT`.
  pending_touch: [bool; SLOTS],
  pending_click: bool,
  moved: bool,
}

impl Decoder {
  pub fn new(kind: Kind, x_range: (i32, i32), y_range: (i32, i32)) -> Self {
    Self {
      kind,
      x_range,
      y_range,
      area: (0, 0),
      slot: 0,
      raw: [(None, None); SLOTS],
      pending_touch: [false; SLOTS],
      pending_click: false,
      moved: false,
    }
  }

  pub fn set_area(&mut self, w: u32, h: u32) {
    self.area = (w, h);
  }

  /// `(raw - min) * size / (max - min + 1)`, so the top raw value lands on the last pixel.
  fn map(raw: i32, range: (i32, i32), size: u32) -> i32 {
    let span = i64::from(range.1 - range.0 + 1).max(1);
    (i64::from(raw - range.0) * i64::from(size) / span) as i32
  }

  fn position(&self, slot: usize) -> Option<(i32, i32)> {
    let (Some(x), Some(y)) = self.raw[slot] else {
      return None;
    };
    Some((
      Self::map(x, self.x_range, self.area.0),
      Self::map(y, self.y_range, self.area.1),
    ))
  }

  /// Feed one kernel event; `Some` when it completes an interaction. Touches and absolute clicks wait for
  /// `SYN_REPORT`, when the frame's coordinates are all in.
  pub fn feed(&mut self, at: Micros, ev_type: u16, code: u16, value: i32) -> Option<InputEvent> {
    match (self.kind, ev_type, code) {
      (Kind::Touchscreen, EV_ABS, ABS_MT_SLOT) => self.slot = usize::try_from(value).unwrap_or(SLOTS),
      (Kind::Touchscreen, EV_ABS, ABS_MT_TRACKING_ID) if self.slot < SLOTS => {
        // A new id is a new contact; -1 is the lift. A contact's later moves carry no id.
        self.pending_touch[self.slot] = value >= 0;
        self.raw[self.slot] = (None, None);
      }
      (Kind::Touchscreen, EV_ABS, ABS_MT_POSITION_X) if self.slot < SLOTS => self.raw[self.slot].0 = Some(value),
      (Kind::Touchscreen, EV_ABS, ABS_MT_POSITION_Y) if self.slot < SLOTS => self.raw[self.slot].1 = Some(value),
      (Kind::Touchscreen, EV_SYN, 0) => {
        let slot = (0..SLOTS).find(|&s| self.pending_touch[s] && self.position(s).is_some())?;
        self.pending_touch[slot] = false;
        let (x, y) = self.position(slot)?;
        return Some(InputEvent::Touch { at, x, y });
      }
      (Kind::AbsPointer, EV_ABS, ABS_X) => {
        self.raw[0].0 = Some(value);
        self.moved = true;
      }
      (Kind::AbsPointer, EV_ABS, ABS_Y) => {
        self.raw[0].1 = Some(value);
        self.moved = true;
      }
      (Kind::AbsPointer, EV_KEY, BTN_LEFT) if value == 1 => self.pending_click = true,
      (Kind::AbsPointer, EV_SYN, 0) => {
        let click = std::mem::take(&mut self.pending_click);
        let moved = std::mem::take(&mut self.moved);
        if click {
          return Some(InputEvent::Click {
            at,
            pos: self.position(0),
          });
        }
        return moved.then_some(InputEvent::Motion { at });
      }
      (Kind::RelPointer, EV_REL, _) => return Some(InputEvent::Motion { at }),
      (Kind::RelPointer, EV_KEY, BTN_LEFT) if value == 1 => return Some(InputEvent::Click { at, pos: None }),
      (Kind::Keyboard, EV_KEY, _) if value == 1 => return Some(InputEvent::Key { at }),
      _ => {}
    }
    None
  }
}

/// `_IOC(_IOC_READ, 'E', nr, len)` as linux/input.h builds it.
const fn eviocg(nr: u32, len: usize) -> libc::c_ulong {
  ((2u32 << 30) | ((len as u32) << 16) | ((b'E' as u32) << 8) | nr) as libc::c_ulong
}

/// `EVIOCGPROP` (nr 0x09) or `EVIOCGBIT(ev)` (nr 0x20 + ev); all zeros when the ioctl fails.
fn read_bits(f: &File, nr: u32) -> Vec<u8> {
  let mut buf = vec![0u8; 96]; // KEY_MAX is 0x2ff: 96 bytes cover every bitmap
  // SAFETY: the request's length field equals `buf`'s, and the fd is open for the call.
  let rc = unsafe { libc::ioctl(f.as_raw_fd(), eviocg(nr, buf.len()) as _, buf.as_mut_ptr()) };
  if rc < 0 {
    buf.fill(0);
  }
  buf
}

/// `(min, max)` of an absolute axis from `EVIOCGABS`, whose `input_absinfo` is six i32s: value, min, max, ...
fn abs_range(f: &File, axis: u16) -> Option<(i32, i32)> {
  let mut info = [0i32; 6];
  // SAFETY: `info` is exactly the 24 bytes the request names.
  let rc = unsafe {
    libc::ioctl(
      f.as_raw_fd(),
      eviocg(0x40 + u32::from(axis), 24) as _,
      info.as_mut_ptr(),
    )
  };
  (rc >= 0).then_some((info[1], info[2]))
}

struct Device {
  path: PathBuf,
  file: File,
  decoder: Decoder,
}

/// Every useful `/dev/input/event*`, read-only and non-blocking. Reading doesn't grab: the displays still get every
/// event.
pub struct Inputs {
  devices: Vec<Device>,
}

impl Default for Inputs {
  fn default() -> Self {
    let mut inputs = Self { devices: Vec::new() };
    inputs.rescan();
    inputs
  }
}

impl Inputs {
  /// Open devices that appeared since the last scan, each with a fresh decoder; one that dropped off USB comes back
  /// as a new device. Devices with nothing to decode are left closed.
  pub fn rescan(&mut self) {
    let Ok(dir) = fs::read_dir("/dev/input") else { return };
    for path in dir.flatten().map(|e| e.path()) {
      let is_event = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("event"));
      if !is_event || self.devices.iter().any(|d| d.path == path) {
        continue;
      }
      let Ok(file) = File::options().read(true).custom_flags(libc::O_NONBLOCK).open(&path) else {
        continue;
      };
      let kind = classify(
        &read_bits(&file, 0x09),
        &read_bits(&file, 0x20),
        &read_bits(&file, 0x20 + u32::from(EV_KEY)),
        &read_bits(&file, 0x20 + u32::from(EV_ABS)),
        &read_bits(&file, 0x20 + u32::from(EV_REL)),
      );
      let ranges = match kind {
        Kind::Touchscreen => abs_range(&file, ABS_MT_POSITION_X).zip(abs_range(&file, ABS_MT_POSITION_Y)),
        Kind::AbsPointer => abs_range(&file, ABS_X).zip(abs_range(&file, ABS_Y)),
        Kind::Other => None,
        Kind::RelPointer | Kind::Keyboard => Some(((0, 0), (0, 0))),
      };
      if let Some((x, y)) = ranges {
        self.devices.push(Device {
          path,
          file,
          decoder: Decoder::new(kind, x, y),
        });
      }
    }
  }

  pub fn fds(&self) -> Vec<RawFd> {
    self.devices.iter().map(|d| d.file.as_raw_fd()).collect()
  }

  /// Read everything waiting and decode it, mapping positions onto `area`. A device that fails to read (unplugged)
  /// is dropped; the next `rescan` reopens it with fresh state.
  pub fn drain(&mut self, area: (u32, u32)) -> Vec<InputEvent> {
    let mut out = Vec::new();
    let mut buf = [0u8; EVENT_SIZE * 64];
    self.devices.retain_mut(|d| {
      d.decoder.set_area(area.0, area.1);
      loop {
        match d.file.read(&mut buf) {
          Ok(0) => return false,
          Ok(n) => {
            for ev in buf[..n].as_chunks::<EVENT_SIZE>().0 {
              let secs = i64::from_ne_bytes(ev[0..8].try_into().unwrap_or_default());
              let usecs = i64::from_ne_bytes(ev[8..16].try_into().unwrap_or_default());
              let ev_type = u16::from_ne_bytes([ev[16], ev[17]]);
              let code = u16::from_ne_bytes([ev[18], ev[19]]);
              let value = i32::from_ne_bytes([ev[20], ev[21], ev[22], ev[23]]);
              out.extend(d.decoder.feed(secs * 1_000_000 + usecs, ev_type, code, value));
            }
          }
          Err(e) if e.kind() == ErrorKind::WouldBlock => return true,
          Err(e) if e.kind() == ErrorKind::Interrupted => {}
          Err(_) => return false,
        }
      }
    });
    out
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn bits(set: &[u16]) -> Vec<u8> {
    let mut v = vec![0u8; 96];
    for &b in set {
      v[b as usize / 8] |= 1 << (b % 8);
    }
    v
  }

  #[test]
  fn classify_by_capabilities() {
    let direct = bits(&[1]); // INPUT_PROP_DIRECT
    let none = bits(&[]);
    let ev_abs = bits(&[3]);
    let ev_key_abs = bits(&[1, 3]);
    let ev_key_rel = bits(&[1, 2]);
    let ev_key = bits(&[1]);
    assert_eq!(
      classify(&direct, &ev_abs, &none, &bits(&[0x35]), &none),
      Kind::Touchscreen
    );
    assert_eq!(
      classify(&none, &ev_key_abs, &bits(&[0x110]), &bits(&[0, 1]), &none),
      Kind::AbsPointer
    );
    assert_eq!(
      classify(&none, &ev_key_rel, &bits(&[0x110]), &none, &bits(&[0, 1])),
      Kind::RelPointer
    );
    assert_eq!(classify(&none, &ev_key, &bits(&[30]), &none, &none), Kind::Keyboard);
    assert_eq!(classify(&none, &ev_key, &bits(&[116]), &none, &none), Kind::Keyboard);
    assert_eq!(classify(&none, &none, &none, &none, &none), Kind::Other);
  }

  const ABS: u16 = 3;

  fn touch_decoder() -> Decoder {
    let mut d = Decoder::new(Kind::Touchscreen, (0, 4095), (0, 4095));
    d.set_area(1366, 768);
    d
  }

  fn frame(d: &mut Decoder, evs: &[(u16, u16, i32)]) -> Vec<InputEvent> {
    let mut out: Vec<_> = evs.iter().filter_map(|&(t, c, v)| d.feed(5, t, c, v)).collect();
    out.extend(d.feed(5, 0, 0, 0));
    out
  }

  #[test]
  fn touch_maps_onto_the_area_and_ignores_moves() {
    let mut d = touch_decoder();
    let down = [(ABS, 0x2f, 0), (ABS, 0x39, 7), (ABS, 0x35, 2048), (ABS, 0x36, 1024)];
    assert_eq!(frame(&mut d, &down), vec![InputEvent::Touch { at: 5, x: 683, y: 192 }]);
    assert!(frame(&mut d, &[(ABS, 0x35, 2100), (ABS, 0x36, 1100)]).is_empty());
    assert!(frame(&mut d, &[(ABS, 0x39, -1)]).is_empty());
  }

  #[test]
  fn second_contact_in_another_slot_is_a_touch() {
    let mut d = touch_decoder();
    frame(
      &mut d,
      &[(ABS, 0x2f, 0), (ABS, 0x39, 7), (ABS, 0x35, 0), (ABS, 0x36, 0)],
    );
    let got = frame(
      &mut d,
      &[(ABS, 0x2f, 1), (ABS, 0x39, 8), (ABS, 0x35, 4095), (ABS, 0x36, 4095)],
    );
    assert_eq!(got, vec![InputEvent::Touch { at: 5, x: 1365, y: 767 }]);
  }

  #[test]
  fn replug_decoder_reports_the_first_touch() {
    let mut old = touch_decoder();
    frame(&mut old, &[(ABS, 0x39, 7), (ABS, 0x35, 10), (ABS, 0x36, 10)]);
    let mut fresh = touch_decoder();
    let got = frame(&mut fresh, &[(ABS, 0x39, 7), (ABS, 0x35, 0), (ABS, 0x36, 0)]);
    assert_eq!(got, vec![InputEvent::Touch { at: 5, x: 0, y: 0 }]);
  }

  #[test]
  fn keyboard_reports_presses_only() {
    let mut d = Decoder::new(Kind::Keyboard, (0, 0), (0, 0));
    assert_eq!(d.feed(1, 1, 30, 1), Some(InputEvent::Key { at: 1 }));
    assert_eq!(d.feed(1, 1, 30, 2), None);
    assert_eq!(d.feed(1, 1, 30, 0), None);
  }

  #[test]
  fn rel_pointer_moves_and_clicks() {
    let mut d = Decoder::new(Kind::RelPointer, (0, 0), (0, 0));
    assert_eq!(d.feed(1, 2, 0, 3), Some(InputEvent::Motion { at: 1 }));
    assert_eq!(d.feed(2, 1, 0x110, 1), Some(InputEvent::Click { at: 2, pos: None }));
    assert_eq!(d.feed(3, 1, 0x110, 0), None);
  }

  #[test]
  fn abs_pointer_click_carries_its_position() {
    let mut d = Decoder::new(Kind::AbsPointer, (0, 1365), (0, 767));
    d.set_area(1366, 768);
    assert_eq!(d.feed(1, ABS, 0, 100), None);
    assert_eq!(d.feed(1, ABS, 1, 50), None);
    assert_eq!(d.feed(1, 1, 0x110, 1), None);
    assert_eq!(
      d.feed(1, 0, 0, 0),
      Some(InputEvent::Click {
        at: 1,
        pos: Some((100, 50))
      })
    );
  }
}
