//! The screen recorder: records the clock's screen around interactions.
pub mod capture;
pub mod clips;
pub mod clock;
pub mod config;
pub mod control;
pub mod encode;
pub mod freeze;
pub mod input;
pub mod process;
pub mod segments;
pub mod status;
pub mod storage;
pub mod textlog;

#[cfg(test)]
pub(crate) mod testutil {
  use std::sync::Once;
  use std::sync::atomic::{AtomicU32, Ordering};

  unsafe extern "C" {
    // The `libc` crate doesn't bind it.
    fn tzset();
  }

  /// Tests share the process's zone, so every test that reads local time sets it through this, once.
  pub fn eastern() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
      // SAFETY: runs once, before any caller reads the zone.
      unsafe {
        std::env::set_var("TZ", "America/New_York");
        tzset();
      }
    });
  }

  /// A fresh, empty directory under the system temp dir; tests remove it themselves.
  pub fn temp_dir(tag: &str) -> std::path::PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("recorder-{tag}-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
  }
}
