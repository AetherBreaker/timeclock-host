//! A closed clip's `.mp4`: 30 fps constant, each output frame showing the newest captured frame at or before its time,
//! with a strip below the picture (the shown frame's capture time; a freeze timer) and rings at positioned touches.
//! Runs as its own idle-priority process (`timeclock-recorder process <dir> <name>`).
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};
use mp4::{AvcConfig, Bytes, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig, TrackType};

use crate::capture::Frame;
use crate::clock::{Micros, local_stamp, parse_local_stamp};
use crate::encode::{H264Decoder, H264Encoder, OPENH264};

const FONT: &str = "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf";
const FPS: i64 = 30;
const STRIP: u32 = 40;
const RING_RADIUS: f64 = 24.0;
const RING_WIDTH: f64 = 3.0;
const RING_HOLD: Micros = 500_000;
/// `#F9E60D` as B, G, R, X.
const RING_COLOR: [u8; 4] = [0x0d, 0xe6, 0xf9, 0];
/// A keyframe every 2 s, so a player can seek.
const IDR_EVERY: u64 = 2 * FPS as u64;

/// A freeze's length as `ss.ff`, `m:ss.ff` or `h:mm:ss.ff`; `ff` is the frame within the second (00-29).
pub fn timer_text(len: Micros) -> String {
  let len = len.max(0);
  let (secs, ff) = (len / 1_000_000, len % 1_000_000 * FPS / 1_000_000);
  let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
  if h > 0 {
    format!("{h}:{m:02}:{s:02}.{ff:02}")
  } else if m > 0 {
    format!("{m}:{s:02}.{ff:02}")
  } else {
    format!("{s:02}.{ff:02}")
  }
}

/// What the `.txt` says to draw.
#[derive(Debug, Default, PartialEq)]
pub struct Overlay {
  /// Touches and clicks with a position.
  pub touches: Vec<(Micros, i32, i32)>,
  /// Start and end; `None` runs to the clip's end.
  pub freezes: Vec<(Micros, Option<Micros>)>,
  /// The first `clip start` line. The footage can begin well before it: a static screen's hours-old segment, or a
  /// split's re-keyed frame.
  pub start: Option<Micros>,
  /// The `clip end` line. A frozen screen sends no frames, so the footage can stop well before it.
  pub end: Option<Micros>,
}

impl Overlay {
  /// Lines it can't read are skipped: a damaged `.txt` costs marks, not the video.
  pub fn parse(text: &str) -> Self {
    let mut o = Self::default();
    for line in text.lines() {
      // `<YYYY-MM-DD HH:MM:SS.ffffff> <event>`: the stamp is 26 characters.
      let (Some(stamp), Some(event)) = (line.get(..26), line.get(27..)) else {
        continue;
      };
      let Some(at) = parse_local_stamp(stamp) else { continue };
      match event.split(' ').collect::<Vec<_>>()[..] {
        ["touch" | "click", x, y] => {
          if let (Ok(x), Ok(y)) = (x.parse(), y.parse()) {
            o.touches.push((at, x, y));
          }
        }
        ["freeze", "start"] => o.freezes.push((at, None)),
        ["freeze", "end", _] => {
          if let Some(open) = o.freezes.last_mut().filter(|f| f.1.is_none()) {
            open.1 = Some(at);
          }
        }
        ["clip", "start"] => {
          o.start.get_or_insert(at);
        }
        ["clip", "end"] => o.end = Some(at),
        _ => {}
      }
    }
    o
  }

  /// The video's first and last output times, for footage whose frames run from `first` to `last`. It starts at
  /// `clip start` (showing the frame on screen then) and runs to `clip end`. Stamps read back through `mktime` can be
  /// an hour off in the fall-back hour, so the end is held within `max_clip` and a minute of the start (no clip is
  /// longer), and a start past the end falls back to the first frame.
  pub fn span(&self, first: Micros, last: Micros, max_clip: Micros) -> (Micros, Micros) {
    let start = self.start.map_or(first, |s| s.max(first));
    let end = self
      .end
      .map_or(last, |e| e.max(last))
      .min(start + max_clip + 60_000_000);
    if start <= end { (start, end) } else { (first, end) }
  }
}

