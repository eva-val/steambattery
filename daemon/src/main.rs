//! steambatteryd — Steam Controller 2 battery telemetry daemon.
//!
//! Reads HID battery reports (0x43) from SC2 hidraw nodes and publishes
//! controller state on the session D-Bus.

mod dbus;
mod discovery;
mod protocol;
mod reader;
mod state;
mod suspend;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use discovery::{Event, HidrawNode};
use state::Registry;

/// Backoff for a device whose reader failed (typically a seat-ACL race at
/// login): 1 s doubling to 30 s for the first `RETRY_FAST_ATTEMPTS`
/// consecutive failures, then one retry per `RETRY_SLOW` indefinitely. The
/// slow phase must not stop: logind applies seat ACLs directly via syscalls
/// with no uevent, so a permissions fix can arrive without any udev event to
/// restart the node — an event-only recovery path would strand a present
/// device. One wakeup per 5 min while a present device is broken is the
/// entire cost.
const RETRY_BASE: Duration = Duration::from_secs(1);
const RETRY_CAP: Duration = Duration::from_secs(30);
const RETRY_FAST_ATTEMPTS: u32 = 6;
const RETRY_SLOW: Duration = Duration::from_mins(5);

/// A reader that ran at least this long before failing was healthy: its
/// failure restarts the backoff at attempt 1 instead of escalating. Only
/// *consecutive* failures escalate — episodic faults (say, a weekly EIO
/// burst on a flaky link, each recovered by one retry) must not accumulate
/// toward the slow phase over the device's lifetime.
const HEALTHY_RUN: Duration = Duration::from_mins(1);

/// A reader's end report: the generation that spawned it, its node, and how
/// it ended. Never sent when the task is aborted on unplug — the udev
/// `Removed` arm already cleans up.
type Done = (u64, HidrawNode, reader::End);

/// A supervised reader task.
struct Reader {
    /// Distinguishes this spawn from earlier ones on the same devnode. Done
    /// reports are matched against it so a queued report from a dead
    /// predecessor can't evict a live replacement (and then double it up
    /// with a retry).
    generation: u64,
    /// Consecutive failures before this spawn (0 = clean start).
    attempt: u32,
    spawned_at: Instant,
    task: JoinHandle<()>,
}

/// A failed devnode scheduled for another attempt.
struct Retry {
    node: HidrawNode,
    /// Consecutive failures so far; carried into the respawned reader.
    attempt: u32,
    at: Instant,
}

/// All mutable supervision state, owned by the main select loop. Every path
/// that starts or stops a reader goes through `adopt`/`forget`/`on_done` so
/// the readers/retries invariants live in one place instead of being
/// copy-pasted per select arm.
struct Supervisor {
    registry: Arc<Registry>,
    done_tx: mpsc::Sender<Done>,
    generations: u64,
    readers: HashMap<PathBuf, Reader>,
    retries: Vec<Retry>,
}

impl Supervisor {
    fn spawn(&mut self, node: HidrawNode, attempt: u32) {
        self.generations += 1;
        let generation = self.generations;
        let registry = self.registry.clone();
        let done_tx = self.done_tx.clone();
        let devnode = node.devnode.clone();
        let task = tokio::spawn(async move {
            let end = reader::run(&node.devnode, &node.key, &node.name, registry).await;
            let _ = done_tx.send((generation, node, end)).await;
        });
        self.readers.insert(
            devnode,
            Reader {
                generation,
                attempt,
                spawned_at: Instant::now(),
                task,
            },
        );
    }

    /// Fresh evidence the devnode exists (initial scan, udev add/change,
    /// resume rescan): forget pending retries and start a reader unless a
    /// live one already owns the devnode.
    fn adopt(&mut self, node: HidrawNode, reason: &str) {
        self.retries.retain(|r| r.node.devnode != node.devnode);
        if self
            .readers
            .get(&node.devnode)
            .is_none_or(|r| r.task.is_finished())
        {
            info!(dev = %node.devnode.display(), key = node.key, "{reason}");
            self.spawn(node, 0);
        }
    }

    /// Unplug: drop every trace of the devnode and abort its reader (the
    /// reader's `ReleaseGuard` drops the registry ref).
    fn forget(&mut self, devnode: &Path) {
        self.retries.retain(|r| r.node.devnode != *devnode);
        if let Some(reader) = self.readers.remove(devnode) {
            info!(dev = %devnode.display(), "device removed");
            reader.task.abort();
        }
    }

    /// A reader reported its end. Ignored unless `generation` matches the
    /// tracked reader: a mismatch means the report is from a dead
    /// predecessor of a replacement `adopt` already spawned (or of a devnode
    /// `forget` already purged), and acting on it would evict the live
    /// reader and then duplicate it via retry.
    fn on_done(&mut self, generation: u64, node: HidrawNode, end: reader::End) {
        if self
            .readers
            .get(&node.devnode)
            .is_none_or(|r| r.generation != generation)
        {
            debug!(dev = %node.devnode.display(), "stale reader report; ignoring");
            return;
        }
        let reader = self
            .readers
            .remove(&node.devnode)
            .expect("generation matched above");
        match end {
            reader::End::DeviceGone => {}
            reader::End::OpenFailed(e) | reader::End::ReadFailed(e) => {
                let failures = if reader.spawned_at.elapsed() >= HEALTHY_RUN {
                    1
                } else {
                    reader.attempt + 1
                };
                let delay = if failures > RETRY_FAST_ATTEMPTS {
                    warn!(dev = %node.devnode.display(), error = %e,
                          "reader keeps failing; retrying slowly");
                    RETRY_SLOW
                } else {
                    debug!(dev = %node.devnode.display(), error = %e, failures,
                           "reader failed; scheduling retry");
                    (RETRY_BASE * 2u32.pow(failures - 1)).min(RETRY_CAP)
                };
                self.retries.push(Retry {
                    node,
                    attempt: failures,
                    at: Instant::now() + delay,
                });
            }
        }
    }

