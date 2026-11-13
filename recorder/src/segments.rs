//! The live segment store: the encoded stream in short files under `live/`, joined into a clip's files when it closes.
use crate::clock::Micros;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentInfo {
  pub seq: u64,
  pub start: Micros,
  pub end: Micros,
}

/// `remove_file` that treats an already-missing file as done, so a retried delete never wedges.
fn remove_if_present(path: &Path) -> io::Result<()> {
  match fs::remove_file(path) {
    Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
    _ => Ok(()),
  }
}

struct Current {
  info: SegmentInfo,
  h264: BufWriter<File>,
  frames: BufWriter<File>,
}

/// Segment `seq`'s `.h264` and `.frames` in `live_dir`.
fn paths(live_dir: &Path, seq: u64) -> (PathBuf, PathBuf) {
  (
    live_dir.join(format!("{seq:010}.h264")),
    live_dir.join(format!("{seq:010}.frames")),
  )
}

pub struct Segments {
  dir: PathBuf,
  next_seq: u64,
  finished: Vec<SegmentInfo>,
  current: Option<Current>,
}

impl Segments {
  /// Creates `live_dir` if needed and recovers crash leftovers: each `.frames` with at least one line becomes a
  /// finished segment (start/end = its first/last frame time); anything else (no frames, no `.h264`) is deleted.
  pub fn open(live_dir: PathBuf) -> io::Result<Self> {
    fs::create_dir_all(&live_dir)?;
    let mut this = Self {
      dir: live_dir,
      next_seq: 0,
      finished: Vec::new(),
      current: None,
    };
    // Only `<digits>.h264` / `<digits>.frames` are segments; anything else in the folder is left alone.
    let mut seqs = std::collections::BTreeSet::new();
    for entry in fs::read_dir(&this.dir)? {
      let path = entry?.path();
      let is_segment = matches!(path.extension().and_then(|e| e.to_str()), Some("h264" | "frames"));
      let seq = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| s.bytes().all(|b| b.is_ascii_digit()));
      if let (true, Some(seq)) = (is_segment, seq.and_then(|s| s.parse::<u64>().ok())) {
        seqs.insert(seq);
      }
    }
    for seq in seqs {
      this.next_seq = seq + 1;
      let (h264, frames) = paths(&this.dir, seq);
      // A crash between the two per-frame writes (or ENOSPC) can leave a torn last `.frames` line or h264 bytes with
      // no line. Keep only complete `<micros> <bytes>` lines whose bytes are all in the `.h264`, then cut both files
      // to exactly those, so frame lines and bytes stay aligned.
      let h264_len = fs::metadata(&h264).map_or(0, |m| m.len());
      let text = fs::read(&frames).unwrap_or_default();
      let (mut times, mut good_lines, mut good_bytes) = (Vec::new(), 0u64, 0u64);
      for line in text.split_inclusive(|&b| b == b'\n').filter(|l| l.ends_with(b"\n")) {
        let parsed = std::str::from_utf8(line).ok().and_then(|l| {
          let (at, len) = l.trim_end().split_once(' ')?;
          Some((at.parse::<Micros>().ok()?, len.parse::<u64>().ok()?))
        });
        match parsed {
          Some((at, len)) if good_bytes + len <= h264_len => {
            times.push(at);
            good_lines += line.len() as u64;
            good_bytes += len;
          }
          _ => break,
        }
      }
      if let (Some(&start), Some(&end)) = (times.first(), times.last()) {
        OpenOptions::new().write(true).open(&frames)?.set_len(good_lines)?;
        OpenOptions::new().write(true).open(&h264)?.set_len(good_bytes)?;
        this.finished.push(SegmentInfo { seq, start, end });
      } else {
        remove_if_present(&h264)?;
        remove_if_present(&frames)?;
      }
    }
    Ok(this)
  }

  /// Starts a segment whose first frame is at `first_at`; finishes any segment still open.
  pub fn begin(&mut self, first_at: Micros) -> io::Result<()> {
    self.finish()?;
    let seq = self.next_seq;
    let (h264, frames) = paths(&self.dir, seq);
    let create = |p: PathBuf| {
      OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(p)
        .map(BufWriter::new)
    };
    self.current = Some(Current {
      info: SegmentInfo {
        seq,
        start: first_at,
        end: first_at,
      },
      h264: create(h264)?,
      frames: create(frames)?,
    });
    self.next_seq += 1;
    Ok(())
  }

  /// Appends one frame's Annex B bytes, then its `<micros> <bytes>` line, flushed per frame so a crash keeps every
  /// frame written. Bytes first, so a line never points at missing data; `open` cuts any torn tail.
  pub fn write(&mut self, at: Micros, annexb: &[u8]) -> io::Result<()> {
    let cur = self
      .current
      .as_mut()
      .ok_or_else(|| io::Error::other("no segment is open"))?;
    cur.h264.write_all(annexb)?;
    cur.h264.flush()?;
    writeln!(cur.frames, "{at} {}", annexb.len())?;
    cur.frames.flush()?;
    cur.info.end = at;
    Ok(())
  }

  /// Closes the current segment into `finished`; one with no frames is deleted instead.
  pub fn finish(&mut self) -> io::Result<()> {
    let Some(mut cur) = self.current.take() else {
      return Ok(());
    };
    cur.h264.flush()?;
    cur.frames.flush()?;
    let (h264, frames) = paths(&self.dir, cur.info.seq);
    if fs::metadata(&frames)?.len() == 0 {
      fs::remove_file(h264)?;
      fs::remove_file(frames)?;
    } else {
      self.finished.push(cur.info);
    }
    Ok(())
  }

  pub fn writing(&self) -> bool {
    self.current.is_some()
  }

  pub fn finished(&self) -> &[SegmentInfo] {
    &self.finished
  }

  /// The latest finished-or-current segment start at or before `t`, else the earliest start.
  pub fn start_for(&self, t: Micros) -> Option<Micros> {
    let starts = || {
      self
        .finished
        .iter()
        .map(|s| s.start)
        .chain(self.current.iter().map(|c| c.info.start))
    };
    starts().filter(|&s| s <= t).max().or_else(|| starts().min())
  }

  /// Deletes finished segments that ended before `older_than`.
  pub fn prune(&mut self, older_than: Micros) -> io::Result<()> {
    let (old, keep): (Vec<SegmentInfo>, Vec<SegmentInfo>) = self.finished.iter().partition(|s| s.end < older_than);
    self.finished = keep;
    Taken {
      dir: self.dir.clone(),
      segments: old,
    }
    .delete()
  }

  /// Removes every finished segment with `start >= from` from the store, for a join done without holding it (the
  /// writer keeps writing new segments meanwhile). The open segment is left alone: a closing clip `finish`es first.
  pub fn take(&mut self, from: Micros) -> Taken {
    let (take, keep): (Vec<SegmentInfo>, Vec<SegmentInfo>) = self.finished.iter().partition(|s| s.start >= from);
    self.finished = keep;
    Taken {
      dir: self.dir.clone(),
      segments: take,
    }
  }

  /// `take(from).join(dir, name)`.
  pub fn join(&mut self, from: Micros, dir: &Path, name: &str) -> io::Result<()> {
    self.take(from).join(dir, name).map_err(|(e, back)| {
      self.give_back(back);
      e
    })
  }

  /// Returns a failed join's segments to `finished`, in sequence order.
  pub fn give_back(&mut self, taken: Taken) {
    self.finished.extend(taken.segments);
    self.finished.sort_by_key(|s| s.seq);
  }
}

