# onedrive-tui

The control surface for [`onedrived`](../daemon/README.md): a scriptable CLI,
and — run with no arguments — a `ratatui` dashboard. Talks to the daemon over
its Unix socket; falls back to reading the state database directly (clearly
marked as stale) when the daemon isn't running, so `status`/`ls`/`find` still
answer.

## First-time setup

```sh
onedrive-tui setup
```

One command: authenticates a remote with rclone, asks for the mount point and
any folders to pin, writes `~/.config/onedrive/config.toml`, installs and
starts the systemd user service with linger enabled, and queues the first
index crawl.

```sh
onedrive-tui setup --reauth <remote-name>   # just redo the OAuth half
```

## The dashboard

```sh
onedrive-tui
```

Five panes — **Overview**, **Pins**, **Conflicts**, **Outbox**, **Log**:

| Key | Does |
| --- | --- |
| `Tab` / `Shift+Tab` | switch pane |
| `↑`/`↓` or `j`/`k` | move the selection (Pins/Conflicts/Outbox) |
| `Enter` | act on the selected row — close a conflict keeping both versions, or retry that row's outbox upload. Never destructive; there's no confirmation dialog because there's nothing here a worse choice would need one for |
| `r` | refresh now (it also refreshes every 2s on its own) |
| `q` / `Esc` / `Ctrl-C` | quit |

## Commands

```sh
onedrive-tui status [--json]        # one screen; exit 0/1/2 = healthy/degraded/broken
onedrive-tui doctor                 # checks the things that commonly break, with a fix for each
onedrive-tui log [-f] [--since 1h] [--level warn] [--remote X] [--pin X]

onedrive-tui pin ls
onedrive-tui pin sync [<name>]      # sync one pin now, or all of them

onedrive-tui conflicts [ls]
onedrive-tui conflicts resolve <id> --keep local|remote|both

onedrive-tui outbox [ls]
onedrive-tui outbox retry [<remote>]
onedrive-tui outbox discard <id>

onedrive-tui ls [<path>]            # offline, straight from the index
onedrive-tui find <pattern>
onedrive-tui index refresh [--full]

onedrive-tui mount remount [<remote>]

onedrive-tui service install | uninstall | restart | status
onedrive-tui reload                 # ask the daemon to re-read its config now
```

Run `onedrive-tui --help` for the authoritative, always-current list.

## Testing

```sh
cargo test -p onedrive-tui
```

`rclone_wizard`'s test drives the real non-interactive `rclone config`
protocol against this machine's actual rclone (no credentials needed — it
only goes as far as the OAuth question, declines the local-browser flow, and
confirms a rejected answer is recoverable rather than fatal). Everything
touching a real terminal (the dashboard's key handling, raw-mode/panic-hook
terminal restoration) was verified manually with `tmux send-keys`/
`capture-pane`, not by an automated test — there's no headless equivalent for
a program that talks to `/dev/tty` directly.
