//! Central registry of controller state, shared between the hidraw readers
//! and the D-Bus layer. Publishes full snapshots over a `watch` channel so
//! consumers can diff at their own pace.

// Mutations intentionally hold the lock through `publish` so every snapshot
// reflects a consistent view of the map.
#![allow(clippy::significant_drop_tightening)]

use std::collections::{btree_map, BTreeMap};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{watch, Notify};

use crate::protocol::BatteryStatus;

/// How long without controller traffic before we consider it asleep/off.
/// Battery reports arrive ~every 2.5 s and state reports at ~266 Hz, so 10 s
/// of silence means the controller is gone (the puck emits no explicit
/// disconnect event).
pub const STALE_AFTER: Duration = Duration::from_secs(10);

/// How long after resume a device may stay silent before being swept. Must
/// cover the reader's re-mark latency (1 s mark throttle + 200 ms drain
/// sleep) so a controller that survived suspend never flaps; a controller
/// that died during sleep is reported disconnected this soon after resume
/// instead of `STALE_AFTER` later (the monotonic clock freezes in suspend,
/// so its `last_seen` still looks fresh).
const RESUME_GRACE: Duration = Duration::from_secs(2);

/// Publish deadbands for the analog telemetry fields, applied against the
/// last *published* value — publishing re-anchors the reference, which gives
/// inherent hysteresis (no flapping at a threshold) while slow drift still
/// accumulates to a publish. Raw reports jitter by a few LSB; without a
/// deadband every 2.5 s battery report emits PropertiesChanged and wakes the
/// applet. `level` and `charge_state` always publish exactly.
const DEADBAND_MV: u16 = 15;
const DEADBAND_MA: u16 = 10;
const DEADBAND_MC: u16 = 300;