/// Segments taken out of the store, to be joined into a clip without holding it.
#[derive(Debug)]
pub struct Taken {
  dir: PathBuf,
  segments: Vec<SegmentInfo>,
}

impl Taken {
  /// Deletes every segment's files even if one delete fails (NotFound is fine), then returns the first error.
  fn delete(&self) -> io::Result<()> {
    let mut first = Ok(());
    for s in &self.segments {
      let (h264, frames) = paths(&self.dir, s.seq);
      for r in [remove_if_present(&h264), remove_if_present(&frames)] {
        first = first.and(r);
      }
    }
    first
  }

  /// Writes the segments, in order, as `<dir>/<name>.h264` and `.frames`, then deletes them. Retry-safe: the clip is
  /// built in truncated `.part` files and renamed into place before any delete, so a crash-and-recover rebuilds the
  /// same clip instead of appending a duplicate. Nothing to join creates no files. A segment missing either file is
  /// skipped as a pair.
  ///
  /// A failure before the clip is in place returns the segments for `Segments::give_back`, so they stay tracked (the
  /// next prune or clip deals with them) instead of lingering in `live/` until recovery joins them, gap and all, with
  /// whatever else is left. A failed delete afterwards returns nothing to give back: the clip already holds them.
  pub fn join(self, dir: &Path, name: &str) -> Result<(), (io::Error, Taken)> {
    if self.segments.is_empty() {
      return Ok(());
    }
    if let Err(e) = self.write_clip(dir, name) {
      return Err((e, self));
    }
    self.delete().map_err(|e| {
      let nothing = Taken {
        dir: self.dir.clone(),
        segments: Vec::new(),
      };
      (e, nothing)
    })
  }

