//! The recorder's process: the live service with no arguments, or `process <recordings_dir> <name>` to make one clip's
//! `.mp4`.
//!
//! Threads, all feeding the controller's one channel:
//! - capture: blocks in `Capture::next` until the screen changes; sends each frame's heartbeat to the controller and
//!   the frame to the writer through a 2-deep channel (full: the frame is dropped).
//! - writer: encodes and appends to the live segments, rotating at every `keyframe` with a forced IDR.
//! - input, status (inotify), sway (GET_TREE every 250 ms): readers.
//! - controller (main thread): owns the clip state (`control::Controller`), the status files and the processing child,
//!   and ticks every 250 ms whatever the others are doing.
//!
//! `Segments` sits behind a mutex shared by the writer (per frame) and the controller (start, join, prune).
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, exit};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use timeclock_recorder::capture::{Capture, Frame, beat};
use timeclock_recorder::clock::{Micros, now_micros, unique_clip_name};
use timeclock_recorder::config::{Config, parse_config};
use timeclock_recorder::control::{Action, Controller, Env, Msg};
use timeclock_recorder::encode::{H264Encoder, OPENH264};
use timeclock_recorder::input::Inputs;
use timeclock_recorder::process;
use timeclock_recorder::segments::Segments;
use timeclock_recorder::status::{self, StatusEvent, StatusWatch};
use timeclock_recorder::storage::enforce_cap;
use timeclock_recorder::textlog;
use timeclock_updater::sway::{Rect, get_tree, window_rect};

const CONFIG: &str = "/etc/timeclock/recorder.conf";
const TICK: Duration = Duration::from_millis(250);
const ALIVE_EVERY: Duration = Duration::from_secs(60);
const STATUS_RETRY: Duration = Duration::from_secs(5);
const INPUT_RESCAN: Duration = Duration::from_secs(2);
const SWAY_DOWN: Duration = Duration::from_secs(30);

/// Locks through poisoning: a panic aborts the process (release profile), so a poisoned lock is never seen live.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
  m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A capture or encoder failure: the error goes to the journal, and systemd restarts the service.
fn fatal(what: &str, e: impl std::fmt::Display) -> ! {
  eprintln!("{what}: {e}");
  exit(1)
}

/// What the capture thread should capture; `generation` changes with every new target.
#[derive(Default)]
struct Target {
  generation: u64,
  rect: Option<Rect>,
}

/// Crash recovery at start. Leftover segments become a clip: the one named in `status/recording` unless that clip is
/// already complete (`.h264` and `.frames` both in place; a re-join of what's left must never replace it), else a
/// fresh name from the first frame. `recovered after a stop` goes at the last frame's time. Any `.part` left after
/// that is from a cut-short join or processing run with nothing left to rebuild it from, so it goes too.
fn recover(recordings_dir: &Path, status_dir: &Path) -> io::Result<Option<String>> {
  let recorded = fs::read_to_string(status_dir.join("recording"))
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty() && !s.contains('/'));
  let mut segments = Segments::open(recordings_dir.join("live"))?;
  let span = segments.finished().first().zip(segments.finished().last());
  let recovered = match span.map(|(first, last)| (first.start, last.end)) {
    Some((start, end)) => {
      let complete = |n: &str| {
        ["h264", "frames"]
          .iter()
          .all(|ext| recordings_dir.join(format!("{n}.{ext}")).exists())
      };
      let name = match recorded {
        Some(n) if !complete(&n) => n,
        _ => unique_clip_name(recordings_dir, start),
      };
      if !recordings_dir.join(format!("{name}.txt")).exists() {
        textlog::append(recordings_dir, &name, start, "clip start")?;
      }
      segments.join(Micros::MIN, recordings_dir, &name)?;
      textlog::append(recordings_dir, &name, end, "recovered after a stop")?;
      Some(name)
    }
    None => None,
  };
  for entry in fs::read_dir(recordings_dir)? {
    let path = entry?.path();
    if path.extension().is_some_and(|e| e == "part") {
      fs::remove_file(path)?;
    }
  }
  status::set_recording(status_dir, None)?;
  Ok(recovered)
}

