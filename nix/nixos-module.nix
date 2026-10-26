# The NixOS system module: installs the package, the systemd --user unit, and
# fuse/linger plumbing for the users listed in `services.onedrive-rc.users`.
#
# Per-user config (mount points, vfs tuning, pins, ...) can be declared two
# ways, both using the same schema (./config-options.nix):
#   - `services.onedrive-rc.userConfigs.<user>` here: one generated
#     `~/.config/onedrive-rc/config.toml` per listed user, symlinked into place
#     by systemd-tmpfiles. No Home Manager needed.
#   - `services.onedrive-rc` in Home Manager (./home-manager-module.nix).
# Don't declare the same user in both: they would fight over the file.
{ self }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.onedrive-rc;
  pkg = self.packages.${pkgs.stdenv.hostPlatform.system}.onedrive-rc;
  shared = import ./config-options.nix { inherit lib pkgs; };

  tmpfilesFor = user: userCfg:
    let
      u = config.users.users.${user};
      dir = "${u.home}/.config/onedrive-rc";
      file = shared.mkConfigFile userCfg;
    in [
      # Mode `-`: created 0755 if missing, left alone if it already exists.
      "d ${u.home}/.config - ${user} ${u.group} -"
      "d ${dir} - ${user} ${u.group} -"
      # `L+` replaces whatever is there, including a hand-written config.toml,
      # so a stale store path never lingers after a rebuild.
      "L+ ${dir}/config.toml - - - - ${file}"
    ];
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
        Users in `userConfigs` get linger automatically.
      '';
    };

    userConfigs = lib.mkOption {
      type = lib.types.attrsOf (lib.types.submodule { options = shared.options; });
      default = { };
      example = lib.literalExpression ''
        {
          alice = {
            remotes.onedrive-timm = {
              mount_point = "~/OneDrive";
              pins.Keepass = {
                remote_path = "Dokumente/Keepass";
                local_path = "~/onedrive-keepass";
              };
            };
          };
        }
      '';
      description = ''
        Per-user `config.toml`, declared from the system config. Each entry
        writes `~/.config/onedrive-rc/config.toml` for that user as a symlink
        into the Nix store, so it is read-only and replaces any existing
        hand-written file there on the next switch (move it aside first if you
        want to keep it). The rclone remote must already exist in that user's
        rclone.conf — run `onedrive-rc setup` once as that user to authenticate.
        Do not also set `services.onedrive-rc` for the same user in Home
        Manager.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = lib.mapAttrsToList (user: _: {
      assertion = config.users.users ? ${user};
      message = "services.onedrive-rc.userConfigs.${user}: no such user in users.users.";
    }) cfg.userConfigs;

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
      # /run/wrappers first: rclone shells out to `fusermount3`, which must be
      # the setuid wrapper from `programs.fuse`. The plain `pkgs.fuse3` binary
      # has no setuid bit, so the kernel refuses the mount (EPERM).
      path = [ "/run/wrappers" pkgs.rclone ];
    };

    users.users = lib.genAttrs
      (lib.unique (cfg.users ++ builtins.attrNames cfg.userConfigs))
      (_: { linger = true; });

    systemd.tmpfiles.rules = lib.concatLists (lib.mapAttrsToList tmpfilesFor cfg.userConfigs);
  };
}
