//! Wire format for Steam Controller 2 ("Triton") HID input reports.
//!
//! Layouts match SDL3's `src/joystick/hidapi/steam/controller_structs.h`
//! (`TritonBatteryStatus_t`, `ETritonReportIDTypes`) as cross-checked by
//! <https://github.com/CouchTurtle/sc2-research>. All multi-byte fields are
//! little-endian; `#pragma pack(1)` semantics (no padding).

pub use steambattery_interface::ChargeState;

pub const REPORT_CONTROLLER_STATE: u8 = 0x42;
pub const REPORT_BATTERY_STATUS: u8 = 0x43;
pub const REPORT_CONTROLLER_STATE_BLE: u8 = 0x45;

/// Total size of report 0x43 including the report-ID byte.
pub const BATTERY_REPORT_LEN: usize = 15;

/// SDL3 `TritonBatteryStatus_t` — the 14-byte payload of report 0x43.
///
/// Units are not documented by Valve but were calibrated against live
/// hardware (2026-07-03): voltages in mV (4134 = full Li-ion cell, 5.1 V USB
/// input while charging), currents in mA, temperature in milli-°C
/// (26424 = 26.4 °C at room temperature).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryStatus {
    pub charge_state: ChargeState,
    /// 0..=100
    pub level: u8,
    /// mV
    pub battery_voltage: u16,
    /// mV
    pub system_voltage: u16,
    /// mV (~5100 while on USB power, 0 unplugged)
    pub input_voltage: u16,
    /// mA
    pub current: u16,
    /// mA
    pub input_current: u16,
    /// milli-°C
    pub temperature: u16,
}

/// What a single hidraw report means to us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Report {
    Battery(BatteryStatus),
    /// Controller state traffic (0x42 wired/dongle, 0x45 BLE) — proof the
    /// controller is awake and connected.
    Liveness,
    Other(u8),
}

fn u16_le(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

/// Parse a report 0x43 buffer (including the leading ID byte).
pub fn parse_battery_report(buf: &[u8]) -> Option<BatteryStatus> {
    if buf.len() < BATTERY_REPORT_LEN || buf[0] != REPORT_BATTERY_STATUS {
        return None;
    }
    Some(BatteryStatus {
        charge_state: ChargeState::from(buf[1]),
        level: buf[2],
        battery_voltage: u16_le(buf, 3),
        system_voltage: u16_le(buf, 5),
        input_voltage: u16_le(buf, 7),
        current: u16_le(buf, 9),
        input_current: u16_le(buf, 11),
        temperature: u16_le(buf, 13),
    })
}

/// Classify one hidraw report (the kernel delivers exactly one per read).
pub fn classify(buf: &[u8]) -> Option<Report> {
    let id = *buf.first()?;
    match id {
        REPORT_BATTERY_STATUS => parse_battery_report(buf).map(Report::Battery),
        REPORT_CONTROLLER_STATE | REPORT_CONTROLLER_STATE_BLE => Some(Report::Liveness),
        other => Some(Report::Other(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_battery_report() -> [u8; 15] {
        let mut r = [0u8; 15];
        r[0] = 0x43;
        r[1] = 1; // discharging
        r[2] = 87; // level
        r[3..5].copy_from_slice(&3812u16.to_le_bytes()); // battery mV
        r[5..7].copy_from_slice(&3790u16.to_le_bytes()); // system mV
        r[7..9].copy_from_slice(&0u16.to_le_bytes()); // input mV (unplugged)
        r[9..11].copy_from_slice(&142u16.to_le_bytes()); // current
        r[11..13].copy_from_slice(&0u16.to_le_bytes()); // input current
        r[13..15].copy_from_slice(&2950u16.to_le_bytes()); // temperature
        r
    }

    #[test]
    fn parses_battery_report() {
        let b = parse_battery_report(&sample_battery_report()).unwrap();
        assert_eq!(b.charge_state, ChargeState::Discharging);
        assert_eq!(b.level, 87);
        assert_eq!(b.battery_voltage, 3812);
        assert_eq!(b.system_voltage, 3790);
        assert_eq!(b.input_voltage, 0);
        assert_eq!(b.current, 142);
        assert_eq!(b.input_current, 0);
        assert_eq!(b.temperature, 2950);
    }

    #[test]
    fn rejects_wrong_id_and_short_reports() {
        let mut r = sample_battery_report();
        r[0] = 0x42;
        assert_eq!(parse_battery_report(&r), None);
        assert_eq!(parse_battery_report(&r[..14]), None);
        assert_eq!(parse_battery_report(&[]), None);
    }

    #[test]
    fn classifies_reports() {
        assert!(matches!(
            classify(&sample_battery_report()),
            Some(Report::Battery(_))
        ));
        assert_eq!(classify(&[0x42; 54]), Some(Report::Liveness));
        assert_eq!(classify(&[0x45; 46]), Some(Report::Liveness));
        assert_eq!(classify(&[0x7b; 13]), Some(Report::Other(0x7b)));
        assert_eq!(classify(&[]), None);
    }
}
