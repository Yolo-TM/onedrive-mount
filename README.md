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

### NixOS

Add to your `configuration.nix`:

```nix
environment.systemPackages = [
  pkgs.rclone
  pkgs.fuse3
  (import (pkgs.fetchTarball {
    url = "https://github.com/Yolo-TM/onedrive-mount/releases/latest/download/onedrive-mount-x86_64-linux-nix.tar.gz";
    sha256 = lib.fakeHash;
  }) { inherit pkgs; })
];
```

Run `nixos-rebuild switch` — it will fail with the correct hash in the error output. Replace `lib.fakeHash` with that hash, then run `nixos-rebuild switch` again.

This installs both binaries and the `.desktop` entry. The daemon runs as a per-user systemd service — start it from the GUI's **Service** tab or with `systemctl --user enable --now onedrive-mountd`.

### Pre-built binaries (GitHub releases)

Three variants are published per release:

| Artifact | Links against | Use when |
| --- | --- | --- |
| `onedrive-mount-x86_64-linux` | glibc + system X11/GL | Debian, Fedora, Arch, etc. |
| `onedrive-mountd-x86_64-linux` | glibc | same, daemon only |
| `onedrive-mountd-x86_64-linux-musl` | nothing (fully static) | any Linux, no GUI |
| `onedrive-mount-x86_64-linux-nix` | Nix store (rpath-patched) | NixOS without flake module |
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

3. Go to the **Service** tab → **Install & enable** to register the systemd user service.

The daemon starts automatically on login from that point on.

## Configuration

Config lives at `~/.config/onedrive-mount/config.toml`. See [config.example.toml](config.example.toml) for all available fields and their defaults. The daemon reloads automatically when the file changes — no restart needed.

## Running the daemon manually

```sh
RUST_LOG=info onedrive-mountd
```

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
