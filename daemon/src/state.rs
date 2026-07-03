//! Central registry of controller state, shared between the hidraw readers
//! and the D-Bus layer. Publishes full snapshots over a `watch` channel so
//! consumers can diff at their own pace.

// Mutations intentionally hold the lock through `publish` so every snapshot
// reflects a consistent view of the map.
#![allow(clippy::significant_drop_tightening)]

use std::collections::{btree_map, BTreeMap};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::watch;

use crate::protocol::BatteryStatus;

/// How long without controller traffic before we consider it asleep/off.
/// Battery reports arrive ~every 2.5 s and state reports at ~266 Hz, so 10 s
/// of silence means the controller is gone (the puck emits no explicit
/// disconnect event).
pub const STALE_AFTER: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSnapshot {
    /// Stable identity, e.g. `<usb syspath>/slot0`.
    pub key: String,
    /// Human label, e.g. "Steam Controller (puck slot 0)".
    pub name: String,
    /// Controller traffic seen within `STALE_AFTER`.
    pub connected: bool,
    /// Last parsed battery report; retained (stale) while disconnected.
    pub battery: Option<BatteryStatus>,
    /// Wall-clock time of the last battery report.
    pub last_updated: Option<SystemTime>,
}

struct Entry {
    snapshot: DeviceSnapshot,
    last_seen: Option<Instant>,
    /// Number of hidraw readers feeding this entry (a wired controller may
    /// expose several HID interfaces that share one logical device).
    refs: usize,
}

pub struct Registry {
    inner: Mutex<BTreeMap<String, Entry>>,
    tx: watch::Sender<Vec<DeviceSnapshot>>,
}

impl Registry {
    pub fn new() -> Self {
        let (tx, _) = watch::channel(Vec::new());
        Self {
            inner: Mutex::new(BTreeMap::new()),
            tx,
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<Vec<DeviceSnapshot>> {
        self.tx.subscribe()
    }

    /// Register a reader for `key`, creating the device entry if new.
    pub fn acquire(&self, key: &str, name: &str) {
        let mut inner = self.inner.lock().unwrap();
        let inserted = match inner.entry(key.to_string()) {
            // Nothing visible changes on a ref bump.
            btree_map::Entry::Occupied(mut o) => {
                o.get_mut().refs += 1;
                false
            }
            btree_map::Entry::Vacant(v) => {
                v.insert(Entry {
                    snapshot: DeviceSnapshot {
                        key: key.to_string(),
                        name: name.to_string(),
                        connected: false,
                        battery: None,
                        last_updated: None,
                    },
                    last_seen: None,
                    refs: 1,
                });
                true
            }
        };
        if inserted {
            self.publish(&inner);
        }
    }

    /// Drop a reader for `key`; removes the device once no readers remain.
    pub fn release(&self, key: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = inner.get_mut(key) {
            entry.refs = entry.refs.saturating_sub(1);
            if entry.refs == 0 {
                inner.remove(key);
                self.publish(&inner);
            }
        }
    }

    /// Called on every controller-state report (~266 Hz) — must stay cheap.
    /// Only publishes when the connected flag actually flips.
    pub fn mark_traffic(&self, key: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = inner.get_mut(key) {
            entry.last_seen = Some(Instant::now());
            if !entry.snapshot.connected {
                entry.snapshot.connected = true;
                self.publish(&inner);
            }
        }
    }

    pub fn update_battery(&self, key: &str, battery: BatteryStatus) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = inner.get_mut(key) {
            entry.last_seen = Some(Instant::now());
            entry.snapshot.connected = true;
            entry.snapshot.battery = Some(battery);
            entry.snapshot.last_updated = Some(SystemTime::now());
            self.publish(&inner);
        }
    }

    /// Flip `connected` off for entries silent longer than `STALE_AFTER`.
    pub fn sweep(&self) {
        let mut inner = self.inner.lock().unwrap();
        let mut changed = false;
        for entry in inner.values_mut() {
            if entry.snapshot.connected && entry.last_seen.is_none_or(|t| t.elapsed() > STALE_AFTER)
            {
                entry.snapshot.connected = false;
                changed = true;
            }
        }
        if changed {
            self.publish(&inner);
        }
    }

    fn publish(&self, inner: &BTreeMap<String, Entry>) {
        let snapshots: Vec<DeviceSnapshot> = inner.values().map(|e| e.snapshot.clone()).collect();
        self.tx.send_if_modified(|current| {
            if *current == snapshots {
                false
            } else {
                *current = snapshots;
                true
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{BatteryStatus, ChargeState};

    fn battery(level: u8) -> BatteryStatus {
        BatteryStatus {
            charge_state: ChargeState::Discharging,
            level,
            battery_voltage: 3800,
            system_voltage: 3780,
            input_voltage: 0,
            current: 100,
            input_current: 0,
            temperature: 2900,
        }
    }

    #[test]
    fn acquire_release_lifecycle() {
        let r = Registry::new();
        let rx = r.subscribe();
        r.acquire("k", "dev");
        r.acquire("k", "dev");
        assert_eq!(rx.borrow().len(), 1);
        r.release("k");
        assert_eq!(rx.borrow().len(), 1);
        r.release("k");
        assert_eq!(rx.borrow().len(), 0);
    }

    #[test]
    fn battery_update_marks_connected_and_survives_sweep_retention() {
        let r = Registry::new();
        let rx = r.subscribe();
        r.acquire("k", "dev");
        r.update_battery("k", battery(80));
        {
            let snap = rx.borrow();
            assert!(snap[0].connected);
            assert_eq!(snap[0].battery.unwrap().level, 80);
        }
        // Sweep with fresh traffic: stays connected.
        r.sweep();
        assert!(rx.borrow()[0].connected);
    }

    #[test]
    fn sweep_disconnects_never_seen_entries() {
        let r = Registry::new();
        let rx = r.subscribe();
        r.acquire("k", "dev");
        r.mark_traffic("k");
        assert!(rx.borrow()[0].connected);
        // Fake staleness by clearing last_seen.
        r.inner.lock().unwrap().get_mut("k").unwrap().last_seen = None;
        r.sweep();
        let snap = rx.borrow();
        assert!(!snap[0].connected);
    }
}
