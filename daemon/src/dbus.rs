//! Session D-Bus service: `io.github.steambattery`.
//!
//! Root object `/io/github/steambattery` (`.Daemon` interface) lists device
//! object paths; one `devN` object per logical controller (`.Device`
//! interface) carries the battery telemetry as properties with standard
//! `PropertiesChanged` signals.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use steambattery_interface::{BUS_NAME, LEVEL_UNKNOWN, ROOT_PATH};
use tracing::{debug, info, warn};
use zbus::fdo::Properties;
use zbus::object_server::{Interface, InterfaceRef};
use zbus::zvariant::{OwnedObjectPath, Value};

use crate::state::{DeviceSnapshot, Registry};

struct Daemon {
    devices: Vec<OwnedObjectPath>,
}

// zbus requires `&self` methods; const-ness is irrelevant behind the macro.
#[allow(clippy::unused_self, clippy::missing_const_for_fn)]
#[zbus::interface(name = "io.github.steambattery.Daemon")]
impl Daemon {
    /// Object paths of currently-present controller devices.
    #[zbus(property)]
    fn devices(&self) -> Vec<OwnedObjectPath> {
        self.devices.clone()
    }

    #[zbus(property)]
    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }
}

struct Device {
    snapshot: DeviceSnapshot,
}

impl Device {
    const fn battery(&self) -> Option<&crate::protocol::BatteryStatus> {
        self.snapshot.battery.as_ref()
    }
}

// zbus requires `&self` methods; const-ness is irrelevant behind the macro.
#[allow(clippy::unused_self, clippy::missing_const_for_fn)]
#[zbus::interface(name = "io.github.steambattery.Device")]
impl Device {
    #[zbus(property)]
    fn name(&self) -> &str {
        &self.snapshot.name
    }

    /// Controller traffic seen within the staleness window.
    #[zbus(property)]
    fn connected(&self) -> bool {
        self.snapshot.connected
    }

    /// SDL3 `EChargeState`: 0 `Reset`, 1 `Discharging`, 2 `Charging`,
    /// 3 `SrcValidate`, 4 `ChargingDone`. 0 until the first battery report.
    #[zbus(property)]
    fn charge_state(&self) -> u8 {
        self.battery().map_or(0, |b| b.charge_state.as_u8())
    }

    /// 0..=100, or 255 when no battery report has been received yet.
    #[zbus(property)]
    fn battery_level(&self) -> u8 {
        self.battery().map_or(LEVEL_UNKNOWN, |b| b.level)
    }

    /// mV
    #[zbus(property)]
    fn battery_voltage(&self) -> u16 {
        self.battery().map_or(0, |b| b.battery_voltage)
    }

    /// mV
    #[zbus(property)]
    fn system_voltage(&self) -> u16 {
        self.battery().map_or(0, |b| b.system_voltage)
    }

    /// mV (~5100 while on USB power, 0 unplugged)
    #[zbus(property)]
    fn input_voltage(&self) -> u16 {
        self.battery().map_or(0, |b| b.input_voltage)
    }

    /// mA
    #[zbus(property)]
    fn current(&self) -> u16 {
        self.battery().map_or(0, |b| b.current)
    }

    /// mA
    #[zbus(property)]
    fn input_current(&self) -> u16 {
        self.battery().map_or(0, |b| b.input_current)
    }

    /// milli-°C
    #[zbus(property)]
    fn temperature(&self) -> u16 {
        self.battery().map_or(0, |b| b.temperature)
    }

    /// Unix seconds of the last *published* battery change (registry
    /// deadbanding suppresses jitter-only reports); 0 = never. While
    /// `Connected` is true the data is live even if this is minutes old.
    #[zbus(property)]
    fn last_updated(&self) -> u64 {
        last_updated_secs(&self.snapshot)
    }
}