/// Annex B NAL units, without their start codes.
fn nal_units(annexb: &[u8]) -> Vec<&[u8]> {
  // (where the start code begins, where the unit begins); a 4-byte code's leading zero isn't part of the unit before.
  let mut starts = Vec::new();
  let mut i = 0;
  while i + 3 <= annexb.len() {
    if annexb[i..i + 3] == [0, 0, 1] {
      starts.push((if i > 0 && annexb[i - 1] == 0 { i - 1 } else { i }, i + 3));
      i += 3;
    } else {
      i += 1;
    }
  }
  (0..starts.len())
    .map(|j| &annexb[starts[j].1..starts.get(j + 1).map_or(annexb.len(), |n| n.0)])
    .collect()
}

/// Draws `text` in white on `canvas`, from `x` (or ending at `x` if `right`), on `baseline`.
fn draw_text(canvas: &mut Frame, font: &FontVec, text: &str, x: f32, baseline: f32, right: bool) {
  let scaled = font.as_scaled(PxScale::from(22.0));
  let ids: Vec<_> = text.chars().map(|c| font.glyph_id(c)).collect();
  let width: f32 = ids.iter().map(|&id| scaled.h_advance(id)).sum();
  let mut pen = if right { x - width } else { x };
  for id in ids {
    let glyph = id.with_scale_and_position(scaled.scale(), point(pen, baseline));
    pen += scaled.h_advance(id);
    let Some(outline) = font.outline_glyph(glyph) else {
      continue;
    };
    let bounds = outline.px_bounds();
    let (w, h) = (canvas.width as i64, canvas.height as i64);
    outline.draw(|gx, gy, coverage| {
      let (px, py) = (bounds.min.x as i64 + i64::from(gx), bounds.min.y as i64 + i64::from(gy));
      if (0..w).contains(&px) && (0..h).contains(&py) {
        let i = ((py * w + px) * 4) as usize;
        for c in &mut canvas.bgrx[i..i + 3] {
          *c = (f32::from(*c) * (1.0 - coverage) + 255.0 * coverage) as u8;
        }
      }
    });
  }
}

/// The picture on a `size` canvas (cropped or padded, should the window have changed size mid-clip), the strip below
/// it, and the rings.
fn compose(pic: &Frame, size: (u32, u32), t: Micros, overlay: &Overlay, font: Option<&FontVec>) -> Frame {
  let (w, h) = size;
  let mut out = Frame {
    at: pic.at,
    width: w,
    height: h + STRIP,
    bgrx: vec![0; (w * (h + STRIP) * 4) as usize],
  };
  let (cw, ch) = (pic.width.min(w) as usize, pic.height.min(h) as usize);
  for y in 0..ch {
    let src = y * pic.width as usize * 4;
    let dst = y * w as usize * 4;
    out.bgrx[dst..dst + cw * 4].copy_from_slice(&pic.bgrx[src..src + cw * 4]);
  }
  let reach = (RING_RADIUS + RING_WIDTH) as i32;
  for &(_, x, y) in overlay.touches.iter().filter(|(at, ..)| *at <= t && t < at + RING_HOLD) {
    for py in (y - reach).max(0)..(y + reach + 1).min(h as i32) {
      for px in (x - reach).max(0)..(x + reach + 1).min(w as i32) {
        let d = f64::from((px - x).pow(2) + (py - y).pow(2)).sqrt();
        if (d - RING_RADIUS).abs() <= RING_WIDTH / 2.0 {
          let i = ((py as u32 * w + px as u32) * 4) as usize;
          out.bgrx[i..i + 4].copy_from_slice(&RING_COLOR);
        }
      }
    }
  }
  if let Some(font) = font {
    let baseline = (h + STRIP / 2 + 8) as f32;
    draw_text(&mut out, font, &local_stamp(pic.at), 10.0, baseline, false);
    // The latest freeze covering `t`; an unfinished one runs to the clip's end, which `t` never passes.
    let freeze = overlay
      .freezes
      .iter()
      .rev()
      .find(|(start, end)| *start <= t && end.is_none_or(|e| t < e));
    if let Some((start, _)) = freeze {
      draw_text(&mut out, font, &timer_text(t - start), w as f32 - 10.0, baseline, true);
    }
  }
  out
}

