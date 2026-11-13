//! H.264 through the system's OpenH264 (`libopenh264.so.8`, Fedora's 2.6.0 build), loaded at run time.
use std::path::Path;

use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::encoder::{Encoder, EncoderConfig, IntraFramePeriod, QpRange, RateControlMode, UsageType};
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};
use openh264_sys2::API;

use crate::capture::Frame;

/// Fedora's OpenH264 2.6.0, on every clock.
pub const OPENH264: &str = "/usr/lib64/libopenh264.so.8";

/// Fixed quantizer, so text stays readable at a modest size. With rate control off, OpenH264
/// may take an IDR's QP from its per-layer default (`iDLayerQp`) instead; the crate (0.9) doesn't expose that setting.
const QP: u8 = 26;

/// Loads `lib`, refusing any version but 2.6.x: the crate's bindings match 2.6's ABI, and a mismatch is UB, not an error.
fn load(lib: &Path) -> Result<OpenH264API, String> {
  // SAFETY: the version is checked right after loading, before any other call. The unchecked loader because the crate's
  // checked one compares the file against Cisco's builds, and Fedora builds its own.
  let api =
    unsafe { OpenH264API::from_blob_path_unchecked(lib) }.map_err(|e| format!("loading {}: {e}", lib.display()))?;
  // SAFETY: `WelsGetCodecVersion` only returns a struct; it touches no state.
  let v = unsafe { api.WelsGetCodecVersion() };
  if (v.uMajor, v.uMinor) != (2, 6) {
    return Err(format!(
      "{} is OpenH264 {}.{}.{}; only 2.6.x is supported",
      lib.display(),
      v.uMajor,
      v.uMinor,
      v.uRevision
    ));
  }
  Ok(api)
}

pub struct H264Encoder {
  encoder: Encoder,
  /// The frames' size; the encoded size drops an odd last column/row, since 4:2:0 needs even dimensions.
  width: u32,
  height: u32,
  yuv: YUVBuffer,
  /// The cropped copy, used only when a dimension is odd.
  even: Vec<u8>,
  started: bool,
}

impl H264Encoder {
  pub fn new(lib: &Path, width: u32, height: u32) -> Result<Self, String> {
    let (even_w, even_h) = (width & !1, height & !1);
    if even_w == 0 || even_h == 0 {
      return Err(format!("can't encode a {width}x{height} frame"));
    }
    let config = EncoderConfig::new()
      .usage_type(UsageType::ScreenContentRealTime)
      .num_threads(1)
      // Constant quality: no bitrate target, every frame at QP 26, none skipped.
      .rate_control_mode(RateControlMode::Off)
      .qp(QpRange::new(QP, QP))
      .skip_frames(false)
      // Both are rate-control aids: off, as unused with rate control off, and OpenH264 warns about each otherwise.
      .adaptive_quantization(false)
      .background_detection(false)
      // Keyframes only where `force_idr` asks (segment starts).
      .intra_frame_period(IntraFramePeriod::auto());
    let encoder = Encoder::with_api_config(load(lib)?, config).map_err(|e| e.to_string())?;
    Ok(Self {
      encoder,
      width,
      height,
      yuv: YUVBuffer::new(even_w as usize, even_h as usize),
      even: Vec::new(),
      started: false,
    })
  }

