# The shared option schema for `~/.config/onedrive-rc/config.toml`, used by both
# the Home Manager module and the NixOS module's per-user `userConfigs`. Every
# option mirrors a key documented in ../config.example.toml, with the same
# defaults, so the two stay interchangeable — write the file by hand, or
# declare it with either module, or start from here and drop to
# extra_flags/patterns for anything unmodelled.
#
# Durations and sizes stay plain strings ("30d", "25G"): onedrive-rc-core parses
# and re-validates them itself (crates/core/src/units.rs), so re-typing that
# grammar as a Nix type would only be a second, weaker copy of checks that
# already happen at daemon startup with a much better error message.
{ lib, pkgs }:
let
  inherit (lib) mkOption types;

  tomlFormat = pkgs.formats.toml { };
  durOrSize = types.str;

  pinModule = types.submodule ({ name, ... }: {
    options = {
      name = mkOption {
        type = types.str;
        default = name;
        description = "Pin name. Must be globally unique across all remotes.";
      };
      remote_path = mkOption {
        type = types.str;
        example = "Dokumente/Keepass";
        description = "Path on the remote, relative to its root. No leading slash.";
      };
      local_path = mkOption {
        type = types.str;
        example = "~/onedrive-keepass";
        description = "A real local directory. Must not live inside any mount point.";
      };
      interval = mkOption {
        type = durOrSize;
        default = "5m";
        description = "Floor on how often this pin syncs.";
      };
      sync_on_change = mkOption {
        type = types.bool;
        default = true;
        description = "Also sync (debounced) on local change, not just on interval.";
      };
      conflict = mkOption {
        type = types.enum [ "keep-both" "newer-wins" ];
        default = "keep-both";
        description = ''
          What to do when both sides changed: "keep-both" keeps the newer
          content under the canonical name and the loser beside it with a
          timestamped suffix; "newer-wins" discards the loser outright (it is
          only recoverable from .versions/ while `versions` is enabled).
        '';
      };
      upload_conflict_copies = mkOption {
        type = types.bool;
        default = true;
        description = "Let conflict copies propagate to the remote too.";
      };
      max_delete_percent = mkOption {
        type = types.ints.between 1 100;
        default = 50;
        description = ''
          Abort the run if more than this PERCENTAGE of files would be
          deleted (rclone's --max-delete, despite the name, is a percentage).
        '';
      };
      versions = mkOption {
        type = types.bool;
        default = true;
        description = "Pre-run snapshots into <local_path>/.versions/.";
      };
      versions_retention = mkOption {
        type = durOrSize;
        default = "14d";
      };
      versions_max_size = mkOption {
        type = durOrSize;
        default = "50M";
        description = "Files larger than this are not snapshotted.";
      };
      patterns = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = "Include filters. Empty means everything.";
      };
      enabled = mkOption {
        type = types.bool;
        default = true;
      };
    };
  });

  remoteModule = types.submodule ({ name, ... }: {
    options = {
      name = mkOption {
        type = types.str;
        default = name;
        description = "The rclone remote name, without the trailing colon. Must already exist in rclone.conf.";
      };
      mount_point = mkOption {
        type = types.str;
        example = "~/OneDrive";
        description = "Where you browse your files — our own FUSE overlay, not the raw rclone mount.";
      };
      enabled = mkOption {
        type = types.bool;
        default = true;
      };

      mount = {
        cache_dir = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "Overrides rclone's --cache-dir. Defaults to ~/.cache/onedrive-rc/<remote>/rclone.";
        };
        vfs_cache_mode = mkOption {
          type = types.enum [ "off" "minimal" "writes" "full" ];
          default = "full";
          description = ''
            "full" or "writes" are required for offline reads to work at all —
            validation rejects "off"/"minimal" while the overlay is enabled.
          '';
        };
        vfs_cache_max_age = mkOption {
          type = durOrSize;
          default = "30d";
          description = "How long a downloaded file stays available offline.";
        };
        vfs_cache_max_size = mkOption {
          type = durOrSize;
          default = "25G";
        };
        vfs_cache_min_free_space = mkOption {
          type = durOrSize;
          default = "10G";
        };
        vfs_write_back = mkOption {
          type = durOrSize;
          default = "5s";
        };
        dir_cache_time = mkOption {
          type = durOrSize;
          default = "72h";
          description = "Safe to make long because poll_interval keeps it fresh and the local index is the offline fallback.";
        };
        poll_interval = mkOption {
          type = durOrSize;
          default = "30s";
          description = "Must be smaller than dir_cache_time.";
        };
        transfers = mkOption {
          type = types.ints.between 1 64;
          default = 4;
        };
        checkers = mkOption {
          type = types.ints.between 1 64;
          default = 4;
        };
        buffer_size = mkOption {
          type = durOrSize;
          default = "8M";
        };
        vfs_read_chunk_size = mkOption {
          type = durOrSize;
          default = "32M";
        };
        extra_flags = mkOption {
          type = types.listOf types.str;
          default = [ ];
          description = ''
            Escape hatch for rclone flags not modelled above. One argv element
            per entry: write "--flag=value", not "--flag value".
          '';
        };
      };

      overlay.enabled = mkOption {
        type = types.bool;
        default = true;
        description = ''
          false exposes the rclone mount directly at mount_point and gives up
          offline directory listings.
        '';
      };

      index = {
        full_crawl_interval = mkOption {
          type = durOrSize;
          default = "24h";
        };
        opportunistic = mkOption {
          type = types.bool;
          default = true;
          description = "Update the index from directory listings your own browsing already triggers.";
        };
        tpslimit = mkOption {
          type = types.ints.between 1 100;
          default = 10;
          description = "rclone --tpslimit for crawls, to stay under OneDrive's throttling.";
        };
      };

      offline = {
        writes = mkOption {
          type = types.enum [ "queue" "reject" "readonly" ];
          default = "queue";
          description = ''
            What happens to a write while offline: "queue" stages it and
            re-verifies the remote before replaying on reconnect, "reject"
            fails the write immediately, "readonly" refuses all writes.
          '';
        };
        outbox_dir = mkOption {
          type = types.str;
          default = "~/OneDrive.pending";
          description = "Visible on purpose, so pending uploads can be inspected in a file manager.";
        };
      };

      pins = mkOption {
        type = types.attrsOf pinModule;
        default = { };
        description = "Folders kept fully available offline, synced bidirectionally via rclone bisync.";
      };
    };
  });

  pinToToml = _: p: {
    inherit (p)
      name remote_path local_path interval sync_on_change conflict
      upload_conflict_copies max_delete_percent versions versions_retention
      versions_max_size patterns enabled;
  };

  remoteToToml = _: r: {
    inherit (r) name mount_point enabled;
    mount = (lib.filterAttrs (_: v: v != null) {
      inherit (r.mount)
        cache_dir vfs_cache_mode vfs_cache_max_age vfs_cache_max_size
        vfs_cache_min_free_space vfs_write_back dir_cache_time poll_interval
        transfers checkers buffer_size vfs_read_chunk_size extra_flags;
    });
    overlay = { inherit (r.overlay) enabled; };
    index = { inherit (r.index) full_crawl_interval opportunistic tpslimit; };
    offline = { inherit (r.offline) writes outbox_dir; };
    pin = lib.mapAttrsToList pinToToml r.pins;
  };
