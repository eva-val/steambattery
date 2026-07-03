# NixOS module: udev access to SC2 hidraw nodes + user service for the daemon.
# Imported by the flake as `nixosModules.default` with `self` applied.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.hardware.steambattery;
  packages = self.packages.${pkgs.stdenv.hostPlatform.system};
in
{
  options.hardware.steambattery = {
    enable = lib.mkEnableOption "Steam Controller 2 battery monitor";
  };

  config = lib.mkIf cfg.enable {
    # Grant access to Valve SC2-family hidraw nodes (1302 wired, 1303 BLE,
    # 1304 puck, 1305 Nereid dongle). Bluetooth controllers have no USB
    # ancestor, so match their hid parent's kernel name
    # (`<bus 0005>:<vid>:<pid>.<instance>`) instead.
    #
    # We grant via group+mode AND uaccess. uaccess is the modern seat-based
    # approach, but it fails where logind never attaches the hidraw node to a
    # seat — e.g. an Asahi/Apple-silicon host whose USB controller isn't a
    # master-of-seat, so USB hidraw nodes are never bound to seat0 and the
    # uaccess ACL is never applied. The `users` group + 0660 grant doesn't
    # depend on seat attachment and covers that case; uaccess remains as a
    # harmless extra for hosts where it does work.
    services.udev.extraRules = ''
      KERNEL=="hidraw*", SUBSYSTEMS=="usb", ATTRS{idVendor}=="28de", ATTRS{idProduct}=="130[2-5]", MODE="0660", GROUP="users", TAG+="uaccess"
      KERNEL=="hidraw*", KERNELS=="0005:28DE:130[2-5].*", MODE="0660", GROUP="users", TAG+="uaccess"
    '';

    # Applet binary + desktop entry, so cosmic-panel can list it under
    # Settings → Desktop → Panel → Configure panel applets.
    environment.systemPackages = [ packages.cosmic-applet-steambattery ];

    systemd.user.services.steambatteryd = {
      description = "Steam Controller 2 battery telemetry daemon";
      wantedBy = [ "default.target" ];
      # The daemon exits on session-bus loss by design; never give up
      # restarting it (the default 5-starts-in-10s limit would otherwise
      # permanently fail the unit after ~10s of fast failures).
      unitConfig.StartLimitIntervalSec = 0;
      serviceConfig = {
        ExecStart = "${packages.steambatteryd}/bin/steambatteryd";
        Restart = "on-failure";
        RestartSec = 2;
      };
    };
  };
}
