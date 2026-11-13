//! The status folder shared with the clock app. The app creates and owns it; the recorder never does (a root-owned
//! folder would lock the app out), so every call here errors while it is missing and the caller retries.
use crate::clock::{Micros, now_micros};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Writes `<dir>/<file>` via a temp file in the same folder and a rename, so a reader never sees a partial file.
pub fn write_atomic(dir: &Path, file: &str, contents: &str) -> io::Result<()> {
  let tmp = dir.join(format!(".{file}.tmp"));
  fs::write(&tmp, contents)?;
  fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644))?;
  fs::rename(&tmp, dir.join(file))
}

/// Writes `recording` with the open clip's name, or removes it.
pub fn set_recording(dir: &Path, name: Option<&str>) -> io::Result<()> {
  match name {
    Some(name) => write_atomic(dir, "recording", name),
    None => match fs::remove_file(dir.join("recording")) {
      Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
      _ => Ok(()),
    },
  }
}

/// `at` as `YYYY-MM-DDTHH:MM:SSZ`, by civil-from-days arithmetic (no time crate).
fn rfc3339_utc(at: Micros) -> String {
  let secs = at.div_euclid(1_000_000);
  let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
  let z = days + 719_468;
  let era = z.div_euclid(146_097);
  let doe = z.rem_euclid(146_097);
  let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
  let mp = (5 * doy + 2) / 153;
  let day = doy - (153 * mp + 2) / 5 + 1;
  let month = if mp < 10 { mp + 3 } else { mp - 9 };
  let year = yoe + era * 400 + i64::from(month <= 2);
  format!(
    "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
    rem / 3_600,
    rem % 3_600 / 60,
    rem % 60
  )
}

