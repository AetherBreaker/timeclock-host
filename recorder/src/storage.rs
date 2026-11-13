//! The storage cap: whole clips are deleted, oldest first, until the recordings folder fits.
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn total_size(dir: &Path) -> io::Result<u64> {
  let mut total = 0;
  for entry in fs::read_dir(dir)? {
    let entry = entry?;
    let meta = entry.metadata()?;
    total += if meta.is_dir() {
      total_size(&entry.path())?
    } else {
      meta.len()
    };
  }
  Ok(total)
}

/// Deletes clips (`<name>.*` in `dir`) oldest name first until everything under `dir`, `live/` included, is within
/// `cap_bytes`. Never the `open` clip, never `live/`. Names are timestamps, so name order is age order. Returns the
/// names deleted.
pub fn enforce_cap(dir: &Path, cap_bytes: u64, open: Option<&str>) -> io::Result<Vec<String>> {
  let mut total = total_size(dir)?;
  let mut deleted = Vec::new();
  if total <= cap_bytes {
    return Ok(deleted);
  }
  let mut clips: BTreeMap<String, Vec<(PathBuf, u64)>> = BTreeMap::new();
  for entry in fs::read_dir(dir)? {
    let entry = entry?;
    let meta = entry.metadata()?;
    let file_name = entry.file_name().to_string_lossy().into_owned();
    let name = file_name.split('.').next().unwrap_or_default().to_string();
    if meta.is_file() && !name.is_empty() && Some(name.as_str()) != open {
      clips.entry(name).or_default().push((entry.path(), meta.len()));
    }
  }
  for (name, files) in clips {
    if total <= cap_bytes {
      break;
    }
    for (path, len) in files {
      fs::remove_file(path)?;
      total -= len;
    }
    deleted.push(name);
  }
  Ok(deleted)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testutil::temp_dir;

  fn fill(dir: &Path) {
    fs::create_dir_all(dir.join("live")).unwrap();
    for name in ["2026-01-01_00-00-01", "2026-01-01_00-00-02", "2026-01-01_00-00-03"] {
      fs::write(dir.join(format!("{name}.h264")), [0u8; 100]).unwrap();
      fs::write(dir.join(format!("{name}.txt")), [0u8; 10]).unwrap();
    }
    fs::write(dir.join("live/0000000001.h264"), [0u8; 50]).unwrap();
  }

  #[test]
  fn oldest_clips_go_first() {
    let dir = temp_dir("cap-old");
    fill(&dir);
    // 380 bytes total; 270 needs only the oldest clip (110) gone.
    let gone = enforce_cap(&dir, 270, None).unwrap();
    assert_eq!(gone, ["2026-01-01_00-00-01"]);
    assert!(!dir.join("2026-01-01_00-00-01.txt").exists());
    assert!(dir.join("2026-01-01_00-00-02.txt").exists());
    fs::remove_dir_all(dir).unwrap();
  }

  #[test]
  fn open_clip_and_live_survive() {
    let dir = temp_dir("cap-open");
    fill(&dir);
    let gone = enforce_cap(&dir, 0, Some("2026-01-01_00-00-01")).unwrap();
    assert_eq!(gone, ["2026-01-01_00-00-02", "2026-01-01_00-00-03"]);
    assert!(dir.join("2026-01-01_00-00-01.h264").exists());
    assert!(dir.join("live/0000000001.h264").exists());
    fs::remove_dir_all(dir).unwrap();
  }

  #[test]
  fn under_the_cap_nothing_is_deleted() {
    let dir = temp_dir("cap-under");
    fill(&dir);
    assert!(enforce_cap(&dir, 1_000, None).unwrap().is_empty());
    assert!(dir.join("2026-01-01_00-00-01.h264").exists());
    fs::remove_dir_all(dir).unwrap();
  }
}
