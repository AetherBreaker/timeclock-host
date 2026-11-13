//! Wall-clock times as microseconds since the Unix epoch, and the local-time names built from them.
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Microseconds since the Unix epoch (wall clock).
pub type Micros = i64;

pub fn now_micros() -> Micros {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map_or(0, |d| d.as_micros() as Micros)
}

/// `at` in the host's zone as `(YYYY-MM-DD, HH:MM:SS)`; `localtime_r` is the thread-safe form.
fn local_parts(at: Micros) -> (String, String) {
  let secs = at.div_euclid(1_000_000) as libc::time_t;
  // SAFETY: `tm` is plain data that `localtime_r` fills in; both pointers are valid for the call.
  let tm = unsafe {
    let mut tm: libc::tm = std::mem::zeroed();
    libc::localtime_r(&secs, &mut tm);
    tm
  };
  (
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday),
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec),
  )
}

/// A text-log timestamp: `2026-11-13 08:15:02.123456`.
pub fn local_stamp(at: Micros) -> String {
  let (date, time) = local_parts(at);
  format!("{date} {time}.{:06}", at.rem_euclid(1_000_000))
}

/// `local_stamp`'s inverse, through `mktime`. In the repeated hour when clocks go back, it may pick the wrong one.
pub fn parse_local_stamp(stamp: &str) -> Option<Micros> {
  let (date, time) = stamp.split_once(' ')?;
  let (hms, frac) = time.split_once('.')?;
  if frac.len() != 6 || !frac.bytes().all(|b| b.is_ascii_digit()) {
    return None;
  }
  let fields: Vec<i32> = date
    .split('-')
    .chain(hms.split(':'))
    .map(|p| p.parse().ok())
    .collect::<Option<_>>()?;
  let [year, month, day, hour, min, sec] = fields[..] else {
    return None;
  };
  // SAFETY: `tm` is plain data; `mktime` reads and normalizes it in place.
  let secs = unsafe {
    let mut tm: libc::tm = std::mem::zeroed();
    (tm.tm_year, tm.tm_mon, tm.tm_mday) = (year - 1900, month - 1, day);
    (tm.tm_hour, tm.tm_min, tm.tm_sec) = (hour, min, sec);
    tm.tm_isdst = -1; // let the zone decide
    libc::mktime(&mut tm)
  };
  if secs == -1 {
    return None;
  }
  Some(secs as Micros * 1_000_000 + frac.parse::<Micros>().ok()?)
}

/// A clip's base name: `2026-11-13_08-15-02`.
pub fn clip_name(at: Micros) -> String {
  let (date, time) = local_parts(at);
  format!("{date}_{}", time.replace(':', "-"))
}

/// `clip_name(at)`, with `-2`, `-3`, ... added while any `<name>.*` file exists in `dir`, so two clips in one second
/// (a split, a reopen) never share files.
pub fn unique_clip_name(dir: &Path, at: Micros) -> String {
  let base = clip_name(at);
  let taken = |name: &str| {
    let prefix = format!("{name}.");
    std::fs::read_dir(dir).is_ok_and(|entries| {
      entries
        .flatten()
        .any(|e| e.file_name().to_string_lossy().starts_with(&prefix))
    })
  };
  let mut name = base.clone();
  let mut n = 2;
  while taken(&name) {
    name = format!("{base}-{n}");
    n += 1;
  }
  name
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testutil::eastern;

  const AT: Micros = 1_763_039_702_123_456;

  #[test]
  fn local_stamp_has_microseconds() {
    eastern();
    assert_eq!(local_stamp(AT), "2025-11-13 08:15:02.123456");
  }

  #[test]
  fn parse_local_stamp_inverts_local_stamp() {
    eastern();
    assert_eq!(parse_local_stamp("2025-11-13 08:15:02.123456"), Some(AT));
    // Summer time too.
    let july = 1_752_000_000_000_001;
    assert_eq!(parse_local_stamp(&local_stamp(july)), Some(july));
    for bad in [
      "",
      "2025-11-13 08:15:02",
      "2025-11-13 08:15:02.12345x",
      "x 08:15:02.123456",
    ] {
      assert_eq!(parse_local_stamp(bad), None, "{bad}");
    }
  }

  #[test]
  fn clip_name_is_filename_safe() {
    eastern();
    assert_eq!(clip_name(AT), "2025-11-13_08-15-02");
  }

  #[test]
  fn unique_clip_name_skips_taken_names() {
    eastern();
    let dir = std::env::temp_dir().join(format!("recorder-clock-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(unique_clip_name(&dir, AT), "2025-11-13_08-15-02");
    std::fs::write(dir.join("2025-11-13_08-15-02.txt"), "").unwrap();
    assert_eq!(unique_clip_name(&dir, AT), "2025-11-13_08-15-02-2");
    std::fs::write(dir.join("2025-11-13_08-15-02-2.h264"), "").unwrap();
    assert_eq!(unique_clip_name(&dir, AT), "2025-11-13_08-15-02-3");
    std::fs::remove_dir_all(&dir).unwrap();
  }
}
