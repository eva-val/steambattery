//! Per-hidraw-node read loop. The kernel delivers exactly one HID report per
//! `read()`, and each open fd gets its own report queue, so this coexists
//! with Steam reading the same node.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::unix::AsyncFd;
use tracing::{debug, info, trace, warn};

use crate::protocol::{classify, Report};
use crate::state::Registry;

/// Largest report we expect (0x42 is 54 bytes); 64 covers everything.
const BUF_LEN: usize = 64;

/// How long to sleep after draining the fd before polling for readiness
/// again. The kernel queues up to `HIDRAW_BUFFER_SIZE - 1` = 63 reports per
/// open fd and drops the NEWEST on overflow, so at the 266 Hz liveness rate
/// the queue covers ~237 ms — 200 ms batches ~53 reports per wakeup without
/// losing anything, cutting the reader from ~266 wakeups/s to ~5/s.
/// `pub`: `state::RESUME_GRACE` must cover this plus `MARK_INTERVAL`
/// (const-asserted there).
pub const DRAIN_SLEEP: Duration = Duration::from_millis(200);

/// Transient read errors (e.g. an EIO hiccup on a flaky BLE link) tolerated
/// before the reader gives up; a successful read resets the count. Giving up
/// tears down the device's D-Bus object until the supervisor retries, so one
/// glitch must not be fatal. Errors take the drain-sleep branch, so the
/// budget spans ~1 s of failures, not five back-to-back syscalls.
const MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// Liveness reports arrive at ~266 Hz but their only consumer is the 10 s
/// staleness window sampled every 2 s — throttle registry marks so the
/// shared lock isn't taken hundreds of times per second.
/// `pub`: `state::RESUME_GRACE` must cover this plus `DRAIN_SLEEP`
/// (const-asserted there).
pub const MARK_INTERVAL: Duration = Duration::from_secs(1);

/// Why a reader stopped — the supervisor retries failures (with backoff)
/// but not clean unplugs.
#[derive(Debug)]
pub enum End {
    /// Unplug/EOF/ENODEV; the device object is gone, nothing to retry.
    DeviceGone,
    /// Could not open the node (typically a seat-ACL race) — worth retrying.
    OpenFailed(std::io::Error),
    /// Reads kept failing after the device opened fine.
    ReadFailed(std::io::Error),
}

/// Releases the registry ref exactly once, even when the reader task is
/// aborted mid-read (Drop runs on abort).
struct ReleaseGuard {
    registry: Arc<Registry>,
    key: String,
}

impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        self.registry.release(&self.key);
    }
}

/// Read reports from `devnode` and feed `registry` under `key` until the
/// device disappears, reads fail persistently, or the task is cancelled.
pub async fn run(devnode: &Path, key: &str, name: &str, registry: Arc<Registry>) -> End {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(devnode)
    {
        Ok(f) => f,
        Err(e) => return End::OpenFailed(e),
    };
    let afd = match AsyncFd::new(file) {
        Ok(afd) => afd,
        Err(e) => return End::OpenFailed(e),
    };

    // Only devices we can actually read appear in the registry — acquiring
    // before the open would publish (and then retract) a D-Bus object on
    // every retry of an unopenable device.
    registry.acquire(key, name);
    let guard = ReleaseGuard {
        registry,
        key: key.to_string(),
    };
    let registry = &guard.registry;
    info!(dev = %devnode.display(), key, "reader started");

    let mut buf = [0u8; BUF_LEN];
    let mut consecutive_errors = 0u32;
    let mut last_mark: Option<Instant> = None;
    loop {
        let mut ready = match afd.readable().await {
            Ok(g) => g,
            Err(e) => return End::ReadFailed(e),
        };
        // Drain everything the kernel queued while we slept, then sleep
        // again — instead of waking once per 266 Hz report. `drained` gates
        // the sleep so an idle fd parks in `readable()` with zero wakeups.
        let mut drained = false;
        loop {
            let read_result = ready.try_io(|inner| {
                // `impl Read for &File` — reads through a shared handle.
                let mut f: &File = inner.get_ref();
                f.read(&mut buf)
            });
            match read_result {
                Ok(Ok(0)) => {
                    info!(dev = %devnode.display(), "EOF, device gone");
                    return End::DeviceGone;
                }
                Ok(Ok(n)) => {
                    drained = true;
                    consecutive_errors = 0;
                    match classify(&buf[..n]) {
                        Some(Report::Battery(status)) => {
                            debug!(dev = %devnode.display(), ?status, "battery report");
                            registry.update_battery(key, status);
                        }
                        Some(Report::Liveness) => {
                            trace!(dev = %devnode.display(), "liveness");
                            if last_mark.is_none_or(|t| t.elapsed() >= MARK_INTERVAL) {
                                last_mark = Some(Instant::now());
                                registry.mark_traffic(key);
                            }
                        }
                        Some(Report::Other(id)) => {
                            trace!(dev = %devnode.display(), id = format!("{id:#04x}"), len = n, "other report");
                        }
                        None => {}
                    }
                }
                Ok(Err(e)) if e.raw_os_error() == Some(libc::ENODEV) => {
                    info!(dev = %devnode.display(), "device unplugged");
                    return End::DeviceGone;
                }
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Ok(Err(e)) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        warn!(dev = %devnode.display(), error = %e, "read failing persistently");
                        return End::ReadFailed(e);
                    }
                    debug!(dev = %devnode.display(), error = %e, "transient read error");
                    // An error does not clear readiness — looping back to
                    // `readable()` would return instantly and busy-spin, so
                    // take the sleep branch to rate-limit retries instead.
                    drained = true;
                    break;
                }
                // Readiness consumed; the queue is empty.
                Err(_would_block) => break,
            }
        }
        drop(ready);
        if drained {
            tokio::time::sleep(DRAIN_SLEEP).await;
        }
    }
}