    /// Earliest scheduled retry; the select loop only arms a timer while
    /// this is Some — the steady state (everything open or nothing present)
    /// runs with no timer at all.
    fn next_retry(&self) -> Option<Instant> {
        self.retries.iter().map(|r| r.at).min()
    }

    fn fire_due_retries(&mut self) {
        let now = Instant::now();
        let mut i = 0;
        while i < self.retries.len() {
            if self.retries[i].at <= now {
                let retry = self.retries.swap_remove(i);
                if self
                    .readers
                    .get(&retry.node.devnode)
                    .is_none_or(|r| r.task.is_finished())
                {
                    info!(dev = %retry.node.devnode.display(), "retrying device");
                    self.spawn(retry.node, retry.attempt);
                }
            } else {
                i += 1;
            }
        }
    }
}

// A current-thread runtime is plenty for this workload (a handful of mostly
// parked tasks) and keeps the daemon to a couple of OS threads instead of
// one worker per core.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "steambatteryd=info".into()),
        )
        .init();

    let registry = Arc::new(Registry::new());

    // D-Bus service. If the bus goes away the daemon exits and systemd
    // restarts it.
    let mut dbus_task = tokio::spawn(dbus::run(registry.clone()));

    // Log state transitions. Deadbanding keeps this low-rate, but each line
    // is still a journald write — debug, not info.
    {
        let mut rx = registry.subscribe();
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let snapshot = rx.borrow_and_update().clone();
                debug!(?snapshot, "state changed");
            }
        });
    }

    // Staleness sweeper: sleeps until the earliest instant a device could go
    // stale, re-armed by the registry when a deadline moves earlier. With
    // nothing connected it parks without any timer.
    {
        let registry = registry.clone();
        tokio::spawn(async move {
            loop {
                match registry.next_deadline() {
                    Some(deadline) => tokio::select! {
                        () = tokio::time::sleep_until(deadline.into()) => registry.sweep(),
                        () = registry.sweep_notified() => {}
                    },
                    None => registry.sweep_notified().await,
                }
            }
        });
    }

    // Suspend/resume watcher (best-effort; see suspend.rs). Capacity 1: all
    // a resume needs is one pending rescan.
    let (resume_tx, mut resume_rx) = mpsc::channel::<()>(1);
    tokio::spawn(suspend::run(registry.clone(), resume_tx));

    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);

    // Hotplug monitor — start before the initial scan so nothing is missed.
    // Runs on its own thread because udev types are !Send. The thread owns
    // the only sender, so if it dies the channel closes and the daemon exits
    // (systemd restarts it) rather than silently losing hotplug events.
    std::thread::spawn(move || {
        if let Err(e) = discovery::monitor_blocking(event_tx) {
            error!(error = %e, "udev monitor failed");
        }
    });

    // Readers report their end over this channel; the supervisor holds a
    // sender for spawning, so `recv` never sees a closed channel.
    let (done_tx, mut done_rx) = mpsc::channel::<Done>(16);
    let mut sup = Supervisor {
        registry,
        done_tx,
        generations: 0,
        readers: HashMap::new(),
        retries: Vec::new(),
    };

    for node in discovery::scan()? {
        sup.adopt(node, "found device");
    }
    if sup.readers.is_empty() {
        info!("no Steam Controller devices present; waiting for hotplug");
    }

    loop {
        let next_retry = sup.next_retry();
        tokio::select! {
            Some((generation, node, end)) = done_rx.recv() => {
                sup.on_done(generation, node, end);
            }
            () = async { tokio::time::sleep_until(next_retry.unwrap()).await },
                if next_retry.is_some() =>
            {
                sup.fire_due_retries();
            }
            event = event_rx.recv() => match event {
                Some(Event::Added(node)) => sup.adopt(node, "device added"),
                Some(Event::Removed(devnode)) => sup.forget(&devnode),
                None => return Err(anyhow::anyhow!("udev monitor thread exited")),
            },
            Some(()) = resume_rx.recv() => {
                // Readers that died around the suspend (their devnode may
                // have re-enumerated without a udev event we saw) get one
                // immediate rescan; hotplug events remain the primary path.
                match discovery::scan() {
                    Ok(nodes) => for node in nodes {
                        sup.adopt(node, "device found after resume");
                    },
                    Err(e) => error!(error = %e, "post-resume rescan failed"),
                }
            }
            result = &mut dbus_task => {
                match result {
                    Ok(Err(e)) => return Err(e.context("D-Bus service failed")),
                    Ok(Ok(())) => unreachable!("dbus::run only returns on error"),
                    Err(e) => return Err(anyhow::anyhow!(e).context("D-Bus task panicked")),
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down");
                break;
            }
        }
    }
    Ok(())
}
