//! Crash-safe file writing.
//!
//! Every file HubbCap owns is written the same way: to a temporary file in the
//! same folder, flushed to disk, then renamed over the target. A crash or power
//! loss leaves either the old file or the complete new one, never a torn mix.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};

/// Prefix of temporary files. Anything starting with this is leftover from an
/// interrupted write and is safe to ignore.
pub const TEMP_PREFIX: &str = ".tmp";

/// Writes `bytes` to `path` atomically, creating parent folders as needed.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent folder", path.display()))?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let mut temp = tempfile::Builder::new()
        .prefix(TEMP_PREFIX)
        .tempfile_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    temp.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("flushing {}", path.display()))?;

    test_pause();

    temp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("saving {}", path.display()))?;
    sync_dir(dir);
    Ok(())
}

/// Flushes a folder's entry list so a rename survives power loss (Unix only;
/// Windows has no equivalent call and commits renames on its own).
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Test hook: when `CAP_TEST_DELAY_MS` is set, every atomic write pauses so the
/// crash test can kill `cap` in the middle of real work.
pub fn test_pause() {
    static DELAY: OnceLock<Option<Duration>> = OnceLock::new();
    let delay = DELAY.get_or_init(|| {
        std::env::var("CAP_TEST_DELAY_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis)
    });
    if let Some(delay) = delay {
        std::thread::sleep(*delay);
    }
}