/// Makes `.mp4`s one child process at a time, oldest clip first, at the lowest CPU and I/O priority.
struct Processing {
  dir: PathBuf,
  queue: BTreeSet<String>,
  running: Option<(String, Child)>,
  /// Clips whose run failed: not retried until the recorder restarts, so a bad clip can't loop.
  failed: HashSet<String>,
}

impl Processing {
  /// Queues every clip with an `.h264` and `.frames` but no `.mp4`, except the one being processed (no `.mp4` yet).
  fn scan(&mut self) {
    let Ok(entries) = fs::read_dir(&self.dir) else { return };
    for entry in entries.flatten() {
      let file = entry.file_name().to_string_lossy().into_owned();
      let Some(name) = file.strip_suffix(".h264") else {
        continue;
      };
      let has = |ext: &str| self.dir.join(format!("{name}.{ext}")).exists();
      let running = self.running.as_ref().is_some_and(|(r, _)| r == name);
      if has("frames") && !has("mp4") && !self.failed.contains(name) && !running {
        self.queue.insert(name.to_string());
      }
    }
  }

  /// Reaps a finished child and starts the next.
  fn poll(&mut self) {
    if let Some((name, child)) = &mut self.running {
      match child.try_wait() {
        Ok(None) => return,
        Ok(Some(status)) if status.success() => eprintln!("clip {} processed", self.dir.join(&*name).display()),
        Ok(Some(status)) => {
          eprintln!("processing {name} failed: {status}");
          self.failed.insert(name.clone());
        }
        Err(e) => {
          eprintln!("processing {name}: {e}");
          self.failed.insert(name.clone());
        }
      }
      self.running = None;
    }
    // Queued while it was being processed, or made since: done already.
    let mp4 = |name: &String| self.dir.join(format!("{name}.mp4")).exists();
    let Some(name) = std::iter::from_fn(|| self.queue.pop_first()).find(|n| !mp4(n)) else {
      return;
    };
    let exe = match std::env::current_exe() {
      Ok(exe) => exe,
      Err(e) => return eprintln!("processing {name}: {e}"),
    };
    let mut cmd = Command::new(exe);
    cmd.arg("process").arg(&self.dir).arg(&name);
    // SAFETY: only async-signal-safe syscalls between fork and exec.
    unsafe {
      cmd.pre_exec(|| {
        let param = libc::sched_param { sched_priority: 0 };
        if libc::sched_setscheduler(0, libc::SCHED_IDLE, &param) != 0 {
          return Err(io::Error::last_os_error());
        }
        // ioprio_set(IOPRIO_WHO_PROCESS, this process, class 3 (idle) << IOPRIO_CLASS_SHIFT); libc has no wrapper.
        if libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13) != 0 {
          return Err(io::Error::last_os_error());
        }
        Ok(())
      });
    }
    match cmd.spawn() {
      Ok(child) => self.running = Some((name, child)),
      Err(e) => {
        eprintln!("processing {name}: {e}");
        self.failed.insert(name);
      }
    }
  }
}

struct LiveEnv<'a> {
  segments: &'a Mutex<Segments>,
  dir: &'a Path,
}

impl Env for LiveEnv<'_> {
  fn start_for(&self, t: Micros) -> Option<Micros> {
    lock(self.segments).start_for(t)
  }
  fn unique_name(&self, at: Micros) -> String {
    unique_clip_name(self.dir, at)
  }
}

/// Retries every 5 s until the app has created the status folder; never creates it (Review Focus 1).
fn open_status_watch(dir: &Path) -> StatusWatch {
  let mut told = false;
  loop {
    match StatusWatch::open(dir) {
      Ok(watch) => return watch,
      Err(e) if !told => {
        eprintln!("waiting for {}: {e}", dir.display());
        told = true;
      }
      Err(_) => {}
    }
    thread::sleep(STATUS_RETRY);
  }
}

fn capture_thread(
  mut capture: Capture,
  target: Arc<(Mutex<Target>, Condvar)>,
  min_gap: Duration,
  msgs: Sender<Msg>,
  frames: SyncSender<(u64, Frame)>,
) {
  let (state, wake) = &*target;
  loop {
    let (generation, rect) = {
      let mut t = lock(state);
      loop {
        if let Some(rect) = t.rect {
          break (t.generation, rect);
        }
        t = wake.wait(t).unwrap_or_else(PoisonError::into_inner);
      }
    };
    let frame = capture.next(rect, min_gap).unwrap_or_else(|e| fatal("capture", e));
    // The target changed while this waited: the rect is stale, and the picture may be the standby.
    if lock(state).generation != generation {
      continue;
    }
    // `beat` reads (2,2).
    if frame.width >= 3 && frame.height >= 3 {
      let _ = msgs.send(Msg::Beat {
        generation,
        at: frame.at,
        beat: beat(&frame),
      });
    }
    if let Err(TrySendError::Disconnected(_)) = frames.try_send((generation, frame)) {
      return;
    }
  }
}

