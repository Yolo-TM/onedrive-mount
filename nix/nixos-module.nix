# The NixOS system module: installs the package, the systemd --user unit, and
# fuse/linger plumbing for the users listed in `services.onedrive-rc.users`.
#
# This is deliberately *not* where per-remote config (mount points, vfs
# tuning, pins, ...) lives — that is per-user data and belongs in each user's
# own Home Manager config (see ./home-manager-module.nix), not something one
# system-wide module can sensibly own for every user on the machine.
{ self }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.onedrive-rc;
  pkg = self.packages.${pkgs.stdenv.hostPlatform.system}.onedrive-rc;
in {
  options.services.onedrive-rc = {
    enable = lib.mkEnableOption "onedrive-rc daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkg;
      description = "The onedrive-rc package to use.";
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

    systemd.user.services.onedrive-rcd = {
      description = "OneDrive daemon";
      wantedBy = [ "default.target" ];

      # Deliberately no `after = [ "network-online.target" ]`: that is a
      # system unit and ordering a user unit against it does nothing.
      # Network readiness is decided at runtime over D-Bus instead.
      serviceConfig = {
        Type = "notify";
        NotifyAccess = "main";
        ExecStart = "${cfg.package}/bin/onedrive-rcd";
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
}
