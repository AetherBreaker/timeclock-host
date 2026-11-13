//! The live loop's decisions, free of I/O so they can be tested: messages from the reader threads in, actions for the
//! runtime (`main.rs`) out, executed in order.
use std::path::PathBuf;

use timeclock_updater::sway::Rect;

use crate::clips::{ClipCmd, ClipTracker, capturing};
use crate::clock::Micros;
use crate::config::Config;
use crate::freeze::{FreezeChange, FreezeDetector};
use crate::input::InputEvent;
use crate::textlog::Recent;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Msg {
  /// At least every 250 ms, whatever the other threads are doing.
  Tick(Micros),
  /// The sway poll: the clock window's rect while it shows.
  Window {
    at: Micros,
    shown: Option<Rect>,
  },
  Input(InputEvent),
  Asleep {
    at: Micros,
    asleep: bool,
  },
  Scan(Micros),
  /// A captured frame's heartbeat pixel, from capture generation `generation`.
  Beat {
    generation: u64,
    at: Micros,
    beat: u32,
  },
  /// SIGTERM: close the open clip properly, then the process exits.
  Stop(Micros),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
  /// Capture `rect`, or stop. Frames tagged with an older `generation` are discarded: their rect is stale.
  Capture {
    generation: u64,
    rect: Option<Rect>,
  },
  /// Finish the segment being written (capture stopped).
  Finish,
  /// Finish the segment being written, then join every segment from `from` on into clip `name`. With `rekey` (a
  /// split), the last frame written then starts a fresh segment again as a keyframe, in the same hold of the store as
  /// the take, so the next clip's video opens on the picture on screen even if the screen never changes again.
  Join {
    from: Micros,
    name: String,
    rekey: bool,
  },
  /// Delete segments that ended before this.
  Prune(Micros),
  Text {
    name: String,
    at: Micros,
    line: String,
  },
  /// Write `status/recording`, or remove it.
  Recording(Option<String>),
  Log(String),
  /// Clip `name` is complete: queue its processing and enforce the storage cap.
  Closed(String),
}

/// What the controller needs to ask of the disk.
pub trait Env {
  /// `Segments::start_for`.
  fn start_for(&self, t: Micros) -> Option<Micros>;
  /// `unique_clip_name` in the recordings folder.
  fn unique_name(&self, at: Micros) -> String;
}

pub struct Controller {
  dir: PathBuf,
  before: Micros,
  keyframe: Micros,
  clips: ClipTracker,
  freeze: FreezeDetector,
  /// Event lines while no clip is open, for the lead-in of the next one.
  recent: Recent,
  /// The open clip's name and its join start. A split's next clip keeps the start: the joined segments are gone, so
  /// everything left from it on is the next clip's, a frame stamped just before the split included.
  clip: Option<(String, Micros)>,
  freeze_start: Option<Micros>,
  window: Option<Rect>,
  asleep: bool,
  generation: u64,
  target: Option<Rect>,
}

impl Controller {
  pub fn new(cfg: &Config, asleep: bool) -> Self {
    Self {
      dir: cfg.recordings_dir.clone(),
      before: cfg.before,
      keyframe: cfg.keyframe,
      clips: ClipTracker::new(cfg.after, cfg.max_clip),
      freeze: FreezeDetector::new(cfg.freeze_frames, cfg.freeze),
      recent: Recent::new(cfg.before + cfg.keyframe),
      clip: None,
      freeze_start: None,
      window: None,
      asleep,
      generation: 0,
      target: None,
    }
  }

  pub fn open_clip(&self) -> Option<&str> {
    self.clip.as_ref().map(|(name, _)| name.as_str())
  }

