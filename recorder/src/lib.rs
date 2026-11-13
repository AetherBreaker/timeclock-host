//! The screen recorder: records the clock's screen around interactions.
pub mod clips;
pub mod clock;
pub mod config;
pub mod freeze;
pub mod input;
pub mod segments;
pub mod status;
pub mod storage;
pub mod textlog;

#[cfg(test)]
pub(crate) mod testutil {
  use std::sync::atomic::{AtomicU32, Ordering};

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
