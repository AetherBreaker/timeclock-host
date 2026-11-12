//! The host updater's process: the update loop on the main thread, and a thread that tracks input so the loop reboots
//! only when nobody has touched the screen for a while.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use timeclock_updater::config::{Config, parse_config};
use timeclock_updater::keys::Inputs;
use timeclock_updater::update;

const CONFIG: &str = "/etc/timeclock/updater.conf";
const POLL: Duration = Duration::from_secs(1);
const RESCAN_EVERY: Duration = Duration::from_secs(2);

fn main() {
  let cfg = match std::fs::read_to_string(CONFIG) {
    Ok(text) => parse_config(&text).unwrap_or_else(|e| {
      eprintln!("{CONFIG}: {e}; using the defaults");
      Config::default()
    }),
    Err(_) => Config::default(),
  };
  eprintln!("updating: {cfg:?}");
  let start = Instant::now();
  // The updater reads when input last came (ms after `start`, `u64::MAX` = none yet) to reboot only when idle.
  let last_input_ms = Arc::new(AtomicU64::new(u64::MAX));
  {
    let last_input_ms = Arc::clone(&last_input_ms);
    thread::spawn(move || {
      let mut inputs = Inputs::default();
      let mut rescanned = Instant::now();
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
        // SAFETY: `fds` is a live, correctly sized array for the duration of the call.
        unsafe {
          libc::poll(
            fds.as_mut_ptr(),
            fds.len() as libc::nfds_t,
            POLL.as_millis() as libc::c_int,
          )
        };
        if inputs.drain().0 {
          last_input_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
        }
        if rescanned.elapsed() >= RESCAN_EVERY {
          inputs.rescan();
          rescanned = Instant::now();
        }
      }
    });
  }
  update::run_updater(cfg, start, last_input_ms);
}
