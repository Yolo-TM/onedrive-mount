# onedrive-rcd

The background daemon. One process per machine, controlled over a Unix
socket by [`onedrive-rc`](../cli/README.md) and configured from
`~/.config/onedrive-rc/config.toml` (schema and every default:
[`config.example.toml`](../../config.example.toml)).

See [`docs/OVERHAUL.md`](../../docs/OVERHAUL.md) for the full design and the
data-safety contract (§4) everything here exists to uphold.

## What it runs

| Subsystem | Does |
| --- | --- |
| **supervisor** | capped-backoff restart of every subsystem below; `sd_notify` READY/WATCHDOG |
| **lifecycle** | D-Bus: sleep/resume, network up/down, screen lock — drives Online ⇄ Offline/Suspending |
| **mount** | a supervised `rclone mount`, hidden behind odfs, with a timeout-guarded liveness probe |
| **odfs** | the FUSE overlay at the mount point — always mounted, serves offline reads from the index/VFS cache when the backing mount can't |
| **index** | `rclone lsjson --hash` crawls plus opportunistic updates from odfs's own `readdir`s, kept in SQLite |
| **pins** | `rclone bisync` per pinned folder — checksum-compared, both versions always kept on a conflict |
| **outbox** | offline writes staged to disk, replayed on reconnect only after re-verifying the remote still matches what the edit started from |
| **alerts** | `org.freedesktop.Notifications` — conflicts and other problems stay visible until actually resolved |
| **log** | one rotated JSONL stream; rclone's own JSON log is parsed and merged in, not kept separate |
| **ipc** | the Unix socket `onedrive-rc` talks to |

## Running it

Normally you don't — the systemd user service does
(`onedrive-rc service install`, or the NixOS module). To run it yourself,
e.g. while debugging:

```sh
onedrive-rcd --foreground    # logs to stderr instead of the JSONL file
```

```sh
onedrive-rcd --check-config  # validate config.toml and exit; what `doctor` uses
onedrive-rcd --print-unit    # print the systemd unit this daemon expects to run under
```

Only one instance runs per user at a time (`flock` + a PID/identity check); a
second invocation refuses rather than racing the first.

## Logs and state

```sh
systemctl --user status onedrive-rcd
journalctl --user -u onedrive-rcd -f
```

- `~/.local/state/onedrive-rc/log/onedrive-rcd.jsonl` — the one log stream (daily
  rotation, retention controlled by `config.toml`'s `[daemon]` section).
  Read it with `onedrive-rc log`, not directly — that's what applies the
  level/remote/pin filters and the human-readable rendering.
- `~/.local/state/onedrive-rc/state.db` — SQLite (WAL): the offline index, pin
  state, open conflicts, the outbox, and recent events. `onedrive-rc`
  reads this directly (read-only) when the daemon isn't answering, which is
  why `status`/`ls`/`find` still work with the daemon down — clearly marked
  as stale rather than silently looking live.

## Testing

```sh
cargo test -p onedrive-rcd
```

The safety-critical pin-sync behaviour (`crates/daemon/tests/safety_matrix.rs`)
runs against a local-disk `alias` rclone remote standing in for OneDrive, so
it needs no credentials and nothing it does ever reaches a real remote.
`odfs`'s and `mount`'s tests that mount a real FUSE filesystem need an actual
`/dev/fuse` — they're skipped by name in `flake.nix`'s Nix sandbox build,
which has neither.
