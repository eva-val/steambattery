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

fn spawn_reader(node: &HidrawNode, registry: &Arc<Registry>) -> JoinHandle<()> {
    let registry = registry.clone();
    let devnode = node.devnode.clone();
    let key = node.key.clone();
    let name = node.name.clone();
    tokio::spawn(async move {
        if let Err(e) = reader::run(&devnode, &key, &name, registry).await {
            error!(dev = %devnode.display(), error = %e, "reader failed");
        }
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

    // Hotplug monitor — start before the initial scan so nothing is missed.
    // Runs on its own thread because udev types are !Send. The thread owns
    // the only sender, so if it dies the channel closes and the daemon exits
    // (systemd restarts it) rather than silently losing hotplug events.
    std::thread::spawn(move || {
        if let Err(e) = discovery::monitor_blocking(event_tx) {
            error!(error = %e, "udev monitor failed");
        }
    });

    let mut readers: HashMap<PathBuf, JoinHandle<()>> = HashMap::new();
    for node in discovery::scan()? {
        info!(dev = %node.devnode.display(), key = node.key, "found device");
        readers.insert(node.devnode.clone(), spawn_reader(&node, &registry));
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
                // Readers that exited on their own (unplug seen as
                // ENODEV/EOF, a persistent error, or a panic) already
                // released their registry ref; drop their dead handles so
                // the devnode counts as absent below.
                readers.retain(|_, task| !task.is_finished());
                match discovery::scan() {
                    Ok(nodes) => {
                        for node in nodes {
                            if !readers.contains_key(&node.devnode) {
                                info!(dev = %node.devnode.display(), key = node.key, "device found on rescan");
                                readers.insert(node.devnode.clone(), spawn_reader(&node, &registry));
                            }
                        }
                    }
                    Err(e) => error!(error = %e, "rescan failed"),
                }
            }
            event = event_rx.recv() => match event {
                Some(Event::Added(node)) => {
                    // A finished handle is a dead reader for a re-enumerated
                    // devnode — replace it.
                    if readers.get(&node.devnode).is_none_or(JoinHandle::is_finished) {
                        info!(dev = %node.devnode.display(), key = node.key, "device added");
                        readers.insert(node.devnode.clone(), spawn_reader(&node, &registry));
                    }
                }
                Some(Event::Removed(devnode)) => {
                    if let Some(task) = readers.remove(&devnode) {
                        info!(dev = %devnode.display(), "device removed");
                        // Abort; the reader's ReleaseGuard drops the registry ref.
                        task.abort();
                    }
                }
                None => return Err(anyhow::anyhow!("udev monitor thread exited")),
            },
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
