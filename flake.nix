{
  description = "onedrive — reliable OneDrive access on Linux, built on rclone";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    let
      # The thing the old 0.2.x module never did: actually install the
      # systemd user unit. Previously the unit was written at runtime by a
      # button in a GUI, which meant a machine could have the package
      # installed and no service at all — and the unit it wrote ordered
      # itself against `network-online.target`, a *system* unit that has no
      # effect in the user manager.
      onedriveModule = { config, lib, pkgs, ... }:
        let
          cfg = config.services.onedrive;
          pkg = self.packages.${pkgs.stdenv.hostPlatform.system}.onedrive;
        in {
          options.services.onedrive = {
            enable = lib.mkEnableOption "onedrive daemon";

            package = lib.mkOption {
              type = lib.types.package;
              default = pkg;
              description = "The onedrive package to use.";
            };

            users = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [ ];
              example = [ "alice" ];
              description = ''
                Users for whom to enable linger, so the daemon keeps running
                after logout. Without this, background sync stops when you log
                out — the single most common way the old setup silently died.
              '';
            };
          };

          config = lib.mkIf cfg.enable {
            environment.systemPackages = [
              cfg.package
              pkgs.rclone
              pkgs.fuse3
            ];

            programs.fuse.userAllowOther = true;

            systemd.user.services.onedrived = {
              description = "OneDrive daemon";
              wantedBy = [ "default.target" ];

              # Deliberately no `after = [ "network-online.target" ]`: that is a
              # system unit and ordering a user unit against it does nothing.
              # Network readiness is decided at runtime over D-Bus instead.
              serviceConfig = {
                Type = "notify";
                NotifyAccess = "main";
                ExecStart = "${cfg.package}/bin/onedrived";
                Restart = "always";
                RestartSec = 2;
                # Never give up restarting; a daemon that has stopped trying is
                # useless, and the default start limiter would do exactly that.
                StartLimitIntervalSec = 0;
                # A daemon wedged on a hung FUSE mount stops pinging and gets
                # restarted, instead of sitting "active" doing nothing forever.
                WatchdogSec = 60;
                TimeoutStopSec = 30;
              };

              environment.RUST_BACKTRACE = "1";
              path = [ pkgs.rclone pkgs.fuse3 ];
            };

            users.users = lib.genAttrs cfg.users (_: { linger = true; });
          };
        };

    in flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rustToolchain = pkgs.rust-bin.stable.latest.default;

        coreToml = builtins.fromTOML (builtins.readFile ./crates/core/Cargo.toml);

        # Exclude target/ and other non-source dirs so the store hash is
        # stable and doesn't differ between machines with dirty trees.
        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = path: type:
            let rel = pkgs.lib.removePrefix (toString ./. + "/") path;
            in !(pkgs.lib.hasPrefix "target/" rel)
            && !(pkgs.lib.hasPrefix ".git/" rel);
        };

        onedrive = pkgs.rustPlatform.buildRustPackage {
          pname = "onedrive";
          version = coreToml.package.version;

          inherit src;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ rustToolchain ];

          # rusqlite is built with its bundled SQLite, and fuser talks to
          # /dev/fuse directly — the only native requirement is a C compiler,
          # which stdenv already provides. No X11/GL, no pkg-config.
          cargoBuildFlags = [ "-p" "onedrived" "-p" "onedrive-tui" ];
          cargoTestFlags = [ "-p" "onedrive-core" "-p" "onedrived" "-p" "onedrive-tui" ];

          # The Nix build sandbox has no `/dev/fuse` and no privilege to mount
          # one, so odfs's and the mount supervisor's tests that spawn a real
          # FUSE mount cannot pass here — verified by running the full suite
          # under `nix build` and seeing exactly these 11 fail with "No such
          # file or directory" from the mount call, nothing else. They run
          # (and are what actually guards this behaviour) under plain `cargo
          # test --workspace` and in CI's non-Nix job, both of which have a
          # real FUSE device.
          checkFlags = [
            "--skip=mount::tests::disabling_the_overlay_unmounts_odfs_on_the_next_reconcile"
            "--skip=mount::tests::odfs_mounts_even_though_the_rclone_backing_mount_cannot"
            "--skip=mount::tests::reconciling_again_does_not_remount_an_already_mounted_odfs"
            "--skip=mount::tests::removing_the_remote_unmounts_odfs_too"
            "--skip=mount::tests::stop_all_unmounts_odfs"
            "--skip=odfs::tests::a_file_created_entirely_offline_is_readable_and_shows_up_in_readdir"
            "--skip=odfs::tests::a_read_with_nothing_cached_at_all_fails_rather_than_inventing_content"
            "--skip=odfs::tests::a_real_mount_lists_and_reads_the_backing_directory"
            "--skip=odfs::tests::an_offline_write_to_a_known_file_is_staged_and_served_on_read_back"
            "--skip=odfs::tests::file_creation_stages_offline_but_mkdir_is_still_refused"
            "--skip=odfs::tests::readdir_survives_the_backing_directory_disappearing"
          ];

          nativeCheckInputs = [ pkgs.rclone ];

          # The sandbox points HOME at a non-existent /homeless-shelter. Our own
          # tests use explicit temp roots, but `Paths::from_env` is exercised and
          # the daemon writes a lock and a database, so give it a real one.
          preCheck = ''
            export HOME=$(mktemp -d)
            export XDG_RUNTIME_DIR=$(mktemp -d)
          '';

          postInstall = ''
            install -Dm644 config.example.toml \
              $out/share/doc/onedrive/config.example.toml
          '';

          meta = with pkgs.lib; {
            description = "Reliable OneDrive access on Linux, built on rclone";
            homepage = "https://github.com/Yolo-TM/onedrive-mount";
            platforms = platforms.linux;
            mainProgram = "onedrive-tui";
          };
        };

      in {
        packages = {
          default = onedrive;
          inherit onedrive;
        };

        apps = {
          onedrived = {
            type = "app";
            program = "${onedrive}/bin/onedrived";
          };
          tui = {
            type = "app";
            program = "${onedrive}/bin/onedrive-tui";
          };
          # default app defined inline — avoids referencing self.apps.${system}
          # which breaks pure flake evaluation
          default = {
            type = "app";
            program = "${onedrive}/bin/onedrive-tui";
          };
        };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = [ rustToolchain ];
          shellHook = ''
            echo "onedrive dev shell"
            echo "  cargo test --workspace"
            echo "  cargo run -p onedrived -- --foreground"
            echo "  cargo run -p onedrive-tui -- status"
          '';
        };
      }) // {
        nixosModules.default = onedriveModule;
        nixosModules.onedrive = onedriveModule;
      };
}