  pub fn handle(&mut self, msg: Msg, env: &dyn Env) -> Vec<Action> {
    let mut out = Vec::new();
    let now = match msg {
      Msg::Tick(now) => {
        if let Some(FreezeChange::Started { at }) = self.freeze.on_tick(now) {
          self.freeze_started(at, env, &mut out);
        }
        match self.clips.poll(now) {
          Some(ClipCmd::Close) => self.close(now, None, &mut out),
          Some(ClipCmd::Split) if let Some((prev, from)) = self.clip.clone() => {
            let next = env.unique_name(now);
            self.close(now, Some(&next), &mut out);
            self.clips.opened(now);
            out.push(Action::Recording(Some(next.clone())));
            self.text(&next, now, "clip start", &mut out);
            self.text(&next, now, &format!("continued from {prev}"), &mut out);
            if let Some(start) = self.freeze_start {
              self.text(&next, start, "freeze start", &mut out);
            }
            out.push(Action::Log(format!("clip {} started", self.dir.join(&next).display())));
            self.clip = Some((next, from));
          }
          _ => {}
        }
        if self.clip.is_none() {
          out.push(Action::Prune(now - (self.before + self.keyframe)));
        }
        now
      }
      Msg::Window { at, shown } => {
        match (self.window, shown) {
          (Some(_), None) => {
            self.event(at, "window gone".into(), true, env, &mut out);
          }
          (None, Some(_)) => {
            self.freeze.reset();
            self.event(at, "window shown".into(), false, env, &mut out);
          }
          _ => {}
        }
        self.window = shown;
        at
      }
      Msg::Input(InputEvent::Touch { at, x, y }) => self.event(at, format!("touch {x} {y}"), true, env, &mut out),
      Msg::Input(InputEvent::Click { at, pos: Some((x, y)) }) => {
        self.event(at, format!("click {x} {y}"), true, env, &mut out)
      }
      Msg::Input(InputEvent::Click { at, pos: None }) => self.event(at, "click".into(), true, env, &mut out),
      Msg::Input(InputEvent::Key { at }) => self.event(at, "key".into(), true, env, &mut out),
      Msg::Input(InputEvent::Motion { at }) => {
        self.clips.activity(at);
        at
      }
      Msg::Asleep { at, asleep } => {
        if asleep != self.asleep {
          self.asleep = asleep;
          self.event(at, if asleep { "asleep" } else { "awake" }.into(), false, env, &mut out);
        }
        at
      }
      Msg::Scan(at) => self.event(at, "scan".into(), true, env, &mut out),
      Msg::Stop(at) => {
        // A plain close, joined and ended, so `recording` goes (recovery relies on it only after a crash). Processing
        // and the cap are left to the next start: the process is about to exit.
        self.close(at, None, &mut out);
        out.retain(|a| !matches!(a, Action::Closed(_)));
        // Then every segment no clip holds goes: a clean stop isn't a crash, so it leaves nothing for recovery to turn
        // into a clip. The generation bump first, so the writer drops queued frames instead of starting a segment.
        self.generation += 1;
        self.target = None;
        out.push(Action::Capture {
          generation: self.generation,
          rect: None,
        });
        out.push(Action::Finish);
        out.push(Action::Prune(Micros::MAX));
        return out;
      }
      Msg::Beat { generation, at, beat } => {
        if generation == self.generation {
          match self.freeze.on_frame(at, beat) {
            Some(FreezeChange::Started { at }) => self.freeze_started(at, env, &mut out),
            Some(FreezeChange::Ended { at, length }) => self.freeze_ended(at, length, env, &mut out),
            None => {}
          }
        }
        at
      }
    };

    let want = self
      .window
      .filter(|_| capturing(true, self.asleep, self.clips.is_open()));
    if want != self.target {
      if want.is_none() {
        // No frames will come, so the detector must stop or `on_tick` would see a freeze. A freeze still running
        // (the window gone) ends first, so it stops holding the clip open.
        if self.freeze.frozen() {
          let start = self.freeze_start.unwrap_or(now);
          self.freeze_ended(now, now - start, env, &mut out);
        }
        self.freeze.reset();
      }
      self.generation += 1;
      self.target = want;
      out.push(Action::Capture {
        generation: self.generation,
        rect: want,
      });
      // After the bump: the writer re-checks the generation under the segments lock, so a frame queued before the
      // stop is either written before this `Finish` or dropped, and never begins a segment nothing would finish.
      if want.is_none() {
        out.push(Action::Finish);
      }
    }
    out
  }

