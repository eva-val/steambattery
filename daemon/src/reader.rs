//! Per-hidraw-node read loop. The kernel delivers exactly one HID report per
//! `read()`, and each open fd gets its own report queue, so this coexists
//! with Steam reading the same node.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::io::unix::AsyncFd;
use tracing::{debug, info, trace, warn};

use crate::protocol::{classify, Report};
use crate::state::Registry;

/// Largest report we expect (0x42 is 54 bytes); 64 covers everything.
const BUF_LEN: usize = 64;

/// Transient read errors (e.g. an EIO hiccup on a flaky BLE link) tolerated
/// before the reader gives up; a successful read resets the count. Giving up
/// tears down the device's D-Bus object until the 30 s rescan, so one glitch
/// must not be fatal.
const MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// Liveness reports arrive at ~266 Hz but their only consumer is the 10 s
/// staleness window sampled every 2 s — throttle registry marks so the
/// shared lock isn't taken hundreds of times per second.
const MARK_INTERVAL: Duration = Duration::from_secs(1);

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
/// device disappears or the task is cancelled. Returns Ok(()) when the device
/// goes away (unplug), Err for unexpected failures (e.g. permissions).
pub async fn run(
    devnode: &Path,
    key: &str,
    name: &str,
    registry: Arc<Registry>,
) -> anyhow::Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(devnode)
        .with_context(|| format!("opening {}", devnode.display()))?;
    let afd = AsyncFd::new(file).context("registering hidraw fd with tokio")?;

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
        let mut guard = afd.readable().await?;
        let read_result = guard.try_io(|inner| {
            // `impl Read for &File` — reads through a shared handle.
            let mut f: &File = inner.get_ref();
            f.read(&mut buf)
        });
        match read_result {
            Ok(Ok(0)) => {
                info!(dev = %devnode.display(), "EOF, device gone");
                return Ok(());
            }
            Ok(Ok(n)) => {
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
                return Ok(());
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Ok(Err(e)) => {
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    warn!(dev = %devnode.display(), error = %e, "read failing persistently");
                    return Err(e).context("reading hidraw report");
                }
                debug!(dev = %devnode.display(), error = %e, "transient read error");
            }
            Err(_would_block) => {}
        }
    }
}
