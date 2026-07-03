{
  description = "Steam Controller 2 battery monitor — daemon + COSMIC applet";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ];
      pkgsFor = system: nixpkgs.legacyPackages.${system};

      # Native deps for libcosmic (applet) and libudev (daemon).
      runtimeLibs = pkgs: with pkgs; [
        libxkbcommon
        wayland
        libGL
        vulkan-loader
        fontconfig
        freetype
      ];
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = pkgsFor system;
          common = {
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = with pkgs; [ pkg-config ];
            buildInputs = [ pkgs.udev ] ++ runtimeLibs pkgs;
          };
        in
        rec {
          steambatteryd = pkgs.rustPlatform.buildRustPackage (common // {
            pname = "steambatteryd";
            version = "0.1.0";
            cargoBuildFlags = [ "-p" "steambatteryd" ];
            cargoTestFlags = [ "-p" "steambatteryd" ];
          });
          default = steambatteryd;
        });

      devShells = forAllSystems (system:
        let pkgs = pkgsFor system;
        in {
          default = pkgs.mkShell {
            nativeBuildInputs = with pkgs; [ pkg-config ];
            buildInputs = [ pkgs.udev ] ++ runtimeLibs pkgs;
            # winit/wgpu dlopen these at runtime.
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (runtimeLibs pkgs);
          };
        });

      nixosModules.default = import ./nix/module.nix self;
    };
}
