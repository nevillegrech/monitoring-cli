//! A machine-wide queue for the account's concurrent-query limit.
//!
//! The server refuses a query when the account already runs its limit of them. Commands on one
//! machine therefore take one of that many lock files first and wait their turn here, without
//! asking the server. Other machines and the web app share the same limit, so the caller still
//! retries when the server refuses.

use std::fs::{self, File, TryLockError};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::error::{Fail, Res};

/// The server's limit of concurrent queries per account.
pub const DEFAULT_SLOTS: usize = 3;
pub const SLOTS_ENV: &str = "DEDAUB_MONITORING_SLOTS";

/// Holds a slot until dropped.
pub struct Slot(#[allow(dead_code)] File);

pub fn slot_count() -> usize {
    std::env::var(SLOTS_ENV).ok().and_then(|v| v.parse().ok()).filter(|n| *n > 0).unwrap_or(DEFAULT_SLOTS)
}

/// A different starting point per process, so that waiting commands do not all poll slot 0.
fn jitter() -> u64 {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(0);
    nanos ^ ((std::process::id() as u64) << 16)
}

/// Take a free slot, waiting until `deadline` for one. `Ok(None)` when the lock files cannot be
/// used at all (an unwritable state directory): the caller then relies on the server's refusals.
pub fn acquire(dir: &Path, count: usize, deadline: Instant, interrupted: &AtomicBool) -> Res<Option<Slot>> {
    let _ = fs::create_dir_all(dir);
    let mut round = jitter();
    loop {
        let mut usable = false;
        for i in 0..count {
            let path = dir.join(format!("slot-{}.lock", (i + round as usize) % count));
            // A sandbox may allow reading the state directory only; a lock needs no write access.
            let file = fs::OpenOptions::new().create(true).append(true).open(&path).or_else(|_| File::open(&path));
            let Ok(file) = file else { continue };
            match file.try_lock() {
                Ok(()) => return Ok(Some(Slot(file))),
                Err(TryLockError::WouldBlock) => usable = true,
                Err(TryLockError::Error(_)) => {}
            }
        }
        if !usable {
            return Ok(None);
        }
        if interrupted.load(Ordering::SeqCst) {
            return Err(Fail::Interrupted);
        }
        if Instant::now() >= deadline {
            return Err(Fail::Busy(format!(
                "all {count} query slots on this machine stayed busy; raise --wait or run fewer commands at once"
            )));
        }
        round = round.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        std::thread::sleep(Duration::from_millis(150 + (round >> 33) % 250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dm-slots-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn hands_out_each_slot_once_and_frees_it_on_drop() {
        let dir = scratch("basic");
        let idle = AtomicBool::new(false);
        let soon = || Instant::now() + Duration::from_millis(50);
        let a = acquire(&dir, 2, soon(), &idle).unwrap().unwrap();
        let b = acquire(&dir, 2, soon(), &idle).unwrap().unwrap();
        assert!(matches!(acquire(&dir, 2, soon(), &idle), Err(Fail::Busy(_))));
        drop(a);
        assert!(acquire(&dir, 2, soon(), &idle).unwrap().is_some());
        drop(b);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stops_waiting_on_an_interrupt() {
        let dir = scratch("interrupt");
        let idle = AtomicBool::new(false);
        let held = acquire(&dir, 1, Instant::now(), &idle).unwrap().unwrap();
        let interrupted = AtomicBool::new(true);
        let later = Instant::now() + Duration::from_secs(30);
        assert_eq!(acquire(&dir, 1, later, &interrupted).err(), Some(Fail::Interrupted));
        drop(held);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unusable_directory_means_no_local_queue() {
        let idle = AtomicBool::new(false);
        let slot = acquire(Path::new("/proc/dm-no-such-dir"), 3, Instant::now(), &idle).unwrap();
        assert!(slot.is_none());
    }
}
