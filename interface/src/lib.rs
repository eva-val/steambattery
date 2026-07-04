//! The `io.github.steambattery` D-Bus contract, shared by the daemon (which
//! serves it) and clients like the COSMIC applet (which consume it through
//! the generated proxies below).

use zbus::zvariant::OwnedObjectPath;

pub const BUS_NAME: &str = "io.github.steambattery";
pub const ROOT_PATH: &str = "/io/github/steambattery";

/// `BatteryLevel` value meaning "no battery report received yet".
pub const LEVEL_UNKNOWN: u8 = 0xff;

/// SDL3 `EChargeState`, carried on the wire as the `ChargeState: y` property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeState {
    Reset,
    Discharging,
    Charging,
    SrcValidate,
    ChargingDone,
    Unknown(u8),
}

impl From<u8> for ChargeState {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Reset,
            1 => Self::Discharging,
            2 => Self::Charging,
            3 => Self::SrcValidate,
            4 => Self::ChargingDone,
            other => Self::Unknown(other),
        }
    }
}

impl ChargeState {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Reset => 0,
            Self::Discharging => 1,
            Self::Charging => 2,
            Self::SrcValidate => 3,
            Self::ChargingDone => 4,
            Self::Unknown(v) => v,
        }
    }

    #[must_use]
    pub const fn is_charging(self) -> bool {
        matches!(self, Self::Charging | Self::SrcValidate)
    }

    /// Human-readable label. `Reset` doubles as "no report yet" on the wire,
    /// so both it and unknown codes read as "Unknown".
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Discharging => "Discharging",
            Self::Charging => "Charging",
            Self::SrcValidate => "Charger validating",
            Self::ChargingDone => "Fully charged",
            Self::Reset | Self::Unknown(_) => "Unknown",
        }
    }
}

/// Root object at [`ROOT_PATH`].
#[zbus::proxy(
    interface = "io.github.steambattery.Daemon",
    default_service = "io.github.steambattery",
    default_path = "/io/github/steambattery"
)]
pub trait Daemon {
    /// Object paths of currently-present controller devices.
    #[zbus(property)]
    fn devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    #[zbus(property)]
    fn version(&self) -> zbus::Result<String>;
}

/// One logical controller, at `{ROOT_PATH}/devN` (listed by
/// [`DaemonProxy::devices`]).
#[zbus::proxy(
    interface = "io.github.steambattery.Device",
    default_service = "io.github.steambattery"
)]
pub trait Device {
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;

    /// Controller traffic seen within the staleness window.
    #[zbus(property)]
    fn connected(&self) -> zbus::Result<bool>;

    /// SDL3 `EChargeState` — see [`ChargeState`]. 0 until the first report.
    #[zbus(property)]
    fn charge_state(&self) -> zbus::Result<u8>;

    /// 0..=100, or [`LEVEL_UNKNOWN`] when no battery report has arrived yet.
    #[zbus(property)]
    fn battery_level(&self) -> zbus::Result<u8>;

    /// mV
    #[zbus(property)]
    fn battery_voltage(&self) -> zbus::Result<u16>;

    /// mV
    #[zbus(property)]
    fn system_voltage(&self) -> zbus::Result<u16>;

    /// mV (~5100 while on USB power, 0 unplugged)
    #[zbus(property)]
    fn input_voltage(&self) -> zbus::Result<u16>;

    /// mA
    #[zbus(property)]
    fn current(&self) -> zbus::Result<u16>;

    /// mA
    #[zbus(property)]
    fn input_current(&self) -> zbus::Result<u16>;

    /// milli-°C
    #[zbus(property)]
    fn temperature(&self) -> zbus::Result<u16>;

    /// Unix seconds of the last *published* battery change; 0 = never.
    /// Telemetry is deadbanded daemon-side, so this only advances when a
    /// value moved meaningfully — while `Connected` is true the data is
    /// live even if this is minutes old. The daemon re-stamps it when
    /// `Connected` flips false, so for a disconnected device it is the
    /// moment the retained values were last known good.
    #[zbus(property)]
    fn last_updated(&self) -> zbus::Result<u64>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charge_state_roundtrip() {
        for v in 0..=5u8 {
            assert_eq!(ChargeState::from(v).as_u8(), v);
        }
        assert_eq!(ChargeState::from(4), ChargeState::ChargingDone);
        assert_eq!(ChargeState::from(9), ChargeState::Unknown(9));
        assert!(ChargeState::Charging.is_charging());
        assert!(ChargeState::SrcValidate.is_charging());
        assert!(!ChargeState::Discharging.is_charging());
        assert_eq!(ChargeState::ChargingDone.label(), "Fully charged");
        assert_eq!(ChargeState::Unknown(9).label(), "Unknown");
    }
}
