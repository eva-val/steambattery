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

const VALVE_VENDOR: &str = "28de";
const PID_CONTROLLER_USB: &str = "1302";
const PID_CONTROLLER_BLE: &str = "1303";
const PID_PUCK: &str = "1304";

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

/// Classify a udev hidraw device; returns None for non-SC2 devices and for
/// the puck's status/dongle interface.
fn classify(device: &udev::Device) -> Option<HidrawNode> {
    let devnode = device.devnode()?.to_path_buf();
    let usb_dev = device
        .parent_with_subsystem_devtype("usb", "usb_device")
        .ok()??;
    let vendor = usb_dev
        .attribute_value("idVendor")?
        .to_str()?
        .to_lowercase();
    if vendor != VALVE_VENDOR {
        return None;
    }
    let product = usb_dev
        .attribute_value("idProduct")?
        .to_str()?
        .to_lowercase();
    let usb_syspath = usb_dev.syspath().to_string_lossy().into_owned();

    match product.as_str() {
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
                key: format!("{usb_syspath}/slot{slot}"),
                name: format!("Steam Controller (puck slot {slot})"),
            })
        }
        PID_CONTROLLER_USB => Some(HidrawNode {
            devnode,
            key: usb_syspath,
            name: "Steam Controller (USB)".to_string(),
        }),
        PID_CONTROLLER_BLE => Some(HidrawNode {
            devnode,
            key: usb_syspath,
            name: "Steam Controller (Bluetooth)".to_string(),
        }),
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
                udev::EventType::Add => classify(&event.device()).map(Event::Added),
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
