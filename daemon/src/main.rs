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
use std::path::PathBuf;
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
/// login): 1 s doubling to 30 s, giving up after 6 attempts. udev add/change
/// events (including logind reapplying ACLs) start the node over, so giving
/// up never strands a device — it just stops a hopeless timer.
const RETRY_BASE: Duration = Duration::from_secs(1);
const RETRY_CAP: Duration = Duration::from_secs(30);
const RETRY_MAX_ATTEMPTS: u32 = 6;

struct Retry {
    node: HidrawNode,
    at: Instant,
}

/// The reader reports how it ended over `done_tx` (never sent when the task
/// is aborted on unplug — the udev `Removed` arm already cleans up).
fn spawn_reader(
    node: HidrawNode,
    registry: Arc<Registry>,
    done_tx: mpsc::Sender<(HidrawNode, reader::End)>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let end = reader::run(&node.devnode, &node.key, &node.name, registry).await;
        let _ = done_tx.send((node, end)).await;
    })
}

// A current-thread runtime is plenty for this workload (a handful of mostly
// parked tasks) and keeps the daemon to a couple of OS threads instead of
// one worker per core.
//
// One select loop owns all mutable supervision state (readers, retries,
// attempts); splitting it into functions would only scatter that state.
#[allow(clippy::too_many_lines)]
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

    // Readers report their end over this channel; main holds a sender for
    // spawning, so `recv` never sees a closed channel.
    let (done_tx, mut done_rx) = mpsc::channel::<(HidrawNode, reader::End)>(16);

    let mut readers: HashMap<PathBuf, JoinHandle<()>> = HashMap::new();
    for node in discovery::scan()? {
        info!(dev = %node.devnode.display(), key = node.key, "found device");
        readers.insert(
            node.devnode.clone(),
            spawn_reader(node, registry.clone(), done_tx.clone()),
        );
    }
    if readers.is_empty() {
        info!("no Steam Controller devices present; waiting for hotplug");
    }

    // Failed readers scheduled for another attempt. The retry timer select
    // arm only exists while this is non-empty — the steady state (everything
    // open or nothing present) runs with no timer at all; recovery from
    // open failures is otherwise event-driven via udev add/change.
    let mut retries: Vec<Retry> = Vec::new();
    let mut attempts: HashMap<PathBuf, u32> = HashMap::new();

    loop {
        let next_retry = retries.iter().map(|r| r.at).min();
        tokio::select! {
            Some((node, end)) = done_rx.recv() => {
                // The reader ended on its own; drop its dead handle.
                readers.remove(&node.devnode);
                match end {
                    reader::End::DeviceGone => {
                        attempts.remove(&node.devnode);
                    }
                    reader::End::OpenFailed(e) | reader::End::ReadFailed(e) => {
                        let attempt = attempts.entry(node.devnode.clone()).or_insert(0);
                        if *attempt >= RETRY_MAX_ATTEMPTS {
                            warn!(dev = %node.devnode.display(), error = %e,
                                  "reader keeps failing; waiting for a udev event");
                        } else {
                            let delay = (RETRY_BASE * 2u32.pow(*attempt)).min(RETRY_CAP);
                            debug!(dev = %node.devnode.display(), error = %e, attempt = *attempt,
                                   ?delay, "reader failed; scheduling retry");
                            *attempt += 1;
                            retries.push(Retry { node, at: Instant::now() + delay });
                        }
                    }
                }
            }
            () = async { tokio::time::sleep_until(next_retry.unwrap()).await },
                if next_retry.is_some() =>
            {
                let now = Instant::now();
                let (due, later): (Vec<_>, Vec<_>) =
                    std::mem::take(&mut retries).into_iter().partition(|r| r.at <= now);
                retries = later;
                for retry in due {
                    if readers.get(&retry.node.devnode).is_none_or(JoinHandle::is_finished) {
                        info!(dev = %retry.node.devnode.display(), "retrying device");
                        readers.insert(
                            retry.node.devnode.clone(),
                            spawn_reader(retry.node, registry.clone(), done_tx.clone()),
                        );
                    }
                }
            }
            event = event_rx.recv() => match event {
                Some(Event::Added(node)) => {
                    // Fresh udev signal: forget past failures and pending
                    // retries; if no live reader exists, start over now.
                    attempts.remove(&node.devnode);
                    retries.retain(|r| r.node.devnode != node.devnode);
                    if readers.get(&node.devnode).is_none_or(JoinHandle::is_finished) {
                        info!(dev = %node.devnode.display(), key = node.key, "device added");
                        readers.insert(
                            node.devnode.clone(),
                            spawn_reader(node, registry.clone(), done_tx.clone()),
                        );
                    }
                }
                Some(Event::Removed(devnode)) => {
                    attempts.remove(&devnode);
                    retries.retain(|r| r.node.devnode != devnode);
                    if let Some(task) = readers.remove(&devnode) {
                        info!(dev = %devnode.display(), "device removed");
                        // Abort; the reader's ReleaseGuard drops the registry ref.
                        task.abort();
                    }
                }
                None => return Err(anyhow::anyhow!("udev monitor thread exited")),
            },
            Some(()) = resume_rx.recv() => {
                // Readers that died before the suspend (their devnode may
                // have re-enumerated without a udev event we saw) get one
                // immediate rescan; hotplug events remain the primary path.
                readers.retain(|_, task| !task.is_finished());
                match discovery::scan() {
                    Ok(nodes) => for node in nodes {
                        if !readers.contains_key(&node.devnode) {
                            info!(dev = %node.devnode.display(), key = node.key, "device found after resume");
                            attempts.remove(&node.devnode);
                            retries.retain(|r| r.node.devnode != node.devnode);
                            readers.insert(
                                node.devnode.clone(),
                                spawn_reader(node, registry.clone(), done_tx.clone()),
                            );
                        }
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
