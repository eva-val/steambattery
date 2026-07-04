//! Suspend/resume awareness via logind's `PrepareForSleep` signal.
//!
//! The monotonic clock freezes during suspend, so nothing timer-based can
//! notice that a controller vanished while the machine slept — on resume it
//! would look fresh for a full staleness window. logind's signal is the only
//! reliable resume edge: it rewinds the registry's liveness horizon (see
//! [`Registry::note_resume`]) and pings the main loop to rescan for devices
//! whose readers died before the suspend.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::state::Registry;

/// Reconnect backoff for the system-bus connection: 5 s doubling to 60 s.
/// A system bus restart is a rare, machine-wide event; the daemon must
/// degrade to timer-less operation without resume awareness, not crash-loop
/// (unlike the session bus, whose death legitimately ends the session).
const RECONNECT_BASE: Duration = Duration::from_secs(5);
const RECONNECT_CAP: Duration = Duration::from_mins(1);

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait LogindManager {
    /// `start` is true just before the machine sleeps, false on resume.
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

async fn watch(registry: &Registry, resume_tx: &mpsc::Sender<()>) -> zbus::Result<()> {
    let conn = zbus::Connection::system().await?;
    let manager = LogindManagerProxy::new(&conn).await?;
    let mut stream = manager.receive_prepare_for_sleep().await?;
    info!("watching logind for suspend/resume");

    while let Some(signal) = stream.next().await {
        match signal.args() {
            Ok(args) if *args.start() => debug!("system suspending"),
            Ok(_) => {
                info!("system resumed");
                registry.note_resume();
                // Wake the main loop for a rescan; if it's mid-iteration a
                // single queued ping is enough (capacity 1, drop extras).
                let _ = resume_tx.try_send(());
            }
            Err(e) => warn!(error = %e, "bad PrepareForSleep signal"),
        }
    }
    Ok(())
}

/// Run the watcher forever, reconnecting with backoff. Never returns an
/// error — suspend awareness is best-effort.
pub async fn run(registry: Arc<Registry>, resume_tx: mpsc::Sender<()>) {
    let mut delay = RECONNECT_BASE;
    loop {
        match watch(&registry, &resume_tx).await {
            Ok(()) => warn!("logind signal stream ended; reconnecting"),
            Err(e) => warn!(error = %e, "logind watcher failed; retrying"),
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(RECONNECT_CAP);
    }
}