/// The writer's encoder, as a trait so a test can stand in for OpenH264.
trait FrameEncoder {
  fn encode(&mut self, frame: &Frame, force_idr: bool) -> Result<Vec<u8>, String>;
}

impl FrameEncoder for H264Encoder {
  fn encode(&mut self, frame: &Frame, force_idr: bool) -> Result<Vec<u8>, String> {
    H264Encoder::encode(self, frame, force_idr)
  }
}

fn writer_thread<E: FrameEncoder>(
  frames: Receiver<(u64, Frame)>,
  target: Arc<(Mutex<Target>, Condvar)>,
  segments: Arc<Mutex<Segments>>,
  keyframe: Micros,
  new_encoder: impl Fn(u32, u32) -> Result<E, String>,
) {
  let (mut encoder, mut size, mut seg_start) = (None::<E>, (0, 0), 0);
  let mut failing = false;
  for (generation, frame) in frames {
    if frame.width < 2 || frame.height < 2 {
      continue;
    }
    // Held through the encode, so a controller rotate or join can't land between the IDR decision and the write.
    let mut segs = lock(&segments);
    // Queued before the target changed: stale. Checked under the segments lock (the controller bumps the generation
    // before it finishes the segment), so a stale frame can't begin a segment after a stop.
    if lock(&target.0).generation != generation {
      continue;
    }
    if (frame.width, frame.height) != size {
      encoder = Some(new_encoder(frame.width, frame.height).unwrap_or_else(|e| fatal("encoder", e)));
      size = (frame.width, frame.height);
      let _ = segs.finish(); // a new size starts a new segment; an error here resurfaces at `begin`
    }
    let idr = !segs.writing() || frame.at - seg_start >= keyframe;
    if idr {
      if let Err(e) = segs.begin(frame.at) {
        // Nothing is encoded without a segment to hold it, so the encoder's next frame still follows on the last.
        if !failing {
          eprintln!("segment: {e}");
          failing = true;
        }
        continue;
      }
      seg_start = frame.at;
    }
    let encoder = encoder.as_mut().expect("built above for this size");
    let bits = encoder.encode(&frame, idr).unwrap_or_else(|e| fatal("encoder", e));
    match segs.write(frame.at, &bits) {
      Ok(()) if failing => {
        eprintln!("segment writes work again");
        failing = false;
      }
      Ok(()) => {}
      Err(e) => {
        if !failing {
          eprintln!("segment: {e}");
          failing = true;
        }
        // The lost frame breaks the chain of references: the next frame starts a fresh segment with an IDR.
        let _ = segs.finish();
      }
    }
  }
}

fn input_thread(area: Arc<Mutex<(u32, u32)>>, msgs: Sender<Msg>) {
  let mut inputs = Inputs::default();
  let mut rescan_at = Instant::now() + INPUT_RESCAN;
  loop {
    let mut fds: Vec<libc::pollfd> = inputs
      .fds()
      .into_iter()
      .map(|fd| libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
      })
      .collect();
    let wait = rescan_at.saturating_duration_since(Instant::now()).as_millis() as i32;
    // SAFETY: `fds` is valid for its length. A hung-up device returns at once and is dropped by `drain`.
    unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, wait) };
    let area = *lock(&area);
    for event in inputs.drain(area) {
      let _ = msgs.send(Msg::Input(event));
    }
    if Instant::now() >= rescan_at {
      inputs.rescan();
      rescan_at = Instant::now() + INPUT_RESCAN;
    }
  }
}