  /// `f` as Annex B bytes. `force_idr` makes it a keyframe (with SPS/PPS), so a segment can start there.
  pub fn encode(&mut self, f: &Frame, force_idr: bool) -> Result<Vec<u8>, String> {
    if (f.width, f.height) != (self.width, self.height) {
      return Err(format!(
        "a {}x{} frame for a {}x{} encoder",
        f.width, f.height, self.width, self.height
      ));
    }
    let (w, h) = (self.width & !1, self.height & !1);
    let bgrx = if (w, h) == (f.width, f.height) {
      &f.bgrx
    } else {
      let row = (f.width * 4) as usize;
      self.even.clear();
      for line in f.bgrx.chunks_exact(row).take(h as usize) {
        self.even.extend_from_slice(&line[..(w * 4) as usize]);
      }
      &self.even
    };
    // B, G, R, X reads as BGRA; the X byte is ignored.
    self.yuv.read_bgra8(BgraSliceU8::new(bgrx, (w as usize, h as usize)));
    // The encoder initializes on its first frame, which is an IDR anyway; forcing before that isn't allowed.
    if force_idr && self.started {
      self.encoder.force_intra_frame();
    }
    self.started = true;
    let bits = self.encoder.encode(&self.yuv).map_err(|e| e.to_string())?;
    Ok(bits.to_vec())
  }
}

pub struct H264Decoder {
  decoder: Decoder,
}

impl H264Decoder {
  pub fn new(lib: &Path) -> Result<Self, String> {
    let decoder = Decoder::with_api_config(load(lib)?, DecoderConfig::new()).map_err(|e| e.to_string())?;
    Ok(Self { decoder })
  }

  /// One frame's Annex B bytes → its picture, or `None` while the decoder needs more. `at` is 0: the stream carries no
  /// wall time, so the caller sets it.
  pub fn decode(&mut self, annexb_frame: &[u8]) -> Result<Option<Frame>, String> {
    let Some(yuv) = self.decoder.decode(annexb_frame).map_err(|e| e.to_string())? else {
      return Ok(None);
    };
    let (w, h) = yuv.dimensions();
    let mut bgrx = vec![0; w * h * 4];
    yuv.write_rgba8(&mut bgrx);
    for px in bgrx.as_chunks_mut::<4>().0 {
      px.swap(0, 2);
      px[3] = 0;
    }
    Ok(Some(Frame {
      at: 0,
      width: w as u32,
      height: h as u32,
      bgrx,
    }))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const LIB: &str = OPENH264;

  /// Left half `#161514` (the page background), right half white, so the decoded picture is checkable despite loss.
  fn frame(width: u32, height: u32) -> Frame {
    let mut bgrx = Vec::new();
    for _ in 0..height {
      for x in 0..width {
        bgrx.extend_from_slice(if x < width / 2 {
          &[0x14, 0x15, 0x16, 0]
        } else {
          &[0xff, 0xff, 0xff, 0]
        });
      }
    }
    Frame {
      at: 0,
      width,
      height,
      bgrx,
    }
  }

  #[test]
  #[ignore = "needs the system's libopenh264.so.8 (run on the dev clock)"]
  fn encode_then_decode_round_trip() {
    // 1367x769: the odd last column and row are cropped away.
    for (w, h) in [(1366, 768), (1367, 769)] {
      let mut encoder = H264Encoder::new(Path::new(LIB), w, h).unwrap();
      let mut decoder = H264Decoder::new(Path::new(LIB)).unwrap();
      let source = frame(w, h);
      for i in 0..3 {
        let bits = encoder.encode(&source, i == 2).unwrap();
        // Annex B: every access unit starts with a start code.
        assert!(bits.starts_with(&[0, 0, 0, 1]), "frame {i}");
        let idr = bits.windows(5).any(|n| n[..4] == [0, 0, 0, 1] && n[4] & 0x1f == 5);
        assert_eq!(idr, i != 1, "frame {i}: only the first and the forced frame are IDRs");
        let out = decoder.decode(&bits).unwrap().expect("a picture per frame");
        assert_eq!((out.width, out.height), (1366, 768));
        let dark = &out.bgrx[(100 * 1366 + 100) * 4..][..3];
        let light = &out.bgrx[(100 * 1366 + 1200) * 4..][..3];
        assert!(
          dark.iter().zip([0x14, 0x15, 0x16]).all(|(a, b)| a.abs_diff(b) <= 4),
          "{dark:?}"
        );
        assert!(light.iter().all(|&a| a >= 0xf8), "{light:?}");
      }
    }
  }
}
