# onedrive-rc-core

Shared library behind [`onedrive-rcd`](../daemon/README.md) and
[`onedrive-rc`](../cli/README.md) — not a binary, not used directly. One
crate so the two never disagree about the wire format or what a valid config
looks like.

| Module | Owns |
| --- | --- |
| `config` | `config.toml`'s schema, full validation (every duration/size/enum, at startup *and* on reload), atomic save |
| `ipc` | the daemon's control protocol (newline-delimited JSON over a Unix socket) and `StatusReport`, the single source of truth `status`, `doctor`, and the dashboard all read from |
| `state` | the SQLite schema (WAL): the offline index, pins, conflicts, the outbox, events |
| `paths` | every on-disk location (`~/OneDrive`, the cache dir, the state DB, the log dir, …), derived once from `$HOME`/XDG so a test `Paths` never touches the real ones |
| `units` | `Dur`/`Size` — the `1h30m`/`25G`-style parsers `config` uses for every duration and size field |
| `code` | stable error codes, each with a one-line fix hint, so a log line is never just prose |
| `log` | the JSONL record format and its rendering/filtering, shared between the daemon (writer) and the CLI (reader) |
| `unit` | the systemd user unit template |

```sh
cargo test -p onedrive-rc-core
```