/// Whether `new` differs enough from the last published `old` to be worth
/// waking every D-Bus client on the machine.
fn significant(old: &BatteryStatus, new: &BatteryStatus) -> bool {
    old.charge_state != new.charge_state
        || old.level != new.level
        || old.battery_voltage.abs_diff(new.battery_voltage) >= DEADBAND_MV
        || old.system_voltage.abs_diff(new.system_voltage) >= DEADBAND_MV
        || old.input_voltage.abs_diff(new.input_voltage) >= DEADBAND_MV
        || old.current.abs_diff(new.current) >= DEADBAND_MA
        || old.input_current.abs_diff(new.input_current) >= DEADBAND_MA
        || old.temperature.abs_diff(new.temperature) >= DEADBAND_MC
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSnapshot {
    /// Stable identity, e.g. `<usb syspath>/slot0`.
    pub key: String,
    /// Human label, e.g. "Steam Controller (puck slot 0)".
    pub name: String,
    /// Controller traffic seen within `STALE_AFTER`.
    pub connected: bool,
    /// Last *published* battery report (deadbanded — see [`significant`]);
    /// retained (stale) while disconnected.
    pub battery: Option<BatteryStatus>,
    /// Wall-clock time of the last published battery change.
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
    /// Pings the sweeper when the next staleness deadline moves *earlier*
    /// (a device newly connected, or a resume rewound `last_seen`);
    /// deadlines moving later just make the sweeper's wake a no-op.
    sweep_notify: Notify,
}

impl Registry {
    pub fn new() -> Self {
        let (tx, _) = watch::channel(Vec::new());
        Self {
            inner: Mutex::new(BTreeMap::new()),
            tx,
            sweep_notify: Notify::new(),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<Vec<DeviceSnapshot>> {
        self.tx.subscribe()
    }

    /// Resolves when the sweeper should recompute its deadline. Uses
    /// `notify_one` permit semantics, so a ping sent while the sweeper is
    /// busy completes its next await instead of being lost.
    pub async fn sweep_notified(&self) {
        self.sweep_notify.notified().await;
    }

    /// Earliest `last_seen + STALE_AFTER` over connected devices; None when
    /// nothing is connected (the sweeper can park without any timer).
    pub fn next_deadline(&self) -> Option<Instant> {
        let inner = self.inner.lock().unwrap();
        inner
            .values()
            .filter(|e| e.snapshot.connected)
            .map(|e| e.last_seen.map_or_else(Instant::now, |t| t + STALE_AFTER))
            .min()
    }

    /// The monotonic clock does not advance during suspend, so on resume
    /// every connected device still looks fresh even if it died hours ago.
    /// Rewind `last_seen` so silence is noticed `RESUME_GRACE` from now: a
    /// surviving controller re-marks well within that (≤ 1.2 s) and never
    /// flaps, a dead one is swept promptly instead of `STALE_AFTER` later.
    pub fn note_resume(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(cutoff) = Instant::now().checked_sub(STALE_AFTER - RESUME_GRACE) {
            for entry in inner.values_mut() {
                if entry.snapshot.connected {
                    entry.last_seen = Some(entry.last_seen.map_or(cutoff, |t| t.min(cutoff)));
                }
            }
        }
        drop(inner);
        self.sweep_notify.notify_one();
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

    /// Called on controller traffic (throttled by the reader) — must stay
    /// cheap. Only publishes when the connected flag actually flips.
    pub fn mark_traffic(&self, key: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = inner.get_mut(key) {
            entry.last_seen = Some(Instant::now());
            if !entry.snapshot.connected {
                entry.snapshot.connected = true;
                self.publish(&inner);
                self.sweep_notify.notify_one();
            }
        }
    }

    /// Record a battery report. Always refreshes liveness, but the snapshot
    /// (and thus the log task, D-Bus diff, and every client) only sees it
    /// when something [`significant`] changed — jitter-only reports leave
    /// the published state untouched.
    pub fn update_battery(&self, key: &str, battery: BatteryStatus) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = inner.get_mut(key) {
            entry.last_seen = Some(Instant::now());
            let newly_connected = !entry.snapshot.connected;
            let publish = newly_connected
                || entry
                    .snapshot
                    .battery
                    .as_ref()
                    .is_none_or(|old| significant(old, &battery));
            if !publish {
                return;
            }
            entry.snapshot.connected = true;
            entry.snapshot.battery = Some(battery);
            entry.snapshot.last_updated = Some(SystemTime::now());
            self.publish(&inner);
            if newly_connected {
                self.sweep_notify.notify_one();
            }
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

    /// Sub-deadband jitter must not publish; liveness still refreshes.
    #[test]
    fn deadband_suppresses_jitter() {
        let r = Registry::new();
        let mut rx = r.subscribe();
        r.acquire("k", "dev");
        r.update_battery("k", battery(80));
        rx.borrow_and_update();

        let mut jitter = battery(80);
        jitter.battery_voltage += DEADBAND_MV - 1;
        jitter.current -= DEADBAND_MA - 1;
        jitter.temperature += DEADBAND_MC - 1;
        r.update_battery("k", jitter);
        assert!(!rx.has_changed().unwrap());
        // Published snapshot still carries the original values.
        assert_eq!(rx.borrow()[0].battery.unwrap().battery_voltage, 3800);
        // ...but the report refreshed liveness.
        assert!(r.inner.lock().unwrap()["k"].last_seen.is_some());
    }

    /// The deadband anchor is the last *published* value, so sub-threshold
    /// drift accumulates until it crosses the band, then re-anchors.
    #[test]
    fn deadband_reanchors_on_publish() {
        let r = Registry::new();
        let mut rx = r.subscribe();
        r.acquire("k", "dev");
        r.update_battery("k", battery(80));
        rx.borrow_and_update();

        let step = |mv: u16| {
            let mut b = battery(80);
            b.battery_voltage = mv;
            b
        };
        r.update_battery("k", step(3800 + 8));
        assert!(!rx.has_changed().unwrap());
        // +16 from the 3800 anchor: crosses the band even though each step
        // was only 8 mV.
        r.update_battery("k", step(3800 + 16));
        assert!(rx.has_changed().unwrap());
        assert_eq!(
            rx.borrow_and_update()[0].battery.unwrap().battery_voltage,
            3816
        );
        // The anchor moved to 3816: 8 mV away is jitter again.
        r.update_battery("k", step(3816 + 8));
        assert!(!rx.has_changed().unwrap());
    }

    /// `level` and `charge_state` publish on any change, no deadband.
    #[test]
    fn level_and_charge_state_always_publish() {
        let r = Registry::new();
        let mut rx = r.subscribe();
        r.acquire("k", "dev");
        r.update_battery("k", battery(80));
        rx.borrow_and_update();

        r.update_battery("k", battery(79));
        assert!(rx.has_changed().unwrap());
        rx.borrow_and_update();

        let mut b = battery(79);
        b.charge_state = ChargeState::Charging;
        r.update_battery("k", b);
        assert!(rx.has_changed().unwrap());
    }

    /// No connected devices → no deadline → the sweeper parks timer-free.
    #[test]
    fn next_deadline_none_when_nothing_connected() {
        let r = Registry::new();
        assert!(r.next_deadline().is_none());
        r.acquire("k", "dev");
        assert!(r.next_deadline().is_none());
        r.mark_traffic("k");
        assert!(r.next_deadline().is_some());
        r.inner.lock().unwrap().get_mut("k").unwrap().last_seen = None;
        r.sweep();
        assert!(r.next_deadline().is_none());
    }

    /// After a resume the staleness deadline lands `RESUME_GRACE` from now,
    /// not `STALE_AFTER` after a pre-suspend `last_seen`.
    #[test]
    fn note_resume_rewinds_deadline() {
        let r = Registry::new();
        r.acquire("k", "dev");
        r.mark_traffic("k");
        let before = r.next_deadline().unwrap();
        r.note_resume();
        let after = r.next_deadline().unwrap();
        assert!(after < before);
        let now = Instant::now();
        assert!(after <= now + RESUME_GRACE);
        // Fresh traffic after resume restores the full window.
        r.mark_traffic("k");
        assert!(r.next_deadline().unwrap() > now + RESUME_GRACE);
    }
}
