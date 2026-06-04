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
          # Pull the package from this flake for the host system
          pkg = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        in {
          options.services.onedrive-mount = {
            enable = lib.mkEnableOption "onedrive-mount daemon";
          };

          config = lib.mkIf cfg.enable {
            # Make both binaries available system-wide
            environment.systemPackages = [ pkg ];

            # The daemon runs as a systemd user service — managed per-user via the GUI.
            # Installing the package puts the binaries in PATH so the GUI's
            # "Install service" button can write the unit and start it.
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

        buildInputs = runtimeLibs ++ (with pkgs; [ rclone ]);

        nativeBuildInputs = with pkgs; [
          rustToolchain
          pkg-config
          patchelf
        ];

        package = pkgs.rustPlatform.buildRustPackage {
          pname = "onedrive-mount";
          version = "0.1.3";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          inherit buildInputs nativeBuildInputs;

          buildPhase = ''
            cargo build --release --bin onedrive-mountd --features daemon
            cargo build --release --bin onedrive-mount  --features gui
          '';

          installPhase = ''
            mkdir -p $out/bin
            install -m755 target/release/onedrive-mountd $out/bin/
            install -m755 target/release/onedrive-mount  $out/bin/

            mkdir -p $out/share/icons/hicolor/scalable/apps
            install -m644 assets/icon.svg $out/share/icons/hicolor/scalable/apps/onedrive-mount.svg

            mkdir -p $out/share/applications
            install -m644 assets/onedrive-mount.desktop $out/share/applications/onedrive-mount.desktop
          '';

          # Patch ELF rpath so the GUI binary finds its libs without LD_LIBRARY_PATH
          postFixup = ''
            patchelf --set-rpath "${runtimeLibPath}" $out/bin/onedrive-mount
          '';
        };

      in {
        packages.default = package;

        # `nix run .#gui` / `nix run .#daemon`
        apps.gui = {
          type = "app";
          program = "${pkgs.writeShellScript "onedrive-mount" ''
            export LD_LIBRARY_PATH="${runtimeLibPath}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
            exec ${package}/bin/onedrive-mount "$@"
          ''}";
        };
        apps.daemon = {
          type = "app";
          program = "${package}/bin/onedrive-mountd";
        };
        apps.default = self.apps.${system}.gui;

        # `nix develop` — drops into a shell with all build + runtime deps available
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
        # Expose the NixOS module at the top level (system-independent)
        nixosModules.default = nixosModule;
        nixosModule = nixosModule;
      };
}
