# The Home Manager module: this is where `~/.config/onedrive-rc/config.toml`
# comes from when you don't want to hand-write TOML. The option schema lives in
# ./config-options.nix, shared with the NixOS module's per-user `userConfigs`.
{ self }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.onedrive-rc;
  inherit (lib) mkEnableOption mkOption types;

  shared = import ./config-options.nix { inherit lib pkgs; };
  configToml = shared.mkConfigFile cfg;
in {
  options.services.onedrive-rc = shared.options // {
    enable = mkEnableOption "the onedrive-rc daemon, with its config declared here";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.onedrive-rc;
      description = "The onedrive-rc package to use.";
    };

    systemdService.enable = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Manage the onedrive-rcd systemd --user unit. Disable this if you already
        get it from the NixOS system module (services.onedrive-rc.enable there)
        and only want this module for config generation — the two define the
        same unit name, and the more specific (this one, per-user) wins if
        both are left enabled, so leaving both on is harmless, just redundant.
      '';
    };
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [
    {
      home.packages = [ cfg.package pkgs.rclone pkgs.fuse3 ];
      xdg.configFile."onedrive-rc/config.toml".source = configToml;
    }
    (lib.mkIf cfg.systemdService.enable {
      systemd.user.services.onedrive-rcd = {
        Unit = {
          Description = "OneDrive daemon";
        };
        Service = {
          Type = "notify";
          NotifyAccess = "main";
          ExecStart = "${cfg.package}/bin/onedrive-rcd";
          Restart = "always";
          RestartSec = 2;
          # Never give up restarting; a daemon that stopped trying is useless.
          StartLimitIntervalSec = 0;
          # A daemon wedged on a hung FUSE mount stops pinging and gets
          # restarted, instead of sitting "active" doing nothing forever.
          WatchdogSec = 60;
          TimeoutStopSec = 30;
          Environment = [
            "RUST_BACKTRACE=1"
            # /run/wrappers/bin first: fusermount3 must be the setuid wrapper,
            # not the plain pkgs.fuse3 binary (no setuid bit -> mount gets EPERM).
            "PATH=/run/wrappers/bin:${lib.makeBinPath [ cfg.package pkgs.rclone ]}:/run/current-system/sw/bin:/usr/bin:/bin"
          ];
        };
        Install = {
          WantedBy = [ "default.target" ];
        };
      };
    })
  ]);
}
