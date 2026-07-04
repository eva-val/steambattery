//! Suspend/resume awareness via logind's `PrepareForSleep` signal.
//!
//! The monotonic clock freezes during suspend, so nothing timer-based can
//! notice that a controller vanished while the machine slept — on resume it
//! would look fresh for a full staleness window. logind's signal is the only
//! reliable resume edge: it rewinds the registry's liveness horizon (see
//! [`Registry::note_resume`]) and pings the main loop to rescan for devices
//! whose readers died before the suspend.

use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// A watch that survived this long was healthy: reset the backoff so a
/// later, genuine bus restart reconnects in `RECONNECT_BASE`, not at a
/// lifetime-ratcheted cap. The reconnect gap matters more here than usual —
/// D-Bus signals are not replayed, so a `PrepareForSleep(false)` fired
/// while disconnected is lost outright and that resume goes unhandled.
const HEALTHY_CONNECTION: Duration = Duration::from_mins(1);

/// Consecutive short-lived attempts before concluding logind isn't coming
/// (no system bus: container, non-systemd) and stopping for good — the
/// stated degradation is *timer-less* operation, not a connect attempt
/// every minute forever.
const MAX_QUICK_FAILURES: u32 = 10;

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

/// Run the watcher, reconnecting with backoff; a healthy connection resets
/// the backoff, and a bus that never comes up stops the watcher for good.
/// Never returns an error — suspend awareness is best-effort.
pub async fn run(registry: Arc<Registry>, resume_tx: mpsc::Sender<()>) {
    let mut delay = RECONNECT_BASE;
    let mut quick_failures = 0u32;
    loop {
        let started = Instant::now();
        match watch(&registry, &resume_tx).await {
            Ok(()) => warn!("logind signal stream ended; reconnecting"),
            Err(e) => warn!(error = %e, "logind watcher failed; retrying"),
        }
        if started.elapsed() >= HEALTHY_CONNECTION {
            delay = RECONNECT_BASE;
            quick_failures = 0;
        } else {
            quick_failures += 1;
            if quick_failures >= MAX_QUICK_FAILURES {
                warn!("logind unreachable; running without suspend awareness");
                return;
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(RECONNECT_CAP);
    }
}
