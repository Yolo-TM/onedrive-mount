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

## Build

### NixOS (recommended)

```sh
nix develop
cargo build --release --bin onedrive-mountd --features daemon
cargo build --release --bin onedrive-mount  --features gui
```

Or build and run directly via flake:

```sh
nix run .#gui
nix run .#daemon
```

### Non-NixOS

```sh
cargo build --release --bin onedrive-mountd --features daemon
cargo build --release --bin onedrive-mount  --features gui
```

The GUI requires `libX11`, `libGL`, `libxkbcommon`, `libXcursor`, `libXi`, `libXrandr`, `libxcb` at runtime. On NixOS, the flake's `postFixup` patches the rpath so no `LD_LIBRARY_PATH` wrapper is needed.

## Install

```sh
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
