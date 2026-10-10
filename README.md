# onedrive

Reliable OneDrive access on Linux, built on [rclone](https://rclone.org) — a
background daemon keeps `~/OneDrive` mounted and browsable even offline, pins
folders you need complete locally, and never discards one version of a
conflicted file in favor of another.

## Install (NixOS, flake)

```nix
{
  inputs.onedrive.url = "github:Yolo-TM/onedrive-mount/v0.3.0";

  outputs = { nixpkgs, onedrive, ... }: {
    nixosConfigurations.your-hostname = nixpkgs.lib.nixosSystem {
      modules = [
        ./configuration.nix
        onedrive.nixosModules.default
        { services.onedrive = { enable = true; users = [ "yourusername" ]; }; }
      ];
    };
  };
}
```

Then `onedrive-tui setup` once you've rebuilt. Other install paths (classic
`configuration.nix`, pre-built binaries, building from source) are below.

![onedrive-tui dashboard](docs/dashboard.svg)

```mermaid
flowchart TB
    User(["you"]) --> CLI["onedrive-tui<br/>CLI + dashboard"]
    CLI <--> Sock[["unix socket"]]

    subgraph D["onedrived"]
        direction TB
        Mount["mount<br/>supervised rclone mount"]
        Odfs["odfs<br/>FUSE overlay"]
        Index["index<br/>offline file listing"]
        Pins["pins<br/>rclone bisync"]
        Outbox["outbox<br/>offline write queue"]
        Alerts["alerts<br/>desktop notifications"]
    end

    Sock <--> D
    Odfs --> Mount
    Odfs --> Index
    Mount <--> OneDrive(("OneDrive"))
    Pins <--> OneDrive
    Outbox -. replays to .-> OneDrive

    Odfs --> FS1["~/OneDrive<br/>(always browsable)"]
    Pins --> FS2["~/onedrive-keepass<br/>(always complete)"]
    Outbox --> FS3["~/OneDrive.pending<br/>(queued offline writes)"]
    Alerts --> Desktop(["desktop notification"])
```

## Components

| | |
| --- | --- |
| [`onedrived`](crates/daemon/README.md) | the daemon — mounts, pins, the offline index, conflict handling, the outbox |
| [`onedrive-tui`](crates/cli/README.md) | CLI + dashboard — `status`, `doctor`, `pin`, `conflicts`, `outbox`, `setup`, … |
| [`onedrive-core`](crates/core/README.md) | the shared library behind both |

Each has its own README for what it does and how to use it. The full design,
the data-safety contract, and the reasoning (and verification) behind every
decision are in [`docs/OVERHAUL.md`](docs/OVERHAUL.md) — start there for *why*,
the component READMEs for *how*.

## Prerequisites

- [rclone](https://rclone.org) on `$PATH`
- `fuse3`, with unprivileged user mounts allowed
- a `systemd --user` session

## Install

### NixOS (classic `configuration.nix`, no flake)

```nix
{ config, pkgs, lib, ... }:
let
  onedrive = builtins.getFlake "github:Yolo-TM/onedrive-mount/v0.3.0";
in {
  imports = [ onedrive.nixosModules.default ];
  services.onedrive = {
    enable = true;
    users = [ "yourusername" ];
  };
}
```

```sh
sudo nixos-rebuild switch
```

### Pre-built binaries (GitHub releases)

| Artifact | Links against | Use when |
| --- | --- | --- |
| `onedrived-x86_64-linux-musl` / `onedrive-tui-x86_64-linux-musl` | nothing (fully static) | any Linux |
| `onedrived-x86_64-linux` / `onedrive-tui-x86_64-linux` | glibc | Debian, Fedora, Arch, etc. |
| `onedrived-x86_64-linux-nix` / `onedrive-tui-x86_64-linux-nix` | Nix store (rpath-patched) | NixOS, manual install without the flake |

```sh
curl -L https://github.com/Yolo-TM/onedrive-mount/releases/latest/download/onedrived-x86_64-linux-musl \
  -o ~/.local/bin/onedrived && chmod +x ~/.local/bin/onedrived
curl -L https://github.com/Yolo-TM/onedrive-mount/releases/latest/download/onedrive-tui-x86_64-linux-musl \
  -o ~/.local/bin/onedrive-tui && chmod +x ~/.local/bin/onedrive-tui
```

### Build from source

```sh
nix develop            # or just: cargo build --release --workspace
cargo build --release -p onedrived -p onedrive-tui
cp target/release/{onedrived,onedrive-tui} ~/.local/bin/
```

## First-time setup

```sh
onedrive-tui setup
```

See [`crates/cli/README.md`](crates/cli/README.md) for what it asks and what
`onedrive-tui --reauth` does. Config lives at `~/.config/onedrive/config.toml`
— every field, its default, and what it does is in
[`config.example.toml`](config.example.toml).