/// A broken watch (the folder replaced or gone, or lost events) is reopened at once, then every 5 s while the folder
/// is missing, and the controller is told the `asleep` state it then finds.
fn status_thread(dir: PathBuf, mut watch: StatusWatch, msgs: Sender<Msg>) {
  loop {
    let mut fd = libc::pollfd {
      fd: watch.fd(),
      events: libc::POLLIN,
      revents: 0,
    };
    // SAFETY: one valid pollfd.
    unsafe { libc::poll(&mut fd, 1, -1) };
    let mut broken = false;
    for event in watch.drain() {
      let msg = match event {
        StatusEvent::Asleep(asleep) => Msg::Asleep {
          at: now_micros(),
          asleep,
        },
        StatusEvent::Scan(at) => Msg::Scan(at),
        StatusEvent::Broken => {
          broken = true;
          continue;
        }
      };
      let _ = msgs.send(msg);
    }
    if broken {
      eprintln!("the status watch broke; reopening it");
      watch = open_status_watch(&dir);
      let _ = msgs.send(Msg::Asleep {
        at: now_micros(),
        asleep: watch.asleep_now(),
      });
    }
  }
}

/// An IPC error isn't a window change, so it sends nothing and the last state stands. Errors lasting `SWAY_DOWN` clear
/// `sway_ok`, which stops `recorder.json` (being connected to sway is part of working) until sway answers again.
fn sway_thread(
  socket: PathBuf,
  app_id: String,
  area: Arc<Mutex<(u32, u32)>>,
  sway_ok: Arc<AtomicBool>,
  msgs: Sender<Msg>,
) {
  let mut failing_since = None;
  loop {
    match get_tree(&socket) {
      Ok(tree) => {
        if failing_since.take().is_some() {
          eprintln!("sway answers again");
        }
        sway_ok.store(true, Ordering::Relaxed);
        let shown = window_rect(&tree, &app_id).and_then(|(shown, rect)| {
          // Touches map onto the window even while it's hidden (a waking tap).
          *lock(&area) = (rect.width, rect.height);
          shown.then_some(rect)
        });
        let _ = msgs.send(Msg::Window {
          at: now_micros(),
          shown,
        });
      }
      Err(e) => {
        let since = *failing_since.get_or_insert_with(|| {
          eprintln!("sway: {e}");
          Instant::now()
        });
        if since.elapsed() >= SWAY_DOWN && sway_ok.swap(false, Ordering::Relaxed) {
          eprintln!("sway has failed for {} s: recorder.json stops", SWAY_DOWN.as_secs());
        }
      }
    }
    thread::sleep(TICK);
  }
}

/// Carries out the controller's actions in order. I/O errors are logged and the loop carries on.
fn execute(
  actions: Vec<Action>,
  cfg: &Config,
  open: Option<&str>,
  segments: &Mutex<Segments>,
  target: &(Mutex<Target>, Condvar),
  processing: &mut Processing,
) {
  let dir = &cfg.recordings_dir;
  for action in actions {
    let result = match &action {
      Action::Capture { generation, rect } => {
        *lock(&target.0) = Target {
          generation: *generation,
          rect: *rect,
        };
        target.1.notify_all();
        Ok(())
      }
      Action::Finish => lock(segments).finish(),
      Action::Join { from, name } => {
        // Only the finish and the take hold the store; the copy and fsync run without it, so the writer carries on.
        let taken = {
          let mut segs = lock(segments);
          segs.finish().map(|()| segs.take(*from))
        };
        taken.and_then(|taken| taken.join(dir, name))
      }
      Action::Prune(older_than) => lock(segments).prune(*older_than),
      Action::Text { name, at, line } => textlog::append(dir, name, *at, line),
      Action::Recording(name) => status::set_recording(&cfg.status_dir, name.as_deref()),
      Action::Log(line) => {
        eprintln!("{line}");
        Ok(())
      }
      Action::Closed(_) => {
        processing.scan();
        processing.poll();
        enforce_cap(dir, cfg.cap_bytes, open).map(|deleted| {
          for name in deleted {
            eprintln!("clip {} deleted to stay under the cap", dir.join(name).display());
          }
        })
      }
    };
    if let Err(e) = result {
      eprintln!("{action:?}: {e}");
    }
  }
}