  fn write_clip(&self, dir: &Path, name: &str) -> io::Result<()> {
    let part = |ext: &str| dir.join(format!("{name}.{ext}.part"));
    let (mut out_h264, mut out_frames) = (File::create(part("h264"))?, File::create(part("frames"))?);
    for s in &self.segments {
      let (h264, frames) = paths(&self.dir, s.seq);
      match (File::open(&h264), File::open(&frames)) {
        (Ok(mut h), Ok(mut f)) => {
          io::copy(&mut h, &mut out_h264)?;
          io::copy(&mut f, &mut out_frames)?;
        }
        (Err(e), _) | (_, Err(e)) if e.kind() == io::ErrorKind::NotFound => {}
        (Err(e), _) | (_, Err(e)) => return Err(e),
      }
    }
    out_h264.sync_all()?;
    out_frames.sync_all()?;
    for ext in ["h264", "frames"] {
      fs::rename(part(ext), dir.join(format!("{name}.{ext}")))?;
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testutil::temp_dir;

  /// Writes one segment of `frames` (time, byte) pairs.
  fn segment(s: &mut Segments, frames: &[(Micros, u8)]) {
    s.begin(frames[0].0).unwrap();
    for &(at, b) in frames {
      s.write(at, &[b]).unwrap();
    }
    s.finish().unwrap();
  }

  fn three(root: &Path) -> Segments {
    let mut s = Segments::open(root.join("live")).unwrap();
    segment(&mut s, &[(1_000, b'a'), (1_500, b'b')]);
    segment(&mut s, &[(2_000, b'c')]);
    segment(&mut s, &[(3_000, b'd'), (3_500, b'e')]);
    s
  }

  #[test]
  fn start_for_picks_latest_start_at_or_before() {
    let root = temp_dir("seg-start");
    let mut s = three(&root);
    assert_eq!(s.start_for(2_500), Some(2_000));
    assert_eq!(s.start_for(3_000), Some(3_000));
    assert_eq!(s.start_for(500), Some(1_000));
    s.begin(4_000).unwrap();
    assert_eq!(s.start_for(9_000), Some(4_000));
    fs::remove_dir_all(root).unwrap();
  }

  #[test]
  fn prune_deletes_only_old_segments() {
    let root = temp_dir("seg-prune");
    let mut s = three(&root);
    s.prune(2_000).unwrap();
    let left: Vec<_> = s.finished().iter().map(|i| i.start).collect();
    assert_eq!(left, [2_000, 3_000]);
    assert!(!root.join("live/0000000000.h264").exists());
    assert!(root.join("live/0000000001.h264").exists());
    fs::remove_dir_all(root).unwrap();
  }

  #[test]
  fn join_concatenates_in_order_and_removes_segments() {
    let root = temp_dir("seg-join");
    let mut s = three(&root);
    s.join(2_000, &root, "clip").unwrap();
    assert_eq!(fs::read(root.join("clip.h264")).unwrap(), b"cde");
    assert_eq!(
      fs::read_to_string(root.join("clip.frames")).unwrap(),
      "2000 1\n3000 1\n3500 1\n"
    );
    assert_eq!(s.finished().len(), 1);
    assert!(!root.join("live/0000000001.h264").exists());
    assert!(root.join("live/0000000000.frames").exists());
    fs::remove_dir_all(root).unwrap();
  }

  #[test]
  fn leftovers_are_recovered_from_their_frames() {
    let root = temp_dir("seg-recover");
    {
      let mut s = three(&root);
      s.begin(4_000).unwrap();
      s.write(4_000, b"f").unwrap();
      s.write(4_250, b"g").unwrap();
      // Dropped without finish: the crash case.
    }
    fs::write(root.join("live/0000000009.frames"), "").unwrap();
    fs::write(root.join("live/0000000009.h264"), "x").unwrap();
    let mut s = Segments::open(root.join("live")).unwrap();
    let got: Vec<_> = s.finished().iter().map(|i| (i.start, i.end)).collect();
    assert_eq!(got, [(1_000, 1_500), (2_000, 2_000), (3_000, 3_500), (4_000, 4_250)]);
    assert!(!root.join("live/0000000009.h264").exists());
    s.begin(5_000).unwrap();
    assert!(root.join("live/0000000010.h264").exists());
    fs::remove_dir_all(root).unwrap();
  }

  /// Writes a crash-style segment (seq 3 after `three`) of two one-byte frames and drops it unfinished.
  fn crashed(root: &Path) {
    let mut s = three(root);
    s.begin(4_000).unwrap();
    s.write(4_000, b"f").unwrap();
    s.write(4_250, b"g").unwrap();
  }

  #[test]
  fn torn_trailing_frames_line_is_cut_at_recovery() {
    let root = temp_dir("seg-torn");
    crashed(&root);
    let mut f = OpenOptions::new()
      .append(true)
      .open(root.join("live/0000000003.frames"))
      .unwrap();
    f.write_all(b"17630").unwrap();
    drop(f);
    let s = Segments::open(root.join("live")).unwrap();
    assert_eq!(s.finished().last().map(|i| (i.start, i.end)), Some((4_000, 4_250)));
    assert_eq!(
      fs::read_to_string(root.join("live/0000000003.frames")).unwrap(),
      "4000 1\n4250 1\n"
    );
    assert_eq!(fs::read(root.join("live/0000000003.h264")).unwrap(), b"fg");
    fs::remove_dir_all(root).unwrap();
  }

  #[test]
  fn h264_bytes_without_a_line_are_cut_at_recovery() {
    let root = temp_dir("seg-extra");
    crashed(&root);
    let mut f = OpenOptions::new()
      .append(true)
      .open(root.join("live/0000000003.h264"))
      .unwrap();
    f.write_all(b"zz").unwrap();
    drop(f);
    Segments::open(root.join("live")).unwrap();
    assert_eq!(fs::read(root.join("live/0000000003.h264")).unwrap(), b"fg");
    fs::remove_dir_all(root).unwrap();
  }

  /// The join runs outside the store's lock: a segment written meanwhile is neither joined nor disturbed.
  #[test]
  fn writing_carries_on_while_taken_segments_are_joined() {
    let root = temp_dir("seg-take");
    let mut s = three(&root);
    let taken = s.take(2_000);
    assert_eq!(s.finished().len(), 1);
    segment(&mut s, &[(4_000, b'f')]);
    taken.join(&root, "clip").unwrap();
    assert_eq!(fs::read(root.join("clip.h264")).unwrap(), b"cde");
    let left: Vec<_> = s.finished().iter().map(|i| i.start).collect();
    assert_eq!(left, [1_000, 4_000]);
    assert!(root.join("live/0000000003.h264").exists());
    fs::remove_dir_all(root).unwrap();
  }

  /// A clean stop: finishing the open segment and pruning everything leaves `live/` empty, so the next start
  /// recovers nothing.
  #[test]
  fn a_clean_stop_leaves_nothing_to_recover() {
    let root = temp_dir("seg-stop");
    let mut s = three(&root);
    s.begin(4_000).unwrap();
    s.write(4_000, b"f").unwrap();
    s.finish().unwrap();
    s.prune(Micros::MAX).unwrap();
    assert_eq!(fs::read_dir(root.join("live")).unwrap().count(), 0);
    assert!(Segments::open(root.join("live")).unwrap().finished().is_empty());
    fs::remove_dir_all(root).unwrap();
  }

  /// A join that fails before the clip is in place hands its segments back, so they stay tracked and a later prune
  /// deletes them, instead of lingering for recovery to join across the gap.
  #[test]
  fn a_failed_join_hands_its_segments_back() {
    let root = temp_dir("seg-fail");
    let mut s = three(&root);
    let taken = s.take(2_000);
    segment(&mut s, &[(4_000, b'f')]);
    let (_, back) = taken.join(&root.join("missing"), "clip").unwrap_err();
    s.give_back(back);
    let tracked: Vec<_> = s.finished().iter().map(|i| (i.seq, i.start)).collect();
    assert_eq!(tracked, [(0, 1_000), (1, 2_000), (2, 3_000), (3, 4_000)]);
    s.prune(10_000).unwrap();
    assert!(s.finished().is_empty());
    assert_eq!(fs::read_dir(root.join("live")).unwrap().count(), 0);
    // `Segments::join` does the same on its own.
    let mut s = three(&root);
    assert!(s.join(2_000, &root.join("missing"), "clip").is_err());
    assert_eq!(s.finished().len(), 3);
    fs::remove_dir_all(root).unwrap();
  }

  #[test]
  fn join_twice_changes_nothing_the_second_time() {
    let root = temp_dir("seg-twice");
    let mut s = three(&root);
    s.join(2_000, &root, "clip").unwrap();
    s.join(2_000, &root, "clip").unwrap();
    assert_eq!(fs::read(root.join("clip.h264")).unwrap(), b"cde");
    s.join(9_000, &root, "none").unwrap();
    assert!(!root.join("none.h264").exists() && !root.join("none.frames").exists());
    fs::remove_dir_all(root).unwrap();
  }

  #[test]
  fn join_completes_when_a_segment_file_is_already_gone() {
    let root = temp_dir("seg-gone");
    let mut s = three(&root);
    fs::remove_file(root.join("live/0000000001.h264")).unwrap();
    s.join(2_000, &root, "clip").unwrap();
    assert_eq!(fs::read(root.join("clip.h264")).unwrap(), b"de");
    assert!(s.finished().len() == 1);
    assert!(!root.join("live/0000000001.frames").exists());
    fs::remove_dir_all(root).unwrap();
  }

  /// Review Focus 3: the window vanishes mid-segment with a clip open; the segment finishes and the join 15 s later
  /// still includes it.
  #[test]
  fn segment_finished_mid_clip_is_joined_later() {
    let root = temp_dir("seg-vanish");
    let mut s = Segments::open(root.join("live")).unwrap();
    s.begin(1_000).unwrap();
    s.write(1_000, b"a").unwrap();
    s.write(1_200, b"b").unwrap();
    s.finish().unwrap();
    assert!(!s.writing());
    s.join(1_000, &root, "clip").unwrap();
    assert_eq!(fs::read(root.join("clip.h264")).unwrap(), b"ab");
    fs::remove_dir_all(root).unwrap();
  }
}