/// The MP4 file: one AVC track at 30 fps, created at the first frame, whose SPS and PPS go in the track's config.
struct Mux {
  path: PathBuf,
  size: (u32, u32),
  writer: Option<Mp4Writer<BufWriter<File>>>,
  /// Frames pushed, empty ones included: the next frame's time in 1/30 s, and the IDR cadence's count.
  count: u64,
  /// Written at the next sample or at `finish`, so an empty frame can lengthen it and later frames keep their times.
  pending: Option<Mp4Sample>,
}

impl Mux {
  /// One frame's Annex B bytes, as a length-prefixed sample; SPS and PPS go in the track's config instead.
  fn push(&mut self, annexb: &[u8]) -> Result<(), String> {
    let (mut sample, mut sps, mut pps, mut sync) = (Vec::new(), None, None, false);
    for nal in nal_units(annexb) {
      match nal.first().map(|b| b & 0x1f) {
        Some(7) => sps = Some(nal),
        Some(8) => pps = Some(nal),
        // Access unit delimiters carry nothing an MP4 needs.
        Some(9) | None => {}
        Some(kind) => {
          sync |= kind == 5;
          sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
          sample.extend_from_slice(nal);
        }
      }
    }
    if sample.is_empty() {
      if let Some(pending) = &mut self.pending {
        pending.duration += 1;
      }
      self.count += 1;
      return Ok(());
    }
    let mp4_err = |e: mp4::Error| format!("{}: {e}", self.path.display());
    if self.writer.is_none() {
      let (Some(sps), Some(pps)) = (sps, pps) else {
        return Err("the first frame has no SPS/PPS".into());
      };
      let file = File::create(&self.path).map_err(|e| format!("{}: {e}", self.path.display()))?;
      let brand = |s: &[u8; 4]| mp4::FourCC::from(*s);
      let config = Mp4Config {
        major_brand: brand(b"isom"),
        minor_version: 512,
        compatible_brands: vec![brand(b"isom"), brand(b"iso2"), brand(b"avc1"), brand(b"mp41")],
        timescale: 1000,
      };
      let mut writer = Mp4Writer::write_start(BufWriter::new(file), &config).map_err(mp4_err)?;
      writer
        .add_track(&TrackConfig {
          track_type: TrackType::Video,
          timescale: FPS as u32,
          language: "und".into(),
          media_conf: MediaConfig::AvcConfig(AvcConfig {
            width: self.size.0 as u16,
            height: self.size.1 as u16,
            seq_param_set: sps.to_vec(),
            pic_param_set: pps.to_vec(),
          }),
        })
        .map_err(mp4_err)?;
      self.writer = Some(writer);
    }
    let sample = Mp4Sample {
      start_time: self.count,
      duration: 1,
      rendering_offset: 0,
      is_sync: sync,
      bytes: Bytes::from(sample),
    };
    if let Some(previous) = self.pending.replace(sample) {
      let writer = self.writer.as_mut().expect("created above");
      writer.write_sample(1, &previous).map_err(mp4_err)?;
    }
    self.count += 1;
    Ok(())
  }