fn live(cfg: Config) -> ! {
  let watch = open_status_watch(&cfg.status_dir);
  match recover(&cfg.recordings_dir, &cfg.status_dir) {
    Ok(Some(name)) => eprintln!("clip {} recovered", cfg.recordings_dir.join(name).display()),
    Ok(None) => {}
    Err(e) => eprintln!("recovery: {e}"),
  }
  if let Err(e) = enforce_cap(&cfg.recordings_dir, cfg.cap_bytes, None) {
    eprintln!("storage cap: {e}");
  }
  let mut processing = Processing {
    dir: cfg.recordings_dir.clone(),
    queue: BTreeSet::new(),
    running: None,
    failed: HashSet::new(),
  };
  processing.scan();
  processing.poll();

  // Both checked before `recorder.json` is first written: it reports a recorder that can capture and encode.
  let capture = Capture::connect(&cfg.wayland_socket).unwrap_or_else(|e| fatal("capture", e));
  if let Err(e) = H264Encoder::new(Path::new(OPENH264), 16, 16) {
    fatal("encoder", e);
  }
  let segments = Segments::open(cfg.recordings_dir.join("live")).unwrap_or_else(|e| fatal("segments", e));
  let segments = Arc::new(Mutex::new(segments));
  let target = Arc::new((Mutex::new(Target::default()), Condvar::new()));
  let area = Arc::new(Mutex::new((0, 0)));
  let (msgs, inbox) = mpsc::channel();
  let (frames_tx, frames_rx) = mpsc::sync_channel(2);

  let min_gap = Duration::from_secs(1) / cfg.max_fps.max(1);
  let (t, m) = (target.clone(), msgs.clone());
  thread::spawn(move || capture_thread(capture, t, min_gap, m, frames_tx));
  let (t, s, keyframe) = (target.clone(), segments.clone(), cfg.keyframe);
  thread::spawn(move || {
    writer_thread(frames_rx, t, s, keyframe, |w, h| {
      H264Encoder::new(Path::new(OPENH264), w, h)
    })
  });
  let (a, m) = (area.clone(), msgs.clone());
  thread::spawn(move || input_thread(a, m));
  let asleep = watch.asleep_now();
  let (d, m) = (cfg.status_dir.clone(), msgs.clone());
  thread::spawn(move || status_thread(d, watch, m));
  let (socket, app_id, m) = (cfg.sway_socket.clone(), cfg.clock_app_id.clone(), msgs);
  let sway_ok = Arc::new(AtomicBool::new(true));
  let ok = sway_ok.clone();
  thread::spawn(move || sway_thread(socket, app_id, area, ok, m));

  let mut ctl = Controller::new(&cfg, asleep);
  let env = LiveEnv {
    segments: &segments,
    dir: &cfg.recordings_dir,
  };
  let (mut next_tick, mut next_alive) = (Instant::now(), Instant::now());
  loop {
    let msg = match inbox.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
      Ok(msg) => Some(msg),
      Err(RecvTimeoutError::Timeout) => None,
      Err(RecvTimeoutError::Disconnected) => fatal("recorder", "every reader thread stopped"),
    };
    let now = Instant::now();
    let tick = now >= next_tick;
    let mut batch: Vec<Msg> = msg.into_iter().collect();
    if tick {
      next_tick = now + TICK;
      // Everything already queued goes first: after a stall (a long join, the cap), beats sent meanwhile are waiting,
      // and a tick ahead of them would see a stale `last_change` and report a freeze that never happened.
      batch.extend(inbox.try_iter());
      batch.push(Msg::Tick(now_micros()));
    }
    for msg in batch {
      let actions = ctl.handle(msg, &env);
      execute(actions, &cfg, ctl.open_clip(), &segments, &target, &mut processing);
    }
    if tick {
      processing.poll();
    }
    if now >= next_alive && sway_ok.load(Ordering::Relaxed) {
      next_alive = now + ALIVE_EVERY;
      if let Err(e) = status::write_alive(&cfg.status_dir, now_micros(), ctl.open_clip().is_some()) {
        eprintln!("recorder.json: {e}");
      }
    }
  }
}

