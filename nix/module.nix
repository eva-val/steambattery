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
    # Grant the active local seat access to Valve SC2-family hidraw nodes
    # (1302 wired, 1303 BLE, 1304 puck, 1305 Nereid dongle).
    services.udev.extraRules = ''
      KERNEL=="hidraw*", SUBSYSTEMS=="usb", ATTRS{idVendor}=="28de", ATTRS{idProduct}=="130[2-5]", TAG+="uaccess"
    '';

    systemd.user.services.steambatteryd = {
      description = "Steam Controller 2 battery telemetry daemon";
      wantedBy = [ "default.target" ];
      serviceConfig = {
        ExecStart = "${packages.steambatteryd}/bin/steambatteryd";
        Restart = "on-failure";
        RestartSec = 2;
      };
    };
  };
}
