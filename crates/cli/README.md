# onedrive-rc

The control surface for [`onedrive-rcd`](../daemon/README.md): a scriptable CLI,
and — run with no arguments — a `ratatui` dashboard. Talks to the daemon over
its Unix socket; falls back to reading the state database directly (clearly
marked as stale) when the daemon isn't running, so `status`/`ls`/`find` still
answer.

## First-time setup

```sh
onedrive-rc setup
```

One command: authenticates a remote with rclone, asks for the mount point and
any folders to pin, writes `~/.config/onedrive-rc/config.toml`, installs and
starts the systemd user service with linger enabled, and queues the first
index crawl.

```sh
onedrive-rc setup --reauth <remote-name>   # just redo the OAuth half
```

## The dashboard

```sh
onedrive-rc
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
onedrive-rc status [--json]        # one screen; exit 0/1/2 = healthy/degraded/broken
onedrive-rc doctor                 # checks the things that commonly break, with a fix for each
onedrive-rc log [-f] [--since 1h] [--level warn] [--remote X] [--pin X]

onedrive-rc pin ls
onedrive-rc pin sync [<name>]      # sync one pin now, or all of them

onedrive-rc conflicts [ls]
onedrive-rc conflicts resolve <id> --keep local|remote|both

onedrive-rc outbox [ls]
onedrive-rc outbox retry [<remote>]
onedrive-rc outbox discard <id>

onedrive-rc ls [<path>]            # offline, straight from the index
onedrive-rc find <pattern>
onedrive-rc index refresh [--full]

onedrive-rc mount remount [<remote>]

onedrive-rc service install | uninstall | restart | status
onedrive-rc reload                 # ask the daemon to re-read its config now
```

Run `onedrive-rc --help` for the authoritative, always-current list.

## Testing

```sh
cargo test -p onedrive-rc
```

`rclone_wizard`'s test drives the real non-interactive `rclone config`
protocol against this machine's actual rclone (no credentials needed — it
only goes as far as the OAuth question, declines the local-browser flow, and
confirms a rejected answer is recoverable rather than fatal). Everything
touching a real terminal (the dashboard's key handling, raw-mode/panic-hook
terminal restoration) was verified manually with `tmux send-keys`/
`capture-pane`, not by an automated test — there's no headless equivalent for
a program that talks to `/dev/tty` directly.