fn main() {
  let cfg = match fs::read_to_string(CONFIG) {
    Ok(text) => parse_config(&text).unwrap_or_else(|e| {
      eprintln!("{CONFIG}: {e}; using the defaults");
      Config::default()
    }),
    Err(_) => Config::default(),
  };
  let args: Vec<String> = std::env::args().skip(1).collect();
  match &args[..] {
    [] => live(cfg),
    [cmd, dir, name] if cmd == "process" => {
      if let Err(e) = process::run(Path::new(dir), name) {
        fatal(&format!("processing {name}"), e);
      }
    }
    _ => {
      eprintln!("usage: timeclock-recorder [process <recordings_dir> <name>]");
      exit(2);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use timeclock_recorder::clock::clip_name;

  /// `(recordings, status)` under a fresh temp dir.
  fn dirs(tag: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("recorder-main-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let (rec, status) = (root.join("recordings"), root.join("status"));
    fs::create_dir_all(&rec).unwrap();
    fs::create_dir_all(&status).unwrap();
    (rec, status)
  }

  const T0: i64 = 1_763_039_702_000_000;

  /// Two segments left behind by a crash: frames at T0, T0+0.5 s, T0+2 s.
  fn leftovers(rec: &Path) {
    let mut s = Segments::open(rec.join("live")).unwrap();
    s.begin(T0).unwrap();
    s.write(T0, b"ab").unwrap();
    s.write(T0 + 500_000, b"c").unwrap();
    s.begin(T0 + 2_000_000).unwrap();
    s.write(T0 + 2_000_000, b"d").unwrap();
    // Dropped unfinished: the crash.
  }

  /// Review Focus 5: `recording` names a clip whose `.txt` exists; recovery reuses the name.
  #[test]
  fn recovery_reuses_the_recorded_name_and_keeps_its_text() {
    let (rec, status) = dirs("reuse");
    leftovers(&rec);
    fs::write(status.join("recording"), "X").unwrap();
    fs::write(rec.join("X.txt"), "earlier line\n").unwrap();
    assert_eq!(recover(&rec, &status).unwrap(), Some("X".into()));
    assert_eq!(fs::read(rec.join("X.h264")).unwrap(), b"abcd");
    assert_eq!(fs::read_to_string(rec.join("X.frames")).unwrap().lines().count(), 3);
    let text = fs::read_to_string(rec.join("X.txt")).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines[0], "earlier line");
    assert!(lines.last().unwrap().ends_with(" recovered after a stop"), "{text}");
    assert!(!status.join("recording").exists());
    assert_eq!(fs::read_dir(rec.join("live")).unwrap().count(), 0);
  }

  #[test]
  fn recovery_without_recording_names_the_clip_from_its_first_frame() {
    let (rec, status) = dirs("fresh");
    leftovers(&rec);
    let name = clip_name(T0);
    assert_eq!(recover(&rec, &status).unwrap(), Some(name.clone()));
    assert_eq!(fs::read(rec.join(format!("{name}.h264"))).unwrap(), b"abcd");
    let text = fs::read_to_string(rec.join(format!("{name}.txt"))).unwrap();
    assert!(text.starts_with(&format!("{} clip start", timeclock_recorder::clock::local_stamp(T0))));
    assert!(text.ends_with(" recovered after a stop\n"));
  }

  /// A clip joined in full before the crash is never overwritten by a re-join of what's left.
  #[test]
  fn recovery_never_renames_over_a_complete_clip() {
    let (rec, status) = dirs("complete");
    leftovers(&rec);
    fs::write(status.join("recording"), "X").unwrap();
    for ext in ["h264", "frames", "txt"] {
      fs::write(rec.join(format!("X.{ext}")), "whole").unwrap();
    }
    let name = recover(&rec, &status).unwrap().unwrap();
    assert_ne!(name, "X");
    assert_eq!(fs::read(rec.join("X.h264")).unwrap(), b"whole");
    assert_eq!(fs::read(rec.join(format!("{name}.h264"))).unwrap(), b"abcd");
  }

  /// A join cut short after renaming the `.h264` but not the `.frames`: the segments are all still there.
  #[test]
  fn recovery_rebuilds_a_half_renamed_clip() {
    let (rec, status) = dirs("half");
    leftovers(&rec);
    fs::write(status.join("recording"), "X").unwrap();
    fs::write(rec.join("X.h264"), "ab").unwrap();
    fs::write(rec.join("X.frames.part"), "partial").unwrap();
    assert_eq!(recover(&rec, &status).unwrap(), Some("X".into()));
    assert_eq!(fs::read(rec.join("X.h264")).unwrap(), b"abcd");
    assert!(rec.join("X.frames").exists());
    assert!(!rec.join("X.frames.part").exists());
  }

  #[test]
  fn with_nothing_left_recovery_only_clears_up() {
    let (rec, status) = dirs("none");
    fs::write(status.join("recording"), "X").unwrap();
    fs::write(rec.join("Y.h264.part"), "x").unwrap();
    fs::write(rec.join("Y.mp4.part"), "x").unwrap();
    fs::write(rec.join("Z.h264"), "keep").unwrap();
    assert_eq!(recover(&rec, &status).unwrap(), None);
    assert!(!status.join("recording").exists());
    assert!(!rec.join("Y.h264.part").exists() && !rec.join("Y.mp4.part").exists());
    assert!(rec.join("Z.h264").exists());
  }

  #[test]
  fn processing_queues_clips_without_an_mp4_oldest_first() {
    let (rec, _) = dirs("queue");
    for (name, exts) in [
      ("b", &["h264", "frames"][..]),
      ("a", &["h264", "frames", "txt"]),
      ("c", &["h264", "frames", "mp4"]),
      ("d", &["h264"]),
    ] {
      for ext in exts {
        fs::write(rec.join(format!("{name}.{ext}")), "").unwrap();
      }
    }
    let mut p = Processing {
      dir: rec,
      queue: BTreeSet::new(),
      running: None,
      failed: ["b".to_string()].into(),
    };
    p.scan();
    assert_eq!(p.queue.iter().collect::<Vec<_>>(), ["a"]);
  }

  /// The clip being processed has no `.mp4` yet, but a scan after a close must not queue it again; and a queued clip
  /// whose `.mp4` appeared meanwhile is skipped when its turn comes.
  #[test]
  fn processing_never_runs_a_clip_twice() {
    let (rec, _) = dirs("twice");
    for name in ["a", "b"] {
      for ext in ["h264", "frames"] {
        fs::write(rec.join(format!("{name}.{ext}")), "").unwrap();
      }
    }
    let mut p = Processing {
      dir: rec.clone(),
      queue: BTreeSet::new(),
      running: Some(("a".into(), Command::new("true").spawn().unwrap())),
      failed: HashSet::new(),
    };
    p.scan();
    assert_eq!(p.queue.iter().collect::<Vec<_>>(), ["b"]);
    if let Some((_, child)) = &mut p.running {
      child.wait().unwrap();
    }
    fs::write(rec.join("b.mp4"), "").unwrap();
    p.poll();
    assert!(p.running.is_none() && p.queue.is_empty());
  }

  struct FakeEncoder;

  impl FrameEncoder for FakeEncoder {
    fn encode(&mut self, _: &Frame, _: bool) -> Result<Vec<u8>, String> {
      Ok(vec![0, 0, 0, 1, 0x65])
    }
  }

  /// A frame queued before a stop reaches the writer after the controller bumped the generation and finished the
  /// segment: it's dropped, so no segment is left open across the stop (a waking tap's lead-in would start there).
  #[test]
  fn a_frame_queued_before_a_stop_never_begins_a_segment_after_it() {
    let (rec, _) = dirs("writer");
    let segments = Arc::new(Mutex::new(Segments::open(rec.join("live")).unwrap()));
    let target = Arc::new((
      Mutex::new(Target {
        generation: 1,
        rect: None,
      }),
      Condvar::new(),
    ));
    let (tx, rx) = mpsc::sync_channel(2);
    let (t, s) = (target.clone(), segments.clone());
    let writer = thread::spawn(move || writer_thread(rx, t, s, 2_000_000, |_, _| Ok(FakeEncoder)));
    let frame = |at| Frame {
      at,
      width: 4,
      height: 4,
      bgrx: vec![0; 64],
    };
    {
      // The controller's stop, run while the writer waits for the store with a generation-1 frame.
      let mut segs = lock(&segments);
      tx.send((1, frame(T0))).unwrap();
      thread::sleep(Duration::from_millis(200));
      lock(&target.0).generation = 2;
      segs.finish().unwrap();
    }
    tx.send((2, frame(T0 + 1_000_000))).unwrap();
    drop(tx);
    writer.join().unwrap();
    let segs = lock(&segments);
    assert!(segs.finished().is_empty());
    assert_eq!(
      segs.start_for(Micros::MAX),
      Some(T0 + 1_000_000),
      "only the current frame began a segment"
    );
  }
}
