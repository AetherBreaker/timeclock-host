//! The clock window's pixels: wlr-screencopy of sway's one output, cropped to the window's rect.
//!
//! Sway offers output capture only (no per-window source), so the whole output is copied and cropped on copy-out.
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, FromRawFd};
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use timeclock_updater::sway::Rect;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer::WlBuffer, wl_output::WlOutput, wl_registry::WlRegistry};
use wayland_client::protocol::{wl_shm, wl_shm::WlShm, wl_shm_pool::WlShmPool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

use crate::clock::{Micros, now_micros};

/// One captured picture of the clock window.
pub struct Frame {
  pub at: Micros,
  pub width: u32,
  pub height: u32,
  /// Tightly packed rows of B, G, R, X bytes.
  pub bgrx: Vec<u8>,
}

/// The heartbeat block's pixel at (2,2), as `0x00RRGGBB`.
pub fn beat(frame: &Frame) -> u32 {
  let i = (2 * frame.width as usize + 2) * 4;
  let p = &frame.bgrx[i..i + 3];
  u32::from(p[2]) << 16 | u32::from(p[1]) << 8 | u32::from(p[0])
}

/// The shm buffer layout the compositor asked for: `(format, width, height, stride)`.
type Layout = (wl_shm::Format, u32, u32, u32);

#[derive(Default)]
struct State {
  /// First supported shm layout offered for the current frame.
  offered: Option<Layout>,
  /// Every shm format offered, as raw `wl_shm` numbers, so an unusable set names itself in the error.
  formats: Vec<u32>,
  buffer_done: bool,
  y_invert: bool,
  /// `ready`'s CLOCK_MONOTONIC presentation time, in µs.
  ready: Option<i64>,
  failed: bool,
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
  fn event(
    _: &mut Self,
    _: &WlRegistry,
    _: <WlRegistry as wayland_client::Proxy>::Event,
    _: &GlobalListContents,
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
  }
}

delegate_noop!(State: ignore WlOutput);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: WlShmPool);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ZwlrScreencopyManagerV1);

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
  fn event(
    state: &mut Self,
    _: &ZwlrScreencopyFrameV1,
    event: zwlr_screencopy_frame_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    use zwlr_screencopy_frame_v1::Event;
    match event {
      // The 32-bit formats `copy_out` turns into BGRx. Sway 1.11 on Intel offers only XBGR8888.
      Event::Buffer {
        format,
        width,
        height,
        stride,
      } => {
        use wl_shm::Format::{Abgr8888, Argb8888, Xbgr8888, Xrgb8888};
        state.formats.push(match format {
          WEnum::Value(f) => f.into(),
          WEnum::Unknown(raw) => raw,
        });
        if let WEnum::Value(f @ (Xrgb8888 | Argb8888 | Xbgr8888 | Abgr8888)) = format
          && state.offered.is_none()
        {
          state.offered = Some((f, width, height, stride));
        }
      }
      Event::BufferDone => state.buffer_done = true,
      Event::Flags { flags } => {
        state.y_invert = flags
          .into_result()
          .is_ok_and(|f| f.contains(zwlr_screencopy_frame_v1::Flags::YInvert));
      }
      Event::Ready {
        tv_sec_hi,
        tv_sec_lo,
        tv_nsec,
      } => {
        let secs = (i64::from(tv_sec_hi) << 32) | i64::from(tv_sec_lo);
        state.ready = Some(secs * 1_000_000 + i64::from(tv_nsec) / 1_000);
      }
      Event::Failed => state.failed = true,
      _ => {}
    }
  }
}

/// The shm buffer frames are copied into, kept while the compositor's layout stays the same.
struct Shm {
  layout: Layout,
  file: File,
  pool: WlShmPool,
  buffer: WlBuffer,
}

pub struct Capture {
  conn: Connection,
  queue: EventQueue<State>,
  state: State,
  shm: WlShm,
  output: WlOutput,
  manager: ZwlrScreencopyManagerV1,
  buffer: Option<Shm>,
  last: Option<Instant>,
}