in {
  # The `daemon` and `remotes` options, as plain attrsets of options so a
  # caller can splice them into its own `options` (HM) or into a submodule
  # (NixOS `userConfigs.<user>`).
  options = {
    daemon = {
      log_level = mkOption {
        type = types.enum [ "error" "warn" "info" "debug" "trace" ];
        default = "info";
        description = "Verbosity of our own log.";
      };
      rclone_log_level = mkOption {
        type = types.enum [ "error" "notice" "info" "debug" ];
        default = "notice";
        description = ''
          Verbosity passed to rclone children. Deliberately separate from
          log_level: sharing one knob is how the old setup produced a 1.4 GB
          rclone log the first time anyone turned on debug logging.
        '';
      };
      log_retention_days = mkOption {
        type = types.ints.positive;
        default = 7;
      };
      log_retention_size = mkOption {
        type = durOrSize;
        default = "200M";
      };
    };

    remotes = mkOption {
      type = types.attrsOf remoteModule;
      default = { };
      example = lib.literalExpression ''
        {
          onedrive-timm = {
            mount_point = "~/OneDrive";
            pins.Keepass = {
              remote_path = "Dokumente/Keepass";
              local_path = "~/onedrive-keepass";
            };
          };
        }
      '';
      description = ''
        Remotes to mount, one per `[[remote]]` block in config.toml. The
        attribute name becomes the rclone remote name (and must already exist
        in rclone.conf — this module does not do the OAuth dance; run
        `onedrive-rc setup` once to authenticate and it will pick up this
        declared config instead of generating its own).
      '';
    };
  };

  # Renders `{ daemon, remotes }` (a evaluated option set) to a store file.
  mkConfigFile = c: tomlFormat.generate "onedrive-rc-config.toml" {
    daemon = {
      inherit (c.daemon)
        log_level rclone_log_level log_retention_days log_retention_size;
    };
    remote = lib.mapAttrsToList remoteToToml c.remotes;
  };
}
