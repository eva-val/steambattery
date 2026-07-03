//! Client side of the `io.github.steambattery` session service: state
//! fetching through the shared [`steambattery_interface`] proxies, plus an
//! iced subscription that pushes updates into the applet.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::time::Duration;

use cosmic::iced::Subscription;
use cosmic::iced::futures::channel::mpsc;
use cosmic::iced::futures::{SinkExt, StreamExt};
use cosmic::iced::stream;
use futures_util::FutureExt;
use steambattery_interface::{BUS_NAME, DaemonProxy, DeviceProxy, ROOT_PATH};
use zbus::zvariant::OwnedObjectPath;
use zbus::{MatchRule, MessageStream, fdo};

pub use steambattery_interface::{ChargeState, LEVEL_UNKNOWN};

/// Mirror of the daemon's `.Device` properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub connected: bool,
    pub charge_state: ChargeState,
    /// 0..=100, or [`LEVEL_UNKNOWN`].
    pub level: u8,
    /// mV
    pub battery_voltage: u16,
    /// mV
    pub system_voltage: u16,
    /// mV
    pub input_voltage: u16,
    /// mA
    pub current: u16,
    /// mA
    pub input_current: u16,
    /// milli-°C
    pub temperature: u16,
    /// Unix seconds; 0 = never.
    pub last_updated: u64,
}

impl DeviceInfo {
    pub const fn has_battery_data(&self) -> bool {
        self.level != LEVEL_UNKNOWN
    }

    pub const fn charging(&self) -> bool {
        self.charge_state.is_charging()
    }

    pub const fn charge_state_label(&self) -> &'static str {
        if self.has_battery_data() {
            self.charge_state.label()
        } else {
            "No data"
        }
    }
}

/// `None` = daemon not reachable on the bus.
pub type State = Option<Vec<DeviceInfo>>;

async fn fetch_device(dev: &DeviceProxy<'_>) -> zbus::Result<DeviceInfo> {
    Ok(DeviceInfo {
        name: dev.name().await?,
        connected: dev.connected().await?,
        charge_state: ChargeState::from(dev.charge_state().await?),
        level: dev.battery_level().await?,
        battery_voltage: dev.battery_voltage().await?,
        system_voltage: dev.system_voltage().await?,
        input_voltage: dev.input_voltage().await?,
        current: dev.current().await?,
        input_current: dev.input_current().await?,
        temperature: dev.temperature().await?,
        last_updated: dev.last_updated().await?,
    })
}

/// Device proxies are cached across fetches: zbus keeps their properties
/// fresh from `PropertiesChanged`, so a steady-state refetch reads from the
/// local cache instead of doing bus round-trips.
async fn fetch_state(
    daemon: &DaemonProxy<'_>,
    conn: &zbus::Connection,
    proxies: &mut HashMap<OwnedObjectPath, DeviceProxy<'static>>,
) -> zbus::Result<Vec<DeviceInfo>> {
    let devices = daemon.devices().await?;
    proxies.retain(|path, _| devices.contains(path));

    let mut out = Vec::with_capacity(devices.len());
    for path in devices {
        let dev = match proxies.entry(path) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let dev = DeviceProxy::builder(conn)
                    .path(e.key().clone())?
                    .build()
                    .await?;
                e.insert(dev)
            }
        };
        match fetch_device(dev).await {
            Ok(info) => out.push(info),
            // The daemon can remove a device object between our Devices read
            // and this fetch; failing the whole fetch would make the UI
            // claim the daemon itself is down.
            Err(e) => tracing::debug!(error = %e, "skipping device that vanished mid-fetch"),
        }
    }
    Ok(out)
}

/// Watch the daemon: refetch on any `PropertiesChanged` under our namespace,
/// on daemon start/stop, and every 30 s as a fallback.
async fn watch(output: &mut mpsc::Sender<State>) -> zbus::Result<()> {
    let conn = zbus::Connection::session().await?;
    let daemon = DaemonProxy::new(&conn).await?;
    let mut proxies = HashMap::new();

    let props_rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.DBus.Properties")?
        .path_namespace(ROOT_PATH)?
        .build();
    let mut props_stream = MessageStream::for_match_rule(props_rule, &conn, Some(32)).await?;

    let dbus_proxy = fdo::DBusProxy::new(&conn).await?;
    let mut name_stream = dbus_proxy
        .receive_name_owner_changed_with_args(&[(0, BUS_NAME)])
        .await?;

    loop {
        let state = fetch_state(&daemon, &conn, &mut proxies).await.ok();
        let _ = output.send(state).await;

        tokio::select! {
            msg = props_stream.next() => {
                if msg.is_none() {
                    // Bus connection died; reconnect from scratch.
                    return Ok(());
                }
                // Let the proxy property caches apply the same signals, and
                // coalesce bursts (several devices updating at once) into
                // one refetch.
                tokio::time::sleep(Duration::from_millis(300)).await;
                while props_stream.next().now_or_never().flatten().is_some() {}
            }
            _ = name_stream.next() => {}
            () = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
    }
}

pub fn subscription() -> Subscription<State> {
    Subscription::run_with("steambattery-dbus", |_| {
        stream::channel(4, |mut output| async move {
            loop {
                match watch(&mut output).await {
                    Ok(()) => tracing::warn!("D-Bus stream ended; reconnecting"),
                    Err(e) => {
                        tracing::warn!(error = %e, "D-Bus watcher failed; retrying");
                        let _ = output.send(None).await;
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        })
    })
}