fn last_updated_secs(snap: &DeviceSnapshot) -> u64 {
    snap.last_updated
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// Every `.Device` property as a wire value — the list `emit_changes` diffs.
/// Keep in step with the `#[zbus::interface]` getters on [`Device`].
fn device_props(snap: &DeviceSnapshot) -> [(&'static str, Value<'static>); 11] {
    let b = snap.battery.as_ref();
    [
        ("Name", snap.name.clone().into()),
        ("Connected", snap.connected.into()),
        (
            "ChargeState",
            b.map_or(0, |b| b.charge_state.as_u8()).into(),
        ),
        ("BatteryLevel", b.map_or(LEVEL_UNKNOWN, |b| b.level).into()),
        ("BatteryVoltage", b.map_or(0, |b| b.battery_voltage).into()),
        ("SystemVoltage", b.map_or(0, |b| b.system_voltage).into()),
        ("InputVoltage", b.map_or(0, |b| b.input_voltage).into()),
        ("Current", b.map_or(0, |b| b.current).into()),
        ("InputCurrent", b.map_or(0, |b| b.input_current).into()),
        ("Temperature", b.map_or(0, |b| b.temperature).into()),
        ("LastUpdated", last_updated_secs(snap).into()),
    ]
}

fn device_path(index: usize) -> OwnedObjectPath {
    OwnedObjectPath::try_from(format!("{ROOT_PATH}/dev{index}"))
        .expect("static path format is always valid")
}

/// Emit a single `PropertiesChanged` carrying every property whose value
/// differs between `old` and `new` (a battery report where only some fields
/// moved signals only those fields).
async fn emit_changes(
    iface: &InterfaceRef<Device>,
    old: &DeviceSnapshot,
    new: &DeviceSnapshot,
) -> zbus::Result<()> {
    let changed: HashMap<&str, Value<'_>> = device_props(new)
        .into_iter()
        .zip(device_props(old))
        .filter(|(new, old)| new.1 != old.1)
        .map(|(new, _)| new)
        .collect();
    if changed.is_empty() {
        return Ok(());
    }
    Properties::properties_changed(
        iface.signal_emitter(),
        <Device as Interface>::name(),
        changed,
        Cow::Borrowed(&[]),
    )
    .await
}

/// Serve the bus, mirroring registry snapshots into D-Bus objects. Runs until
/// the bus connection fails.
pub async fn run(registry: Arc<Registry>) -> Result<()> {
    let conn = zbus::connection::Builder::session()
        .context("connecting to session bus")?
        .name(BUS_NAME)
        .context("requesting bus name")?
        .serve_at(ROOT_PATH, Daemon { devices: vec![] })
        .context("serving root object")?
        .build()
        .await
        .context("building D-Bus connection")?;
    info!(name = BUS_NAME, "session bus name acquired");

    let server = conn.object_server();
    let root = server
        .interface::<_, Daemon>(ROOT_PATH)
        .await
        .context("looking up root interface")?;
    let mut rx = registry.subscribe();
    // key -> devN index of the served object (which holds the mirrored
    // snapshot).
    let mut present: BTreeMap<String, usize> = BTreeMap::new();
    // Monotonic so a path is never reused for a different controller —
    // clients cache per-path property state, and a reused path with no
    // signal for its initial values would keep serving the old device.
    let mut next_index: usize = 0;

    loop {
        let snapshots = rx.borrow_and_update().clone();

        // Removals first.
        let gone: Vec<String> = present
            .keys()
            .filter(|k| !snapshots.iter().any(|s| &s.key == *k))
            .cloned()
            .collect();
        for key in gone {
            if let Some(index) = present.remove(&key) {
                let path = device_path(index);
                debug!(%path, "removing device object");
                if let Err(e) = server.remove::<Device, _>(&path).await {
                    warn!(%path, error = %e, "failed to remove device object");
                }
            }
        }

        for snap in &snapshots {
            if let Some(&index) = present.get(&snap.key) {
                let path = device_path(index);
                let iface = server
                    .interface::<_, Device>(&path)
                    .await
                    .context("looking up device interface")?;
                let old = {
                    let mut dev = iface.get_mut().await;
                    if dev.snapshot == *snap {
                        continue;
                    }
                    std::mem::replace(&mut dev.snapshot, snap.clone())
                };
                if let Err(e) = emit_changes(&iface, &old, snap).await {
                    warn!(%path, error = %e, "failed to emit PropertiesChanged");
                }
            } else {
                let index = next_index;
                next_index += 1;
                let path = device_path(index);
                info!(%path, key = snap.key, "adding device object");
                server
                    .at(
                        &path,
                        Device {
                            snapshot: snap.clone(),
                        },
                    )
                    .await
                    .context("adding device object")?;
                present.insert(snap.key.clone(), index);
            }
        }

        // Mirror the device list on the root object.
        let mut indices: Vec<usize> = present.values().copied().collect();
        indices.sort_unstable();
        let paths: Vec<OwnedObjectPath> = indices.into_iter().map(device_path).collect();
        if root.get().await.devices != paths {
            root.get_mut().await.devices = paths;
            if let Err(e) = root
                .get()
                .await
                .devices_changed(root.signal_emitter())
                .await
            {
                warn!(error = %e, "failed to emit Devices change");
            }
        }

        rx.changed().await.context("registry channel closed")?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    /// `device_props` must list exactly the properties the
    /// `#[zbus::interface]` macro generates for [`Device`] — a missing or
    /// misspelled entry silently stops that property's `PropertiesChanged`
    /// emission.
    #[test]
    fn device_props_matches_interface() {
        let snap = DeviceSnapshot {
            key: "k".to_string(),
            name: "dev".to_string(),
            connected: true,
            battery: None,
            last_updated: Some(SystemTime::now()),
        };
        let mut xml = String::new();
        Device {
            snapshot: snap.clone(),
        }
        .introspect_to_writer(&mut xml, 0);

        let props = device_props(&snap);
        assert_eq!(xml.matches("<property").count(), props.len());
        for (name, _) in props {
            assert!(
                xml.contains(&format!("name=\"{name}\"")),
                "device_props entry {name} is not a Device property"
            );
        }
    }
}
