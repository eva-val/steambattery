//! Per-hidraw-node read loop. The kernel delivers exactly one HID report per
//! `read()`, and each open fd gets its own report queue, so this coexists
//! with Steam reading the same node.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use tokio::io::unix::AsyncFd;
use tracing::{debug, info, trace, warn};

use crate::protocol::{classify, Report};
use crate::state::Registry;

/// Largest report we expect (0x42 is 54 bytes); 64 covers everything.
const BUF_LEN: usize = 64;

/// Read reports from `devnode` and feed `registry` under `key` until the
/// device disappears or the task is cancelled. Returns Ok(()) when the device
/// goes away (unplug), Err for unexpected failures (e.g. permissions).
pub async fn run(devnode: &Path, key: &str, registry: Arc<Registry>) -> anyhow::Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(devnode)
        .with_context(|| format!("opening {}", devnode.display()))?;
    let afd = AsyncFd::new(file).context("registering hidraw fd with tokio")?;
    info!(dev = %devnode.display(), key, "reader started");

    let mut buf = [0u8; BUF_LEN];
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
            Ok(Ok(n)) => match classify(&buf[..n]) {
                Some(Report::Battery(status)) => {
                    debug!(dev = %devnode.display(), ?status, "battery report");
                    registry.update_battery(key, status);
                }
                Some(Report::Liveness) => {
                    trace!(dev = %devnode.display(), "liveness");
                    registry.mark_traffic(key);
                }
                Some(Report::Other(id)) => {
                    trace!(dev = %devnode.display(), id = format!("{id:#04x}"), len = n, "other report");
                }
                None => {}
            },
            Ok(Err(e)) if e.raw_os_error() == Some(libc::ENODEV) => {
                info!(dev = %devnode.display(), "device unplugged");
                return Ok(());
            }
            Ok(Err(e)) => {
                warn!(dev = %devnode.display(), error = %e, "read error");
                return Err(e).context("reading hidraw report");
            }
            Err(_would_block) => {}
        }
    }
}
