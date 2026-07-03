//! Client side of the `io.github.steambattery` session service: state
//! fetching plus an iced subscription that pushes updates into the applet.

use std::time::Duration;

use cosmic::iced::Subscription;
use cosmic::iced::futures::channel::mpsc;
use cosmic::iced::futures::{SinkExt, StreamExt};
use cosmic::iced::stream;
use futures_util::FutureExt;
use zbus::zvariant::OwnedObjectPath;
use zbus::{MatchRule, MessageStream, fdo, names::InterfaceName};

pub const BUS_NAME: &str = "io.github.steambattery";
pub const ROOT_PATH: &str = "/io/github/steambattery";
const DEVICE_IFACE: &str = "io.github.steambattery.Device";
const DAEMON_IFACE: &str = "io.github.steambattery.Daemon";

/// `BatteryLevel` sentinel for "no battery report yet".
pub const LEVEL_UNKNOWN: u8 = 0xff;

/// Mirror of the daemon's `.Device` properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub connected: bool,
    pub charging: bool,
    /// SDL3 `EChargeState` (0 reset, 1 discharging, 2 charging,
    /// 3 src-validate, 4 done); 0 until the first report.
    pub charge_state: u8,
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

    pub const fn charge_state_label(&self) -> &'static str {
        if !self.has_battery_data() {
            return "No data";
        }
        match self.charge_state {
            1 => "Discharging",
            2 => "Charging",
            3 => "Charger validating",
            4 => "Fully charged",
            _ => "Unknown",
        }
    }
}

/// `None` = daemon not reachable on the bus.
pub type State = Option<Vec<DeviceInfo>>;

async fn fetch_state(conn: &zbus::Connection) -> zbus::Result<Vec<DeviceInfo>> {
    let daemon_props = fdo::PropertiesProxy::builder(conn)
        .destination(BUS_NAME)?
        .path(ROOT_PATH)?
        .build()
        .await?;
    let devices: Vec<OwnedObjectPath> = daemon_props
        .get(InterfaceName::try_from(DAEMON_IFACE)?, "Devices")
        .await?
        .try_into()
        .map_err(zbus::Error::Variant)?;

    let mut out = Vec::with_capacity(devices.len());
    for path in devices {
        let props = fdo::PropertiesProxy::builder(conn)
            .destination(BUS_NAME)?
            .path(path)?
            .build()
            .await?;
        let all = props
            .get_all(InterfaceName::try_from(DEVICE_IFACE)?)
            .await?;
        let get_u16 = |k: &str| all.get(k).and_then(|v| v.downcast_ref::<u16>().ok());
        out.push(DeviceInfo {
            name: all
                .get("Name")
                .and_then(|v| v.downcast_ref::<String>().ok())
                .unwrap_or_default(),
            connected: all
                .get("Connected")
                .and_then(|v| v.downcast_ref::<bool>().ok())
                .unwrap_or_default(),
            charging: all
                .get("Charging")
                .and_then(|v| v.downcast_ref::<bool>().ok())
                .unwrap_or_default(),
            charge_state: all
                .get("ChargeState")
                .and_then(|v| v.downcast_ref::<u8>().ok())
                .unwrap_or_default(),
            level: all
                .get("BatteryLevel")
                .and_then(|v| v.downcast_ref::<u8>().ok())
                .unwrap_or(LEVEL_UNKNOWN),
            battery_voltage: get_u16("BatteryVoltage").unwrap_or_default(),
            system_voltage: get_u16("SystemVoltage").unwrap_or_default(),
            input_voltage: get_u16("InputVoltage").unwrap_or_default(),
            current: get_u16("Current").unwrap_or_default(),
            input_current: get_u16("InputCurrent").unwrap_or_default(),
            temperature: get_u16("Temperature").unwrap_or_default(),
            last_updated: all
                .get("LastUpdated")
                .and_then(|v| v.downcast_ref::<u64>().ok())
                .unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Watch the daemon: refetch on any `PropertiesChanged` under our namespace,
/// on daemon start/stop, and every 30 s as a fallback.
async fn watch(output: &mut mpsc::Sender<State>) -> zbus::Result<()> {
    let conn = zbus::Connection::session().await?;

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
        let state = fetch_state(&conn).await.ok();
        let _ = output.send(state).await;

        tokio::select! {
            msg = props_stream.next() => {
                if msg.is_none() {
                    // Bus connection died; reconnect from scratch.
                    return Ok(());
                }
                // One battery report fans out into several PropertiesChanged
                // signals — debounce and drain before refetching.
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
