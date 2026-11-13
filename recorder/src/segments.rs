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

struct Current {
  info: SegmentInfo,
  h264: BufWriter<File>,
  frames: BufWriter<File>,
}

pub struct Segments {
  dir: PathBuf,
  next_seq: u64,
  finished: Vec<SegmentInfo>,
  current: Option<Current>,
}

impl Segments {
  fn paths(&self, seq: u64) -> (PathBuf, PathBuf) {
    (
      self.dir.join(format!("{seq:010}.h264")),
      self.dir.join(format!("{seq:010}.frames")),
    )
  }

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
    let mut seqs = std::collections::BTreeSet::new();
    for entry in fs::read_dir(&this.dir)? {
      let path = entry?.path();
      if let Some(seq) = path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse::<u64>().ok())
      {
        seqs.insert(seq);
      }
    }
    for seq in seqs {
      this.next_seq = seq + 1;
      let (h264, frames) = this.paths(seq);
      let times: Vec<Micros> = fs::read_to_string(&frames)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
      match (times.first(), times.last()) {
        (Some(&start), Some(&end)) if h264.exists() => this.finished.push(SegmentInfo { seq, start, end }),
        _ => {
          let _ = fs::remove_file(&h264);
          let _ = fs::remove_file(&frames);
        }
      }
    }
    Ok(this)
  }

  /// Starts a segment whose first frame is at `first_at`; finishes any segment still open.
  pub fn begin(&mut self, first_at: Micros) -> io::Result<()> {
    self.finish()?;
    let seq = self.next_seq;
    let (h264, frames) = self.paths(seq);
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

  /// Appends one frame's Annex B bytes and its time, flushed per frame so a crash keeps every frame written. The
  /// bytes go first, so a frame line never points at missing data.
  pub fn write(&mut self, at: Micros, annexb: &[u8]) -> io::Result<()> {
    let cur = self
      .current
      .as_mut()
      .ok_or_else(|| io::Error::other("no segment is open"))?;
    cur.h264.write_all(annexb)?;
    cur.h264.flush()?;
    writeln!(cur.frames, "{at}")?;
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
    let (h264, frames) = self.paths(cur.info.seq);
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
    for s in &old {
      let (h264, frames) = self.paths(s.seq);
      fs::remove_file(h264)?;
      fs::remove_file(frames)?;
    }
    self.finished = keep;
    Ok(())
  }

  /// Appends every finished segment with `start >= from`, in order, to `<dir>/<name>.h264` and `.frames`, then
  /// deletes them. The open segment is left alone: a closing clip `finish`es first.
  pub fn join(&mut self, from: Micros, dir: &Path, name: &str) -> io::Result<()> {
    let append = |ext: &str| {
      OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{name}.{ext}")))
    };
    let (mut out_h264, mut out_frames) = (append("h264")?, append("frames")?);
    let (take, keep): (Vec<SegmentInfo>, Vec<SegmentInfo>) = self.finished.iter().partition(|s| s.start >= from);
    for s in &take {
      let (h264, frames) = self.paths(s.seq);
      io::copy(&mut File::open(&h264)?, &mut out_h264)?;
      io::copy(&mut File::open(&frames)?, &mut out_frames)?;
    }
    out_h264.sync_all()?;
    out_frames.sync_all()?;
    for s in &take {
      let (h264, frames) = self.paths(s.seq);
      fs::remove_file(h264)?;
      fs::remove_file(frames)?;
    }
    self.finished = keep;
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
      "2000\n3000\n3500\n"
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
