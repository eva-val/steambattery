# steambattery

Battery monitoring for the **Steam Controller 2** ("Triton") on Linux: a
daemon that reads battery telemetry straight from the controller's HID
reports, and a COSMIC panel applet that displays it.ck

Supports all 3 connection methods

The kernel's `hid-steam` driver doesn't (yet) support the SC2 family, so
nothing shows up in UPower — this fills the gap in userspace.

- `daemon/` — **steambatteryd**: discovers SC2 devices (wired `28de:1302`,
  Bluetooth `28de:1303`, puck dongle `28de:1304`) via udev, reads battery
  reports from hidraw, and publishes state on the session D-Bus.
- `applet/` — **cosmic-applet-steambattery**: panel icon + percentage, with a
  popup showing charge state, voltages, currents, and temperature.
- `interface/` — **steambattery-interface**: the shared D-Bus contract
  (constants, `ChargeState`, and `zbus` client proxies for third parties).

## Protocol

The controller pushes input report `0x43` (`TritonBatteryStatus_t`, from
SDL3's [`controller_structs.h`]) roughly every 2.5 s while awake — no query
needed. Units were calibrated against live hardware: voltages in mV, currents
in mA, temperature in milli-°C. The `0x42` state stream (~266 Hz) doubles as
a liveness signal; 10 s of silence marks the controller asleep/disconnected.

Protocol research: [CouchTurtle/sc2-research] and the SDL3 Triton driver.

[`controller_structs.h`]: https://github.com/libsdl-org/SDL/blob/main/src/joystick/hidapi/steam/controller_structs.h
[CouchTurtle/sc2-research]: https://github.com/CouchTurtle/sc2-research

## Install (NixOS)

```nix
# flake inputs
inputs.steambattery.url = "github:eva-val/steambattery";

# NixOS configuration
imports = [ inputs.steambattery.nixosModules.default ];
hardware.steambattery.enable = true;
```

The module installs:

- a udev rule granting the active seat access to SC2 hidraw nodes (`uaccess`),
- a systemd **user** service running `steambatteryd`,
- the applet package with its desktop entry.

Then add the applet: **COSMIC Settings → Desktop → Panel → Configure panel
applets → Add applet → "Steam Controller Battery"**.

## D-Bus interface

Session bus name `io.github.steambattery`.

- `/io/github/steambattery` — `io.github.steambattery.Daemon`
  - `Devices: ao` — device object paths
  - `Version: s`
- `/io/github/steambattery/devN` — `io.github.steambattery.Device`
  - `Name: s`, `Connected: b`
  - `ChargeState: y` — SDL3 `EChargeState`: 0 reset, 1 discharging,
    2 charging, 3 charger-validate, 4 done (2 and 3 mean "charging")
  - `BatteryLevel: y` — 0–100, 255 = no report yet
  - `BatteryVoltage`, `SystemVoltage`, `InputVoltage: q` — mV
  - `Current`, `InputCurrent: q` — mA
  - `Temperature: q` — milli-°C
  - `LastUpdated: t` — unix seconds, 0 = never

All properties emit `PropertiesChanged`. Quick check:

```console
$ busctl --user get-property io.github.steambattery \
    /io/github/steambattery/dev0 io.github.steambattery.Device BatteryLevel
y 100
```

## Development

```console
$ direnv allow        # or: nix develop
$ cargo test
$ cargo run -p steambatteryd                   # needs hidraw access (udev rule)
$ cargo run -p cosmic-applet-steambattery      # runs standalone as a window
```

## License

GPL-3.0-or-later.
