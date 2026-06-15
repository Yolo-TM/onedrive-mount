{
  description = "onedrive-mount — rclone mount manager with egui GUI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    let
      # NixOS module — add to any machine's imports
      nixosModule = { config, lib, pkgs, ... }:
        let
          cfg = config.services.onedrive-mount;
          pkg = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        in {
          options.services.onedrive-mount = {
            enable = lib.mkEnableOption "onedrive-mount daemon";
          };

          config = lib.mkIf cfg.enable {
            environment.systemPackages = [
              pkg
              pkgs.rclone
              pkgs.fuse3
            ];
          };
        };

    in flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rustToolchain = pkgs.rust-bin.stable.latest.default;

        # Libraries that eframe/winit dlopen at runtime on Linux (X11 backend)
        runtimeLibs = with pkgs; [
          libx11
          libxcursor
          libxi
          libxrandr
          libxcb
          libGL
          libxkbcommon
        ];

        runtimeLibPath = pkgs.lib.makeLibraryPath runtimeLibs;

        buildInputs = runtimeLibs;

        nativeBuildInputs = with pkgs; [
          rustToolchain
          pkg-config
          autoPatchelfHook
        ];

        package = pkgs.rustPlatform.buildRustPackage {
          pname = "onedrive-mount";
          version = "0.2.0";

          # Exclude target/ and other non-source dirs so the store hash is
          # stable and doesn't differ between machines with dirty trees.
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: type:
              let rel = pkgs.lib.removePrefix (toString ./. + "/") path;
              in !(pkgs.lib.hasPrefix "target/" rel)
              && !(pkgs.lib.hasPrefix ".git/" rel);
          };

          cargoLock.lockFile = ./Cargo.lock;

          inherit buildInputs nativeBuildInputs;

          # Build both binaries; features joined as a single string is what
          # rustPlatform.buildRustPackage expects for CARGO_BUILD_FEATURES.
          cargoBuildFlags = [ "--bins" "--features" "daemon,gui" ];

          postInstall = ''
            mkdir -p $out/share/icons/hicolor/scalable/apps
            install -m644 assets/icon.svg \
              $out/share/icons/hicolor/scalable/apps/onedrive-mount.svg

            mkdir -p $out/share/applications
            install -m644 assets/onedrive-mount.desktop \
              $out/share/applications/onedrive-mount.desktop
          '';

          # autoPatchelfHook rewrites ELF RPATHs using the buildInputs above,
          # producing store-path rpaths that are valid on any NixOS machine.
        };

        # Wrapper script that sets LD_LIBRARY_PATH for the GUI binary.
        # Required on non-NixOS hosts (nix-on-droid, foreign Linux with Nix)
        # where the dynamic linker won't find Mesa/X11 via rpath alone.
        guiWrapper = pkgs.writeShellScriptBin "onedrive-mount" ''
          export LD_LIBRARY_PATH="${runtimeLibPath}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
          exec ${package}/bin/onedrive-mount "$@"
        '';

      in {
        packages = {
          default = package;
          # Variant with the LD_LIBRARY_PATH wrapper bundled — useful on non-NixOS
          wrapped = pkgs.symlinkJoin {
            name = "onedrive-mount-wrapped";
            paths = [ guiWrapper package ];
          };
        };

        apps = {
          gui = {
            type = "app";
            program = "${guiWrapper}/bin/onedrive-mount";
          };
          daemon = {
            type = "app";
            program = "${package}/bin/onedrive-mountd";
          };
          # default app defined inline — avoids referencing self.apps.${system}
          # which breaks pure flake evaluation
          default = {
            type = "app";
            program = "${guiWrapper}/bin/onedrive-mount";
          };
        };

        devShells.default = pkgs.mkShell {
          inherit buildInputs nativeBuildInputs;
          LD_LIBRARY_PATH = runtimeLibPath;
          shellHook = ''
            echo "onedrive-mount dev shell"
            echo "  cargo build --bin onedrive-mountd --features daemon"
            echo "  cargo build --bin onedrive-mount  --features gui"
          '';
        };
      }) // {
        nixosModules.default = nixosModule;
        nixosModule = nixosModule;
      };
}