impl Capture {
  pub fn connect(wayland_socket: &Path) -> io::Result<Self> {
    let conn = Connection::from_socket(UnixStream::connect(wayland_socket)?).map_err(io::Error::other)?;
    let (globals, queue) = registry_queue_init::<State>(&conn).map_err(io::Error::other)?;
    let qh = queue.handle();
    // The clock has one screen, so the first output is it.
    let output = globals.bind(&qh, 1..=1, ()).map_err(io::Error::other)?;
    let shm = globals.bind(&qh, 1..=1, ()).map_err(io::Error::other)?;
    // Version 3 (sway 1.11) ends the buffer offers with `buffer_done`.
    let manager = globals.bind(&qh, 3..=3, ()).map_err(io::Error::other)?;
    Ok(Self {
      conn,
      queue,
      state: State::default(),
      shm,
      output,
      manager,
      buffer: None,
      last: None,
    })
  }

  /// The next changed picture of `rect`, at least `min_gap` after the previous one. Blocks until the screen changes.
  pub fn next(&mut self, rect: Rect, min_gap: Duration) -> io::Result<Frame> {
    if let Some(wait) = self.last.and_then(|last| min_gap.checked_sub(last.elapsed())) {
      std::thread::sleep(wait);
    }
    let qh = self.queue.handle();
    self.state = State::default();
    let frame = self.manager.capture_output(0, &self.output, &qh, ());
    let result = self.copy(&frame, rect);
    frame.destroy();
    self.conn.flush().map_err(io::Error::other)?;
    self.last = Some(Instant::now());
    result
  }

  fn copy(&mut self, frame: &ZwlrScreencopyFrameV1, rect: Rect) -> io::Result<Frame> {
    let qh = self.queue.handle();
    while !self.state.buffer_done && !self.state.failed {
      self
        .queue
        .blocking_dispatch(&mut self.state)
        .map_err(io::Error::other)?;
    }
    let layout @ (format, width, height, stride) = self.state.offered.ok_or_else(|| {
      io::Error::other(format!(
        "the compositor offered no XRGB/ARGB/XBGR/ABGR8888 shm buffer; offered wl_shm formats: {:?}",
        self.state.formats
      ))
    })?;
    if self.buffer.as_ref().is_none_or(|b| b.layout != layout) {
      if let Some(old) = self.buffer.take() {
        old.buffer.destroy();
        old.pool.destroy();
      }
      let size = stride * height;
      // SAFETY: a plain syscall with a valid C string; the fd it returns is owned by `file` alone.
      let file = unsafe {
        let fd = libc::memfd_create(c"timeclock-recorder".as_ptr(), libc::MFD_CLOEXEC);
        if fd < 0 {
          return Err(io::Error::last_os_error());
        }
        File::from_raw_fd(fd)
      };
      file.set_len(u64::from(size))?;
      let pool = self.shm.create_pool(file.as_fd(), size as i32, &qh, ());
      let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, format, &qh, ());
      self.buffer = Some(Shm {
        layout,
        file,
        pool,
        buffer,
      });
    }
    let shm = self.buffer.as_ref().expect("set above");
    // Blocks in the compositor until the output is damaged, so an unchanged screen costs nothing.
    frame.copy_with_damage(&shm.buffer);
    let presented = loop {
      if self.state.failed {
        return Err(io::Error::other("screencopy failed"));
      }
      if let Some(at) = self.state.ready {
        break at;
      }
      self
        .queue
        .blocking_dispatch(&mut self.state)
        .map_err(io::Error::other)?;
    };

    // Monotonic → wall: shift by how long ago it was presented, so clock steps can't skew frames against each other.
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `now` is a valid timespec for the call to fill in.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    let mono_now = now.tv_sec * 1_000_000 + now.tv_nsec / 1_000;
    let at = now_micros() - (mono_now - presented);

