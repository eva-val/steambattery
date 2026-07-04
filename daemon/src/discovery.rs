//! Discovery of Steam Controller 2 hidraw nodes via udev, plus hotplug
//! monitoring.
//!
//! Topology (from sc2-research):
//! - Puck dongle `28de:1304`: USB interfaces 2..=5 are controller slots 0..=3,
//!   interface 6 is a dongle status channel (silent; skipped).
//! - Wired controller `28de:1302` / BLE `28de:1303`: all hidraw nodes of one
//!   physical device are treated as a single logical controller.

use std::io;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{debug, warn};

const VALVE_VENDOR: u32 = 0x28de;
const PID_CONTROLLER_USB: u32 = 0x1302;
const PID_CONTROLLER_BLE: u32 = 0x1303;
const PID_PUCK: u32 = 0x1304;

#[derive(Debug, Clone)]
pub struct HidrawNode {
    pub devnode: PathBuf,
    /// Stable logical-device key (shared by nodes that belong to the same
    /// controller), e.g. `<usb syspath>/slot0`.
    pub key: String,
    /// Human-readable label for the logical device.
    pub name: String,
}

#[derive(Debug)]
pub enum Event {
    Added(HidrawNode),
    Removed(PathBuf),
}

/// Syspath of the USB device ancestor (absent for Bluetooth transports).
fn usb_syspath(device: &udev::Device) -> Option<String> {
    let usb_dev = device
        .parent_with_subsystem_devtype("usb", "usb_device")
        .ok()??;
    Some(usb_dev.syspath().to_string_lossy().into_owned())
}

/// Classify a udev hidraw device; returns None for non-SC2 devices and for
/// the puck's status/dongle interface.
///
/// Identity comes from the hid parent's `HID_ID` (`<bus>:<vid>:<pid>`, hex),
/// which is present on every transport — a Bluetooth controller has no USB
/// ancestor to read `idVendor`/`idProduct` from.
fn classify(device: &udev::Device) -> Option<HidrawNode> {
    let devnode = device.devnode()?.to_path_buf();
    let hid_dev = device.parent_with_subsystem("hid").ok()??;
    let hid_id = hid_dev.property_value("HID_ID")?.to_str()?;
    let (_bus, rest) = hid_id.split_once(':')?;
    let (vendor, product) = rest.split_once(':')?;
    if u32::from_str_radix(vendor, 16).ok()? != VALVE_VENDOR {
        return None;
    }

    match u32::from_str_radix(product, 16).ok()? {
        PID_PUCK => {
            let iface = device
                .parent_with_subsystem_devtype("usb", "usb_interface")
                .ok()??
                .attribute_value("bInterfaceNumber")?
                .to_str()?
                .parse::<u8>()
                .ok()?;
            // Interfaces 2..=5 are controller slots 0..=3; 6 is dongle status.
            let slot = iface.checked_sub(2)?;
            if slot > 3 {
                return None;
            }
            Some(HidrawNode {
                devnode,
                key: format!("{}/slot{slot}", usb_syspath(device)?),
                name: format!("Steam Controller (puck slot {slot})"),
            })
        }
        PID_CONTROLLER_USB => Some(HidrawNode {
            devnode,
            key: usb_syspath(device)?,
            name: "Steam Controller (USB)".to_string(),
        }),
        PID_CONTROLLER_BLE => {
            // HID_UNIQ is the controller's BT address — stable across
            // reconnects, unlike the hid syspath (instance-numbered).
            let key = hid_dev
                .property_value("HID_UNIQ")
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .map_or_else(
                    || hid_dev.syspath().to_string_lossy().into_owned(),
                    |uniq| format!("ble/{uniq}"),
                );
            Some(HidrawNode {
                devnode,
                key,
                name: "Steam Controller (Bluetooth)".to_string(),
            })
        }
        _ => None,
    }
}

/// Enumerate currently-present SC2 hidraw nodes.
pub fn scan() -> Result<Vec<HidrawNode>> {
    let mut enumerator = udev::Enumerator::new().context("creating udev enumerator")?;
    enumerator
        .match_subsystem("hidraw")
        .context("matching hidraw subsystem")?;
    let nodes = enumerator
        .scan_devices()
        .context("scanning udev devices")?
        .filter_map(|d| classify(&d))
        .collect();
    Ok(nodes)
}

/// Forward hidraw add/remove events to `tx` forever. Blocking — udev types
/// are !Send, so this runs on a dedicated thread; only plain `Event` values
/// (paths/strings) cross into the async world.
// The monitor thread owns the sender for its whole life; by-value is the
// honest signature.
#[allow(clippy::needless_pass_by_value)]
pub fn monitor_blocking(tx: mpsc::Sender<Event>) -> Result<()> {
    let socket = udev::MonitorBuilder::new()
        .context("creating udev monitor")?
        .match_subsystem("hidraw")
        .context("matching hidraw subsystem")?
        .listen()
        .context("listening on udev monitor")?;
    let fd = socket.as_raw_fd();

    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let r = unsafe { libc::poll(&raw mut pfd, 1, -1) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e).context("polling udev monitor");
        }
        for event in socket.iter() {
            let out = match event.event_type() {
                // Treat `Change` like an add so a node that failed to open
                // can recover on the event (e.g. a `udevadm trigger` or an
                // attribute rewrite). Not every recovery emits one, though:
                // logind applies seat ACLs directly via syscalls with no
                // uevent, which is why the supervisor also keeps a slow
                // retry (see main.rs) instead of relying on events alone.
                udev::EventType::Add | udev::EventType::Change => {
                    classify(&event.device()).map(Event::Added)
                }
                udev::EventType::Remove => event
                    .device()
                    .devnode()
                    .map(|d| Event::Removed(d.to_path_buf())),
                _ => None,
            };
            if let Some(out) = out {
                debug!(?out, "udev event");
                if tx.blocking_send(out).is_err() {
                    warn!("event receiver dropped, stopping udev monitor");
                    return Ok(());
                }
            }
        }
    }
}
