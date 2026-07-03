//! steambatteryd — Steam Controller 2 battery telemetry daemon.
//!
//! Reads HID battery reports (0x43) from SC2 hidraw nodes and publishes
//! controller state on the session D-Bus.

mod dbus;
mod discovery;
mod protocol;
mod reader;
mod state;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info};

use discovery::{Event, HidrawNode};
use state::Registry;

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

fn spawn_reader(
    node: &HidrawNode,
    registry: &Arc<Registry>,
    done_tx: &mpsc::Sender<PathBuf>,
) -> JoinHandle<()> {
    registry.acquire(&node.key, &node.name);
    let guard = ReleaseGuard {
        registry: registry.clone(),
        key: node.key.clone(),
    };
    let registry = registry.clone();
    let done_tx = done_tx.clone();
    let devnode = node.devnode.clone();
    let key = node.key.clone();
    tokio::spawn(async move {
        let _guard = guard;
        if let Err(e) = reader::run(&devnode, &key, registry).await {
            error!(dev = %devnode.display(), error = %e, "reader failed");
        }
        let _ = done_tx.send(devnode).await;
    })
}

#[tokio::main]
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

    // Log state transitions.
    {
        let mut rx = registry.subscribe();
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let snapshot = rx.borrow_and_update().clone();
                info!(?snapshot, "state changed");
            }
        });
    }

    // Staleness sweeper.
    {
        let registry = registry.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            loop {
                tick.tick().await;
                registry.sweep();
            }
        });
    }

    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    let (done_tx, mut done_rx) = mpsc::channel::<PathBuf>(16);

    // Hotplug monitor — start before the initial scan so nothing is missed.
    // Runs on its own thread because udev types are !Send.
    {
        let event_tx = event_tx.clone();
        std::thread::spawn(move || {
            if let Err(e) = discovery::monitor_blocking(event_tx) {
                error!(error = %e, "udev monitor failed");
            }
        });
    }

    let mut readers: HashMap<PathBuf, JoinHandle<()>> = HashMap::new();
    for node in discovery::scan()? {
        info!(dev = %node.devnode.display(), key = node.key, "found device");
        readers.insert(
            node.devnode.clone(),
            spawn_reader(&node, &registry, &done_tx),
        );
    }
    if readers.is_empty() {
        info!("no Steam Controller devices present; waiting for hotplug");
    }

    // Periodic rescan: recovers devices whose reader failed to open (e.g.
    // the daemon started before the seat ACL was applied, or a device
    // re-enumerated without an ACL and got one later).
    let mut rescan = tokio::time::interval(Duration::from_secs(30));
    rescan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = rescan.tick() => {
                match discovery::scan() {
                    Ok(nodes) => {
                        for node in nodes {
                            if !readers.contains_key(&node.devnode) {
                                info!(dev = %node.devnode.display(), key = node.key, "device found on rescan");
                                readers.insert(node.devnode.clone(), spawn_reader(&node, &registry, &done_tx));
                            }
                        }
                    }
                    Err(e) => error!(error = %e, "rescan failed"),
                }
            }
            Some(event) = event_rx.recv() => match event {
                Event::Added(node) => {
                    if !readers.contains_key(&node.devnode) {
                        info!(dev = %node.devnode.display(), key = node.key, "device added");
                        readers.insert(node.devnode.clone(), spawn_reader(&node, &registry, &done_tx));
                    }
                }
                Event::Removed(devnode) => {
                    if let Some(task) = readers.remove(&devnode) {
                        info!(dev = %devnode.display(), "device removed");
                        // Abort; the reader's ReleaseGuard drops the registry ref.
                        task.abort();
                    }
                }
            },
            Some(devnode) = done_rx.recv() => {
                // Reader exited on its own (unplug seen as ENODEV/EOF, or a
                // persistent error). It already released its registry ref.
                readers.remove(&devnode);
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