    // The rect is in layout pixels; the one output sits at (0,0) with scale 1, so they're buffer pixels too.
    let x0 = rect.x.clamp(0, width as i32) as u32;
    let y0 = rect.y.clamp(0, height as i32) as u32;
    let x1 = (i64::from(rect.x) + i64::from(rect.width)).clamp(0, i64::from(width)) as u32;
    let y1 = (i64::from(rect.y) + i64::from(rect.height)).clamp(0, i64::from(height)) as u32;
    if x1 <= x0 || y1 <= y0 {
      // `InvalidInput`: a sway state (an empty or off-screen window), not a capture failure; the caller waits it out.
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("the window rect {rect:?} is off the {width}x{height} screen"),
      ));
    }
    let (w, h) = (x1 - x0, y1 - y0);
    // One read of the rows the rect spans (mirrored when the buffer is stored bottom-up), then the columns are cut.
    let first = if self.state.y_invert { height - y1 } else { y0 };
    let mut band = vec![0; (h * stride) as usize];
    shm.file.read_exact_at(&mut band, u64::from(first * stride))?;
    Ok(Frame {
      at,
      width: w,
      height: h,
      bgrx: copy_out(&band, format, stride, x0, (w, h), self.state.y_invert),
    })
  }
}

/// The `w`x`h` pixels at column `x0` of `band` (rows `stride` bytes apart, bottom-up if `y_invert`), tightly packed
/// as B, G, R, X.
fn copy_out(band: &[u8], format: wl_shm::Format, stride: u32, x0: u32, (w, h): (u32, u32), y_invert: bool) -> Vec<u8> {
  let mut bgrx = Vec::with_capacity((w * h * 4) as usize);
  for i in 0..h {
    let row = if y_invert { h - 1 - i } else { i };
    let start = (row * stride + x0 * 4) as usize;
    bgrx.extend_from_slice(&band[start..start + (w * 4) as usize]);
  }
  // XBGR8888/ABGR8888 are R, G, B, X in memory.
  if matches!(format, wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888) {
    for px in bgrx.as_chunks_mut::<4>().0 {
      px.swap(0, 2);
    }
  }
  bgrx
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn beat_reads_the_pixel_at_2_2() {
    let mut bgrx = [0x14, 0x15, 0x16, 0x00].repeat(64);
    let i = (2 * 8 + 2) * 4;
    bgrx[i..i + 4].copy_from_slice(&[0x15, 0x16, 0x17, 0x00]);
    let frame = Frame {
      at: 0,
      width: 8,
      height: 8,
      bgrx,
    };
    assert_eq!(beat(&frame), 0x171615);
  }

  /// Two rows of four pixels (stride 16), each pixel distinct; `rgb_order` stores them R, G, B, X.
  fn band(rgb_order: bool) -> Vec<u8> {
    let mut band = Vec::new();
    for y in 0..2u8 {
      for x in 0..4u8 {
        let (b, g, r) = (x, y, 0x80 + x);
        band.extend_from_slice(&if rgb_order { [r, g, b, 0xff] } else { [b, g, r, 0xff] });
      }
    }
    band
  }

  #[test]
  fn copy_out_crops_and_keeps_bgrx() {
    let out = copy_out(&band(false), wl_shm::Format::Xrgb8888, 16, 1, (2, 2), false);
    assert_eq!(
      out,
      band(false)[4..12]
        .iter()
        .chain(&band(false)[20..28])
        .copied()
        .collect::<Vec<_>>()
    );
  }

  #[test]
  fn copy_out_swaps_xbgr_into_bgrx() {
    for format in [wl_shm::Format::Xbgr8888, wl_shm::Format::Abgr8888] {
      let out = copy_out(&band(true), format, 16, 1, (2, 2), false);
      assert_eq!(
        out,
        band(false)[4..12]
          .iter()
          .chain(&band(false)[20..28])
          .copied()
          .collect::<Vec<_>>()
      );
    }
  }

  #[test]
  fn copy_out_flips_a_bottom_up_buffer() {
    let out = copy_out(&band(false), wl_shm::Format::Argb8888, 16, 0, (1, 2), true);
    assert_eq!(out, [band(false)[16..20].to_vec(), band(false)[0..4].to_vec()].concat());
  }
}