/// The recorder's heartbeat for the app: `recorder.json`, written every 60 s.
pub fn write_alive(dir: &Path, now: Micros, recording: bool) -> io::Result<()> {
  let state = if recording { "recording" } else { "idle" };
  write_atomic(
    dir,
    "recorder.json",
    &format!("{{\"at\":\"{}\",\"state\":\"{state}\"}}", rfc3339_utc(now)),
  )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusEvent {
  Asleep(bool),
  Scan(Micros),
  /// The watch lost events or its folder: reopen it and re-check `asleep_now`.
  Broken,
}

/// inotify on the status folder for `asleep` (created/removed by the app) and `scan` (renamed into place).
pub struct StatusWatch {
  fd: OwnedFd,
  dir: PathBuf,
}

impl StatusWatch {
  /// Errors without creating anything if `dir` is missing (the app creates it); the caller retries every 5 s.
  pub fn open(dir: &Path) -> io::Result<Self> {
    // SAFETY: plain syscall.
    let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if raw < 0 {
      return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh, valid fd that nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).map_err(io::Error::other)?;
    let mask = libc::IN_CREATE
      | libc::IN_DELETE
      | libc::IN_MOVED_TO
      | libc::IN_MOVED_FROM
      | libc::IN_DELETE_SELF
      | libc::IN_MOVE_SELF;
    // SAFETY: `path` is NUL-terminated and outlives the call.
    if unsafe { libc::inotify_add_watch(raw, path.as_ptr(), mask) } < 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(Self {
      fd,
      dir: dir.to_path_buf(),
    })
  }

  pub fn fd(&self) -> RawFd {
    self.fd.as_raw_fd()
  }

  /// Whether `asleep` exists now (the state at startup, before any event).
  pub fn asleep_now(&self) -> bool {
    self.dir.join("asleep").exists()
  }

  /// Reads every pending event; the fd is non-blocking, so this returns at once.
  pub fn drain(&self) -> Vec<StatusEvent> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
      // SAFETY: `buf` is valid for `buf.len()` bytes.
      let n = unsafe { libc::read(self.fd(), buf.as_mut_ptr().cast(), buf.len()) };
      if n <= 0 {
        return out;
      }
      let n = n as usize;
      let mut off = 0;
      // Each event: i32 wd, u32 mask, u32 cookie, u32 len, then `len` bytes of NUL-padded name.
      while off + 16 <= n {
        let mask = u32::from_ne_bytes(buf[off + 4..off + 8].try_into().unwrap());
        let len = u32::from_ne_bytes(buf[off + 12..off + 16].try_into().unwrap()) as usize;
        let end = (off + 16 + len).min(n);
        let name = &buf[off + 16..end];
        let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
        // Overflow lost events; the folder going away (or being replaced) kills the watch. Either way the caller
        // must reopen the watch and re-read `asleep_now`.
        if mask & (libc::IN_Q_OVERFLOW | libc::IN_IGNORED | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0 {
          out.push(StatusEvent::Broken);
          off = end;
          continue;
        }
        let created = mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0;
        let removed = mask & (libc::IN_DELETE | libc::IN_MOVED_FROM) != 0;
        match name {
          b"asleep" if created => out.push(StatusEvent::Asleep(true)),
          b"asleep" if removed => out.push(StatusEvent::Asleep(false)),
          b"scan" if mask & libc::IN_MOVED_TO != 0 => out.push(StatusEvent::Scan(now_micros())),
          _ => {}
        }
        off = end;
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testutil::temp_dir;

  #[test]
  fn recorder_json_is_utc_with_a_zone() {
    let dir = temp_dir("status-json");
    write_alive(&dir, 1_763_039_702_123_456, true).unwrap();
    assert_eq!(
      fs::read_to_string(dir.join("recorder.json")).unwrap(),
      r#"{"at":"2025-11-13T13:15:02Z","state":"recording"}"#
    );
    write_alive(&dir, 951_782_400_000_000, false).unwrap(); // leap day 2000
    assert!(
      fs::read_to_string(dir.join("recorder.json"))
        .unwrap()
        .contains("2000-02-29T00:00:00Z")
    );
    assert_eq!(
      fs::metadata(dir.join("recorder.json")).unwrap().permissions().mode() & 0o777,
      0o644
    );
    fs::remove_dir_all(dir).unwrap();
  }

  #[test]
  fn set_recording_writes_and_removes() {
    let dir = temp_dir("status-rec");
    set_recording(&dir, Some("2026-11-13_08-15-02")).unwrap();
    assert_eq!(
      fs::read_to_string(dir.join("recording")).unwrap(),
      "2026-11-13_08-15-02"
    );
    set_recording(&dir, None).unwrap();
    assert!(!dir.join("recording").exists());
    set_recording(&dir, None).unwrap();
    fs::remove_dir_all(dir).unwrap();
  }

  #[test]
  fn watch_reports_asleep_and_scan() {
    let dir = temp_dir("status-watch");
    let watch = StatusWatch::open(&dir).unwrap();
    assert!(!watch.asleep_now());
    fs::write(dir.join("asleep"), "").unwrap();
    assert!(watch.asleep_now());
    assert_eq!(watch.drain(), [StatusEvent::Asleep(true)]);
    fs::remove_file(dir.join("asleep")).unwrap();
    assert_eq!(watch.drain(), [StatusEvent::Asleep(false)]);
    fs::write(dir.join("scan.tmp"), "1").unwrap();
    fs::rename(dir.join("scan.tmp"), dir.join("scan")).unwrap();
    assert!(matches!(watch.drain().as_slice(), [StatusEvent::Scan(_)]));
    fs::remove_dir_all(dir).unwrap();
  }

  #[test]
  fn watch_reports_a_removed_folder_as_broken() {
    let dir = temp_dir("status-broken");
    let watch = StatusWatch::open(&dir).unwrap();
    fs::remove_dir_all(&dir).unwrap();
    assert!(watch.drain().contains(&StatusEvent::Broken));
  }

  /// Review Focus 1: a missing folder is an error, never created.
  #[test]
  fn watch_on_a_missing_dir_errors_and_creates_nothing() {
    let dir = temp_dir("status-missing");
    let missing = dir.join("status");
    assert!(StatusWatch::open(&missing).is_err());
    assert!(!missing.exists());
    assert!(write_alive(&missing, 0, false).is_err());
    assert!(!missing.exists());
    fs::remove_dir_all(dir).unwrap();
  }
}