  fn text(&self, name: &str, at: Micros, line: &str, out: &mut Vec<Action>) {
    out.push(Action::Text {
      name: name.into(),
      at,
      line: line.into(),
    });
  }

  /// Logs `line` to the open clip, or to `recent` with none. An interaction with no clip open opens one first:
  /// `clip start`, the lead-in lines since the footage's start, then `line`. Returns `at`.
  fn event(&mut self, at: Micros, line: String, interaction: bool, env: &dyn Env, out: &mut Vec<Action>) -> Micros {
    let opened = interaction && self.clips.interaction(at).is_some();
    if opened {
      // The join takes from the last segment start at or before the lead-in; none yet (capture just resumed) starts at
      // `at`. A static screen sends no frames, so that segment can start hours back: the clip's name, `clip start`
      // (where its video starts) and `max_clip` count from at most `before + keyframe` back.
      let from = env.start_for(at - self.before).map_or(at, |s| s.min(at));
      let start = from.max(at - self.before - self.keyframe);
      let name = env.unique_name(start);
      self.clips.opened(start);
      out.push(Action::Recording(Some(name.clone())));
      self.text(&name, start, "clip start", out);
      for (t, l) in self.recent.since(start) {
        self.text(&name, t, &l, out);
      }
      self.text(&name, at, &line, out);
      out.push(Action::Log(format!("clip {} started", self.dir.join(&name).display())));
      self.clip = Some((name, from));
    } else if let Some((name, _)) = &self.clip {
      self.text(name, at, &line, out);
    } else {
      self.recent.push(at, line);
    }
    at
  }

  /// Interaction first, so a freeze with no clip open opens one; then the freeze holds it.
  fn freeze_started(&mut self, at: Micros, env: &dyn Env, out: &mut Vec<Action>) {
    self.freeze_start = Some(at);
    self.event(at, "freeze start".into(), true, env, out);
    self.clips.freeze(true, at);
  }

  fn freeze_ended(&mut self, at: Micros, length: Micros, env: &dyn Env, out: &mut Vec<Action>) {
    self.freeze_start = None;
    self.clips.freeze(false, at);
    let line = format!("freeze end {}.{:06}", length / 1_000_000, length % 1_000_000);
    self.event(at, line, true, env, out);
  }

