{
  description = "Steam Controller 2 battery monitor — daemon + COSMIC applet";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
      ];
      pkgsFor = system: nixpkgs.legacyPackages.${system};

      # Native deps for libcosmic (applet) and libudev (daemon).
      runtimeLibs =
        pkgs: with pkgs; [
          libxkbcommon
          wayland
          libGL
          vulkan-loader
          fontconfig
          freetype
        ];
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          common = {
            src = self;
            useFetchCargoVendor = true;
            # Covers the whole workspace incl. git deps; update when
            # Cargo.lock changes (`nix build` prints the expected hash).
            cargoHash = "sha256-uWT2rs0JKa/FqdZ4JyFVRNsHUnvV/yQbO1n60uLwECY=";
            nativeBuildInputs = with pkgs; [ pkg-config ];
            buildInputs = [ pkgs.udev ] ++ runtimeLibs pkgs;
          };
        in
        rec {
          steambatteryd = pkgs.rustPlatform.buildRustPackage (
            common
            // {
              pname = "steambatteryd";
              version = "0.1.0";
              cargoBuildFlags = [
                "-p"
                "steambatteryd"
              ];
              cargoTestFlags = [
                "-p"
                "steambatteryd"
              ];
            }
          );

          cosmic-applet-steambattery = pkgs.rustPlatform.buildRustPackage (
            common
            // {
              pname = "cosmic-applet-steambattery";
              version = "0.1.0";
              cargoBuildFlags = [
                "-p"
                "cosmic-applet-steambattery"
              ];
              cargoTestFlags = [
                "-p"
                "cosmic-applet-steambattery"
              ];
              postInstall = ''
                install -Dm644 applet/data/io.github.steambattery.Applet.desktop \
                  $out/share/applications/io.github.steambattery.Applet.desktop
              '';
              # The applet loads wayland/vulkan/xkbcommon at runtime.
              postFixup = ''
                patchelf --add-rpath ${pkgs.lib.makeLibraryPath (runtimeLibs pkgs)} \
                  $out/bin/cosmic-applet-steambattery
              '';
            }
          );

          default = steambatteryd;
        }
      );

      formatter = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        pkgs.writeShellApplication {
          name = "steambattery-fmt";
          runtimeInputs = with pkgs; [
            nixfmt
            cargo
            rustfmt
            findutils
          ];
          text = ''
            find . -name '*.nix' -not -path './target/*' -exec nixfmt {} +
            cargo fmt --all
          '';
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.mkShell {
            nativeBuildInputs = with pkgs; [ pkg-config ];
            buildInputs = [ pkgs.udev ] ++ runtimeLibs pkgs;
            # winit/wgpu dlopen these at runtime.
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (runtimeLibs pkgs);
          };
        }
      );

      nixosModules.default = import ./nix/module.nix self;
    };
}