  fn finish(self) -> Result<(), String> {
    let path = self.path.display().to_string();
    let (Some(mut writer), Some(last)) = (self.writer, self.pending) else {
      return Err(format!("{path}: no frames"));
    };
    writer.write_sample(1, &last).map_err(|e| format!("{path}: {e}"))?;
    writer.write_end().map_err(|e| format!("{path}: {e}"))?;
    let file = writer
      .into_writer()
      .into_inner()
      .map_err(|e| format!("{path}: {}", e.error()))?;
    file.sync_all().map_err(|e| format!("{path}: {e}"))
  }
}

/// Makes `<dir>/<name>.mp4` from the clip's `.h264`, `.frames` and `.txt`, through `.mp4.part`. `max_clip` bounds its
/// length (`Overlay::span`).
pub fn run(dir: &Path, name: &str, max_clip: Micros) -> Result<(), String> {
  let path = |ext: &str| dir.join(format!("{name}.{ext}"));
  let h264 = fs::read(path("h264")).map_err(|e| format!("{name}.h264: {e}"))?;
  let index = fs::read_to_string(path("frames")).map_err(|e| format!("{name}.frames: {e}"))?;
  // `<micros> <bytes>` per frame: its time, and its length in the `.h264`.
  let mut frames = Vec::new();
  let mut offset = 0;
  for (i, line) in index.lines().enumerate() {
    let parsed = line
      .split_once(' ')
      .and_then(|(at, len)| Some((at.parse::<Micros>().ok()?, len.parse::<usize>().ok()?)));
    let Some((at, len)) = parsed.filter(|(_, len)| offset + len <= h264.len()) else {
      return Err(format!(
        "{name}.frames line {}: unreadable or past the .h264's end",
        i + 1
      ));
    };
    frames.push((at, offset..offset + len));
    offset += len;
  }
  let (Some(first), Some(last)) = (frames.first(), frames.last()) else {
    return Err(format!("{name}: no frames"));
  };
  let overlay = fs::read_to_string(path("txt")).map_or_else(|_| Overlay::default(), |t| Overlay::parse(&t));
  let (t0, end) = overlay.span(first.0, last.0, max_clip);
  let font = fs::read(FONT).ok().and_then(|b| FontVec::try_from_vec(b).ok());
  if font.is_none() {
    eprintln!("{FONT} is missing: {name}.mp4 gets no times or timer");
  }

  let mut decoder = H264Decoder::new(Path::new(OPENH264))?;
  let mut encoder: Option<H264Encoder> = None;
  let mut mux = Mux {
    path: path("mp4.part"),
    size: (0, 0),
    writer: None,
    count: 0,
    pending: None,
  };
  let (mut next, mut shown) = (0, None::<Frame>);
  for k in 0..=(end - t0) * FPS / 1_000_000 {
    let t = t0 + k * 1_000_000 / FPS;
    while let Some((at, range)) = frames.get(next).filter(|(at, _)| *at <= t) {
      if let Some(mut pic) = decoder.decode(&h264[range.clone()])? {
        pic.at = *at;
        shown = Some(pic);
      }
      next += 1;
    }
    let Some(pic) = &shown else { continue };
    // The canvas is the first picture's size: the encoder and the track keep one size throughout.
    if encoder.is_none() {
      mux.size = (pic.width & !1, (pic.height & !1) + STRIP);
      encoder = Some(H264Encoder::new(Path::new(OPENH264), mux.size.0, mux.size.1)?);
    }
    let out = compose(pic, (mux.size.0, mux.size.1 - STRIP), t, &overlay, font.as_ref());
    let enc = encoder.as_mut().expect("created above");
    mux.push(&enc.encode(&out, mux.count.is_multiple_of(IDR_EVERY))?)?;
  }
  mux.finish()?;
  fs::rename(path("mp4.part"), path("mp4")).map_err(|e| format!("{name}.mp4: {e}"))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testutil::temp_dir;

  #[test]
  fn timer_text_formats() {
    assert_eq!(timer_text(3_250_000), "03.07");
    assert_eq!(timer_text(63_500_000), "1:03.15");
    assert_eq!(timer_text(3_723_000_000), "1:02:03.00");
    assert_eq!(timer_text(0), "00.00");
  }

  #[test]
  fn overlay_reads_touches_freezes_and_the_end() {
    crate::testutil::eastern();
    let base = 1_763_039_702_000_000;
    let line = |off: Micros, event: &str| format!("{} {event}\n", local_stamp(base + off));
    let text = [
      line(0, "clip start"),
      line(1_000_000, "touch 10 20"),
      line(1_500_000, "click 30 40"),
      line(1_600_000, "click"),
      line(2_000_000, "freeze start"),
      line(5_250_000, "freeze end 3.250000"),
      line(6_000_000, "freeze start"),
      line(9_000_000, "clip end"),
      "garbage\n".into(),
    ]
    .concat();
    assert_eq!(
      Overlay::parse(&text),
      Overlay {
        touches: vec![(base + 1_000_000, 10, 20), (base + 1_500_000, 30, 40)],
        freezes: vec![(base + 2_000_000, Some(base + 5_250_000)), (base + 6_000_000, None)],
        start: Some(base),
        end: Some(base + 9_000_000),
      }
    );
  }

  #[test]
  fn span_starts_at_clip_start_and_bounds_the_end() {
    const S: Micros = 1_000_000;
    const MAX: Micros = 600 * S;
    let o = |start, end| Overlay {
      start,
      end,
      ..Overlay::default()
    };
    // A normal clip: frames from its start, ending after the last frame (a frozen tail).
    assert_eq!(
      o(Some(100 * S), Some(200 * S)).span(100 * S, 150 * S, MAX),
      (100 * S, 200 * S)
    );
    // A split during a freeze: one re-keyed frame from long before the clip; the video still covers the clip.
    assert_eq!(
      o(Some(5_000 * S), Some(5_600 * S)).span(1_000 * S, 1_000 * S, MAX),
      (5_000 * S, 5_600 * S)
    );
    // A static screen's hours-old segment: the video starts at `clip start`, not the first frame.
    assert_eq!(
      o(Some(7_183 * S), Some(7_215 * S)).span(0, 7_210 * S, MAX),
      (7_183 * S, 7_215 * S)
    );
    // The fall-back hour: an end read an hour late is held within max_clip and a minute.
    assert_eq!(
      o(Some(100 * S), Some(3_800 * S)).span(100 * S, 150 * S, MAX),
      (100 * S, 760 * S)
    );
    // A start read an hour late falls back to the first frame.
    assert_eq!(
      o(Some(3_700 * S), Some(200 * S)).span(100 * S, 150 * S, MAX),
      (100 * S, 200 * S)
    );
    // No `.txt`: the frames alone.
    assert_eq!(Overlay::default().span(100 * S, 150 * S, MAX), (100 * S, 150 * S));
  }

  #[test]
  fn nal_units_split_on_three_and_four_byte_start_codes() {
    let s = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5];
    assert_eq!(nal_units(&s), [&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4, 5]]);
    assert!(nal_units(&[1, 2, 3]).is_empty());
  }

  fn grey(width: u32, height: u32) -> Frame {
    Frame {
      at: 1_763_039_702_000_000,
      width,
      height,
      bgrx: [0x80, 0x80, 0x80, 0].repeat((width * height) as usize),
    }
  }

  fn px(f: &Frame, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * f.width + x) * 4) as usize;
    f.bgrx[i..i + 4].try_into().unwrap()
  }

  #[test]
  fn compose_adds_the_strip_and_rings_for_half_a_second() {
    let pic = grey(100, 100);
    let overlay = Overlay {
      touches: vec![(pic.at, 50, 50)],
      ..Overlay::default()
    };
    let out = compose(&pic, (100, 100), pic.at + 100_000, &overlay, None);
    assert_eq!((out.width, out.height), (100, 140));
    assert_eq!(px(&out, 50, 50), [0x80, 0x80, 0x80, 0]);
    assert_eq!(px(&out, 74, 50), RING_COLOR);
    assert_eq!(px(&out, 50, 26), RING_COLOR);
    assert_eq!(px(&out, 70, 50), [0x80, 0x80, 0x80, 0]);
    assert_eq!(px(&out, 50, 139), [0, 0, 0, 0]);
    let later = compose(&pic, (100, 100), pic.at + RING_HOLD, &overlay, None);
    assert_eq!(px(&later, 74, 50), [0x80, 0x80, 0x80, 0]);
  }

  #[test]
  fn compose_pads_a_smaller_picture() {
    let out = compose(&grey(10, 10), (100, 100), 0, &Overlay::default(), None);
    assert_eq!((out.width, out.height), (100, 140));
    assert_eq!(px(&out, 5, 5), [0x80, 0x80, 0x80, 0]);
    assert_eq!(px(&out, 50, 50), [0, 0, 0, 0]);
    let out = compose(&grey(200, 200), (100, 100), 0, &Overlay::default(), None);
    assert_eq!((out.width, out.height), (100, 140));
  }

  #[test]
  fn compose_draws_the_time_and_the_freeze_timer_when_the_font_is_there() {
    let Some(font) = fs::read(FONT).ok().and_then(|b| FontVec::try_from_vec(b).ok()) else {
      eprintln!("{FONT} is missing; skipped");
      return;
    };
    let pic = grey(400, 100);
    let lit = |f: &Frame, xs: std::ops::Range<u32>| {
      xs.flat_map(|x| (100..140).map(move |y| (x, y)))
        .any(|(x, y)| px(f, x, y) != [0, 0, 0, 0])
    };
    let calm = compose(&pic, (400, 100), pic.at, &Overlay::default(), Some(&font));
    assert!(lit(&calm, 0..200), "the capture time on the left");
    assert!(!lit(&calm, 300..400), "no timer without a freeze");
    let frozen = Overlay {
      freezes: vec![(pic.at - 3_000_000, None)],
      ..Overlay::default()
    };
    assert!(lit(&compose(&pic, (400, 100), pic.at, &frozen, Some(&font)), 300..400));
  }

  #[test]
  fn mux_writes_a_30_fps_avc_track() {
    let dir = temp_dir("mux");
    let path = dir.join("x.mp4");
    let mut mux = Mux {
      path: path.clone(),
      size: (16, 16),
      writer: None,
      count: 0,
      pending: None,
    };
    mux
      .push(&[
        0, 0, 0, 1, 0x67, 0x42, 0, 0x1e, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 9, 9,
      ])
      .unwrap();
    mux.push(&[0, 0, 0, 1, 0x41, 7]).unwrap();
    // An encode with no picture (only a delimiter): the previous frame stays up for it, and later ones keep their time.
    mux.push(&[0, 0, 0, 1, 0x09, 0xf0]).unwrap();
    mux.push(&[0, 0, 0, 1, 0x41, 8]).unwrap();
    assert_eq!(mux.count, 4);
    mux.finish().unwrap();
    let size = fs::metadata(&path).unwrap().len();
    let mut reader = mp4::Mp4Reader::read_header(std::io::BufReader::new(File::open(&path).unwrap()), size).unwrap();
    let track = reader.tracks().get(&1).unwrap();
    assert_eq!(track.sample_count(), 3);
    assert_eq!(track.timescale(), 30);
    assert_eq!(track.sequence_parameter_set().unwrap(), [0x67, 0x42, 0, 0x1e]);
    let first = reader.read_sample(1, 1).unwrap().unwrap();
    assert!(first.is_sync);
    assert_eq!(first.bytes.as_ref(), [0, 0, 0, 3, 0x65, 9, 9]);
    let second = reader.read_sample(1, 2).unwrap().unwrap();
    assert!(!second.is_sync);
    assert_eq!((second.start_time, second.duration), (1, 2));
    assert_eq!(reader.read_sample(1, 3).unwrap().unwrap().start_time, 3);
    fs::remove_dir_all(dir).unwrap();
  }
}