  /// Joins the open clip and ends its log. A split (`next`) keeps `recording` for the next clip to overwrite.
  fn close(&mut self, now: Micros, next: Option<&str>, out: &mut Vec<Action>) {
    let Some((name, from)) = self.clip.take() else {
      return;
    };
    out.push(Action::Join {
      from,
      name: name.clone(),
      rekey: next.is_some(),
    });
    if let Some(next) = next {
      self.text(&name, now, &format!("continues in {next}"), out);
    }
    self.text(&name, now, "clip end", out);
    if next.is_none() {
      out.push(Action::Recording(None));
    }
    out.push(Action::Log(format!("clip {} ended", self.dir.join(&name).display())));
    out.push(Action::Closed(name));
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::cell::Cell;

  const S: Micros = 1_000_000;
  const RECT: Rect = Rect {
    x: 0,
    y: 0,
    width: 1366,
    height: 768,
  };

  /// Segments that start every 2 s from 0; names `c1`, `c2`, ... in order.
  struct FakeEnv {
    names: Cell<u32>,
  }

  impl Env for FakeEnv {
    fn start_for(&self, t: Micros) -> Option<Micros> {
      Some(t.div_euclid(2 * S) * 2 * S).filter(|&s| s >= 0)
    }
    fn unique_name(&self, _: Micros) -> String {
      self.names.set(self.names.get() + 1);
      format!("c{}", self.names.get())
    }
  }

  fn env() -> FakeEnv {
    FakeEnv { names: Cell::new(0) }
  }

  fn ctl() -> Controller {
    let cfg = Config {
      recordings_dir: "/r".into(),
      ..Config::default()
    };
    Controller::new(&cfg, false)
  }

  fn text(name: &str, at: Micros, line: &str) -> Action {
    Action::Text {
      name: name.into(),
      at,
      line: line.into(),
    }
  }

  fn texts(actions: &[Action]) -> Vec<(String, Micros, String)> {
    actions
      .iter()
      .filter_map(|a| match a {
        Action::Text { name, at, line } => Some((name.clone(), *at, line.clone())),
        _ => None,
      })
      .collect()
  }

  fn touch(at: Micros) -> Msg {
    Msg::Input(InputEvent::Touch { at, x: 10, y: 20 })
  }

  /// A shown window at 20 s, so capture runs from there.
  fn shown(c: &mut Controller, e: &FakeEnv) -> Vec<Action> {
    c.handle(
      Msg::Window {
        at: 20 * S,
        shown: Some(RECT),
      },
      e,
    )
  }

  #[test]
  fn a_shown_window_starts_capture_and_logs_only_to_recent() {
    let (mut c, e) = (ctl(), env());
    assert_eq!(
      shown(&mut c, &e),
      [Action::Capture {
        generation: 1,
        rect: Some(RECT)
      }]
    );
  }

  #[test]
  fn a_touch_opens_a_clip_with_its_lead_in() {
    let (mut c, e) = (ctl(), env());
    shown(&mut c, &e);
    for (at, asleep) in [(28, true), (30, false)] {
      c.handle(Msg::Asleep { at: at * S, asleep }, &e);
    }
    let a = c.handle(touch(40 * S), &e);
    // Footage from the segment at or before 40 - 15 = 25 s: the one starting at 24 s.
    assert_eq!(
      a,
      [
        Action::Recording(Some("c1".into())),
        text("c1", 24 * S, "clip start"),
        text("c1", 28 * S, "asleep"),
        text("c1", 30 * S, "awake"),
        text("c1", 40 * S, "touch 10 20"),
        Action::Log("clip /r/c1 started".into()),
      ]
    );
    assert_eq!(c.open_clip(), Some("c1"));
    // The window shown at 20 s is older than the footage, so it isn't in the lead-in.
    assert!(!texts(&a).iter().any(|t| t.2 == "window shown"));
  }

  #[test]
  fn quiet_closes_the_clip() {
    let (mut c, e) = (ctl(), env());
    shown(&mut c, &e);
    c.handle(touch(40 * S), &e);
    c.handle(Msg::Input(InputEvent::Motion { at: 50 * S }), &e);
    assert!(
      !c.handle(Msg::Tick(65 * S - 1), &e)
        .iter()
        .any(|a| matches!(a, Action::Join { .. }))
    );
    let a = c.handle(Msg::Tick(65 * S), &e);
    assert_eq!(
      a,
      [
        Action::Join {
          from: 24 * S,
          name: "c1".into(),
          rekey: false,
        },
        text("c1", 65 * S, "clip end"),
        Action::Recording(None),
        Action::Log("clip /r/c1 ended".into()),
        Action::Closed("c1".into()),
        Action::Prune(65 * S - 17 * S),
      ]
    );
    assert_eq!(c.open_clip(), None);
  }

  #[test]
  fn motion_never_opens_a_clip() {
    let (mut c, e) = (ctl(), env());
    shown(&mut c, &e);
    assert!(c.handle(Msg::Input(InputEvent::Motion { at: 30 * S }), &e).is_empty());
    assert_eq!(c.open_clip(), None);
  }

  #[test]
  fn clicks_scans_and_keys_are_logged_without_detail() {
    let (mut c, e) = (ctl(), env());
    shown(&mut c, &e);
    c.handle(Msg::Scan(30 * S), &e);
    let a = c.handle(Msg::Input(InputEvent::Click { at: 31 * S, pos: None }), &e);
    let b = c.handle(
      Msg::Input(InputEvent::Click {
        at: 32 * S,
        pos: Some((5, 6)),
      }),
      &e,
    );
    let k = c.handle(Msg::Input(InputEvent::Key { at: 33 * S }), &e);
    assert_eq!(texts(&a), [("c1".into(), 31 * S, "click".into())]);
    assert_eq!(texts(&b), [("c1".into(), 32 * S, "click 5 6".into())]);
    assert_eq!(texts(&k), [("c1".into(), 33 * S, "key".into())]);
  }

  /// Ruling for Task 8: a freeze with no clip open opens one (interaction first), and holds it open.
  #[test]
  fn a_freeze_opens_and_holds_a_clip() {
    let (mut c, e) = (ctl(), env());
    shown(&mut c, &e);
    c.handle(
      Msg::Beat {
        generation: 1,
        at: 21 * S,
        beat: 0,
      },
      &e,
    );
    c.handle(
      Msg::Beat {
        generation: 1,
        at: 22 * S,
        beat: 1,
      },
      &e,
    );
    let a = c.handle(Msg::Tick(23 * S), &e);
    assert_eq!(c.open_clip(), Some("c1"));
    assert!(texts(&a).contains(&("c1".into(), 22 * S, "freeze start".into())));
    assert!(c.handle(Msg::Tick(100 * S), &e).is_empty());
    let a = c.handle(
      Msg::Beat {
        generation: 1,
        at: 101 * S,
        beat: 0,
      },
      &e,
    );
    assert_eq!(texts(&a), [("c1".into(), 101 * S, "freeze end 79.000000".into())]);
    // The heartbeat runs on; the `after` countdown restarted at the freeze's end.
    let beat = |c: &mut Controller, at: Micros| {
      c.handle(
        Msg::Beat {
          generation: 1,
          at: at * S,
          beat: (at % 2) as u32,
        },
        &e,
      )
    };
    for at in 102..116 {
      assert!(beat(&mut c, at).is_empty());
    }
    assert!(c.handle(Msg::Tick(116 * S - 1), &e).is_empty());
    beat(&mut c, 116);
    assert!(c.handle(Msg::Tick(116 * S), &e).contains(&Action::Closed("c1".into())));
  }

  #[test]
  fn beats_from_an_older_capture_are_ignored() {
    let (mut c, e) = (ctl(), env());
    shown(&mut c, &e);
    for (at, beat) in [(21, 0), (22, 1)] {
      c.handle(
        Msg::Beat {
          generation: 0,
          at: at * S,
          beat,
        },
        &e,
      );
    }
    assert!(
      c.handle(Msg::Tick(30 * S), &e)
        .iter()
        .all(|a| matches!(a, Action::Prune(_)))
    );
  }

  /// Ruling for Task 8 and Review Focus 3: the window vanishes mid-freeze. The freeze ends (and stops holding the
  /// clip) before the detector resets; capture stops and the segment finishes; the clip still closes 15 s later.
  #[test]
  fn window_gone_mid_freeze_ends_it_and_the_clip_closes_later() {
    let (mut c, e) = (ctl(), env());
    shown(&mut c, &e);
    for (at, beat) in [(21, 0), (22, 1)] {
      c.handle(
        Msg::Beat {
          generation: 1,
          at: at * S,
          beat,
        },
        &e,
      );
    }
    c.handle(Msg::Tick(23 * S), &e);
    let a = c.handle(
      Msg::Window {
        at: 30 * S,
        shown: None,
      },
      &e,
    );
    let t = texts(&a);
    assert!(t.contains(&("c1".into(), 30 * S, "window gone".into())), "{t:?}");
    assert!(
      t.contains(&("c1".into(), 30 * S, "freeze end 8.000000".into())),
      "{t:?}"
    );
    // The generation bump comes first, so the writer drops queued frames instead of beginning a segment after it.
    let stop = a.iter().position(|x| *x == Action::Finish).expect("segment finished");
    assert_eq!(
      a[stop - 1],
      Action::Capture {
        generation: 2,
        rect: None
      }
    );
    // No freeze holds it: it closes after 15 quiet seconds, and no stale tick restarts the freeze.
    assert!(c.handle(Msg::Tick(45 * S - 1), &e).is_empty());
    let a = c.handle(Msg::Tick(45 * S), &e);
    assert!(a.contains(&Action::Join {
      from: 6 * S,
      name: "c1".into(),
      rekey: false,
    }));
  }

  #[test]
  fn window_shown_again_is_logged_and_capture_resumes() {
    let (mut c, e) = (ctl(), env());
    shown(&mut c, &e);
    c.handle(
      Msg::Window {
        at: 30 * S,
        shown: None,
      },
      &e,
    );
    let a = c.handle(
      Msg::Window {
        at: 31 * S,
        shown: Some(RECT),
      },
      &e,
    );
    assert_eq!(texts(&a), [("c1".into(), 31 * S, "window shown".into())]);
    assert!(a.contains(&Action::Capture {
      generation: 3,
      rect: Some(RECT)
    }));
  }

  #[test]
  fn asleep_stops_capture_once_no_clip_is_open_and_a_touch_restarts_it() {
    let (mut c, e) = (ctl(), env());
    shown(&mut c, &e);
    c.handle(touch(30 * S), &e);
    let a = c.handle(
      Msg::Asleep {
        at: 31 * S,
        asleep: true,
      },
      &e,
    );
    assert_eq!(texts(&a), [("c1".into(), 31 * S, "asleep".into())]);
    assert!(
      !a.iter().any(|x| matches!(x, Action::Capture { .. })),
      "the open clip runs on"
    );
    let a = c.handle(Msg::Tick(45 * S), &e);
    let join = a.iter().position(|x| matches!(x, Action::Join { .. })).unwrap();
    let stop = a.iter().position(|x| *x == Action::Finish).unwrap();
    assert!(join < stop);
    assert_eq!(
      a[stop - 1],
      Action::Capture {
        generation: 2,
        rect: None
      }
    );
    // A repeat of the same state (a reopened status watch) logs nothing.
    assert!(
      c.handle(
        Msg::Asleep {
          at: 46 * S,
          asleep: true
        },
        &e
      )
      .is_empty()
    );
    let a = c.handle(touch(50 * S), &e);
    assert!(a.contains(&Action::Recording(Some("c2".into()))));
    assert!(a.contains(&Action::Capture {
      generation: 3,
      rect: Some(RECT)
    }));
  }

  #[test]
  fn a_long_clip_splits_and_carries_on() {
    let (mut c, e) = (ctl(), env());
    shown(&mut c, &e);
    c.handle(touch(30 * S), &e);
    for at in [(31, 0), (32, 1)] {
      c.handle(
        Msg::Beat {
          generation: 1,
          at: at.0 * S,
          beat: at.1,
        },
        &e,
      );
    }
    c.handle(Msg::Tick(34 * S), &e); // freeze from 32 s holds the clip open
    // Footage from 14 s; 600 s later it splits.
    assert!(c.handle(Msg::Tick(614 * S - 1), &e).is_empty());
    let a = c.handle(Msg::Tick(614 * S), &e);
    assert_eq!(
      a,
      [
        // The split re-keys, so c2's video opens on the frozen picture.
        Action::Join {
          from: 14 * S,
          name: "c1".into(),
          rekey: true,
        },
        text("c1", 614 * S, "continues in c2"),
        text("c1", 614 * S, "clip end"),
        Action::Log("clip /r/c1 ended".into()),
        Action::Closed("c1".into()),
        Action::Recording(Some("c2".into())),
        text("c2", 614 * S, "clip start"),
        text("c2", 614 * S, "continued from c1"),
        // The freeze timer in c2's copy counts from the real start.
        text("c2", 32 * S, "freeze start"),
        Action::Log("clip /r/c2 started".into()),
      ]
    );
    assert_eq!(c.open_clip(), Some("c2"));
    assert!(c.handle(Msg::Tick(1214 * S - 1), &e).is_empty());
    assert!(c.handle(Msg::Tick(1214 * S), &e).contains(&Action::Join {
      from: 14 * S,
      name: "c2".into(),
      rekey: true,
    }));
  }

  /// SIGTERM mid-clip: joined and ended like a close, `recording` cleared, no processing started.
  #[test]
  fn stop_closes_the_open_clip() {
    let (mut c, e) = (ctl(), env());
    shown(&mut c, &e);
    c.handle(touch(30 * S), &e);
    // Then capture stops and every unheld segment goes, so a clean restart leaves nothing to recover.
    let cleanup = |generation| {
      [
        Action::Capture { generation, rect: None },
        Action::Finish,
        Action::Prune(Micros::MAX),
      ]
    };
    assert_eq!(
      c.handle(Msg::Stop(33 * S), &e),
      [
        &[
          Action::Join {
            from: 14 * S,
            name: "c1".into(),
            rekey: false,
          },
          text("c1", 33 * S, "clip end"),
          Action::Recording(None),
          Action::Log("clip /r/c1 ended".into()),
        ][..],
        &cleanup(2),
      ]
      .concat()
    );
    assert_eq!(ctl().handle(Msg::Stop(33 * S), &e), cleanup(1));
  }

  /// A stop when the clip is already due to split still closes it plainly: no `continues in`, no continuation clip.
  #[test]
  fn stop_never_splits() {
    let (mut c, e) = (ctl(), env());
    shown(&mut c, &e);
    for (at, beat) in [(30, 0), (31, 1)] {
      c.handle(
        Msg::Beat {
          generation: 1,
          at: at * S,
          beat,
        },
        &e,
      );
    }
    c.handle(Msg::Tick(33 * S), &e); // the freeze opens c1 and holds it past max_clip
    let a = c.handle(Msg::Stop(700 * S), &e);
    let t = texts(&a);
    assert!(t.iter().all(|(name, ..)| name == "c1"), "{t:?}");
    assert!(!t.iter().any(|(_, _, line)| line.starts_with("continues in")), "{t:?}");
    assert!(a.contains(&Action::Join {
      from: 16 * S,
      name: "c1".into(),
      rekey: false,
    }));
  }

  /// A static screen's segment can start hours back: the join still takes it, but the clip's name and `clip start`
  /// count from at most `before + keyframe` back.
  #[test]
  fn an_hours_old_segment_doesnt_move_the_clip_start() {
    struct Old;
    impl Env for Old {
      fn start_for(&self, _: Micros) -> Option<Micros> {
        Some(0)
      }
      fn unique_name(&self, at: Micros) -> String {
        format!("n{}", at / S)
      }
    }
    let mut c = ctl();
    c.handle(touch(7_200 * S), &Old);
    assert_eq!(c.open_clip(), Some("n7183"));
    let a = c.handle(Msg::Tick(7_215 * S), &Old);
    assert_eq!(
      a[0],
      Action::Join {
        from: 0,
        name: "n7183".into(),
        rekey: false,
      }
    );
  }

  #[test]
  fn with_no_segments_the_footage_starts_at_the_interaction() {
    struct Empty;
    impl Env for Empty {
      fn start_for(&self, _: Micros) -> Option<Micros> {
        None
      }
      fn unique_name(&self, _: Micros) -> String {
        "n".into()
      }
    }
    let mut c = ctl();
    let a = c.handle(touch(30 * S), &Empty);
    assert_eq!(a[1], text("n", 30 * S, "clip start"));
  }
}
