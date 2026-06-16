# onedrive-mount

GUI wrapper around [rclone](https://rclone.org) for managing OneDrive mounts and offline file sync. A persistent background daemon handles mounts and sync rules; the GUI configures it and shows live status — no GUI needs to be open for the daemon to run.

## Components

| Binary | Purpose |
| --- | --- |
| `onedrive-mountd` | Background daemon — manages mounts, runs sync rules |
| `onedrive-mount` | GUI — config editor and live status viewer |

Communication is file-based: the GUI writes [`~/.config/onedrive-mount/config.toml`](src/paths.rs), the daemon watches it via inotify and reloads on change. The daemon writes a status file the GUI polls for live updates.

## Prerequisites

- [rclone](https://rclone.org) in `$PATH` with at least one remote configured
- `fuse` / `fuse3` for mounting
- `systemd` user session (for service management)

## Install

### NixOS (classic, no flake)

If your system uses a plain `configuration.nix`, add this to it:

```nix
# /etc/nixos/configuration.nix
{ config, pkgs, lib, ... }:
let
  onedrive-mount = builtins.getFlake "github:Yolo-TM/onedrive-mount/v0.2.0";
in {
  imports = [ onedrive-mount.nixosModules.default ];
  services.onedrive-mount.enable = true;
  # ... rest of your config
}
```

Then rebuild as normal — no `--flake` flag needed:

```sh
sudo nixos-rebuild switch
```

To update to a new release, bump the version tag in the `builtins.getFlake` URL and run `nixos-rebuild switch` again.

### NixOS (flake-based system)

Add this flake as an input in your `/etc/nixos/flake.nix`:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    onedrive-mount.url = "github:Yolo-TM/onedrive-mount/v0.2.0";
  };

  outputs = { nixpkgs, onedrive-mount, ... }: {
    nixosConfigurations.your-hostname = nixpkgs.lib.nixosSystem {
      modules = [
        ./configuration.nix
        onedrive-mount.nixosModules.default
        { services.onedrive-mount.enable = true; }
      ];
    };
  };
}
```

```sh
sudo nixos-rebuild switch --flake /etc/nixos#your-hostname
```

To update: bump the version tag in the input URL, or run `nix flake update onedrive-mount` to pull latest, then rebuild.

---

Both methods install the binaries, the `.desktop` entry, `rclone`, `fuse3`, and enable unprivileged FUSE mounts. The daemon runs as a per-user systemd service — start it from the GUI's **Service** tab or with:

```sh
systemctl --user enable --now onedrive-mountd
```

> **Note:** To keep the daemon running after logout (e.g. for background sync without an active session):
>
> ```sh
> loginctl enable-linger
> ```

### Pre-built binaries (GitHub releases)

| Artifact | Links against | Use when |
| --- | --- | --- |
| `onedrive-mount-x86_64-linux` | glibc + system X11/GL | Debian, Fedora, Arch, etc. |
| `onedrive-mountd-x86_64-linux` | glibc | same, daemon only |
| `onedrive-mountd-x86_64-linux-musl` | nothing (fully static) | any Linux, no GUI |
| `onedrive-mount-x86_64-linux-nix` | Nix store (rpath-patched) | NixOS, manual install without flake |
| `onedrive-mountd-x86_64-linux-nix` | Nix store (rpath-patched) | same, daemon only |

```sh
# example: daemon-only on an arbitrary Linux box
curl -L https://github.com/Yolo-TM/onedrive-mount/releases/latest/download/onedrive-mountd-x86_64-linux-musl \
  -o ~/.local/bin/onedrive-mountd && chmod +x ~/.local/bin/onedrive-mountd
```

### Build from source

**NixOS:**

```sh
nix develop
cargo build --release --bin onedrive-mountd --features daemon
cargo build --release --bin onedrive-mount  --features gui
# or: nix run .#gui / nix run .#daemon
```

**Other Linux** (requires `libX11`, `libGL`, `libxkbcommon`, `libXcursor`, `libXi`, `libXrandr`, `libxcb`):

```sh
cargo build --release --bin onedrive-mountd --features daemon
cargo build --release --bin onedrive-mount  --features gui
cp target/release/onedrive-mountd target/release/onedrive-mount ~/.local/bin/
```

## First-time setup

1. Authenticate a remote with rclone — either run `rclone config` in a terminal, or use the **Setup new remote…** button in the GUI.

2. Open the GUI (`onedrive-mount`), configure your remotes and sync rules, then click **Save**.

3. Open the GUI, click **Install service** in the bottom status bar to register the systemd user service.

The daemon starts automatically on login from that point on.

## Configuration

Config lives at `~/.config/onedrive-mount/config.toml`. See [config.example.toml](config.example.toml) for all available fields and their defaults. The daemon reloads automatically when the file changes — no restart needed.

## Running the daemon manually

```sh
onedrive-mountd
```

Logs are written to `~/.local/share/onedrive-mount/daemon.log`. The log level can be changed in the GUI's **Logging** tab or directly in `config.toml`.

```sh
systemctl --user status onedrive-mountd
journalctl --user -u onedrive-mountd -f
```

## Uninstall service

Use the **Service** tab in the GUI → **Disable & remove**, or:

```sh
systemctl --user disable --now onedrive-mountd
rm ~/.config/systemd/user/onedrive-mountd.service
systemctl --user daemon-reload
```
