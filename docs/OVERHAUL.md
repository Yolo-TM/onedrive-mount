# onedrive-mount → `onedrive` : full overhaul plan

Status: **approved 2026-10-08.** P0 and P1 are implemented — see §14 and §15.
P2 is implemented and verified live against the real `onedrive-timm` remote —
see §17. P3 (offline index, odfs overlay) is implemented and measured — see §18.
Target version: `0.3.0` (breaking; new config, new binaries, new state format).

Binaries: **`onedrived`** (daemon) and **`onedrive-tui`** (CLI + TUI, one binary).
The old `onedrive-mount` (GUI) and `onedrive-mountd` are removed.

---

## 1. Context

The project exists to give one Linux laptop reliable access to a personal OneDrive. In its
current shape (`v0.2.8`, running here as pid 1087102) it does not deliver that:

- **It has been actively destroying data.** The KeePass rule in the live config is
  `sync_strategy = "mirror_down"`, which the daemon implements as
  `rclone sync onedrive-timm:Dokumente/Keepass /data/falcon/onedrive-keepass`
  every 15 minutes — an unconditional *remote wins, delete anything else* operation.
  The journal shows `mirror_down: discarding local changes` 96 times a day.
  Even the `bidirectional` strategy is unsafe (§2).
- **There is no offline story.** `--vfs-cache-mode full` caches file *contents* but
  directory metadata lives only in rclone's in-memory `--dir-cache-time` (15m). With no
  network, `~/onedrive-timm` returns I/O errors and nothing is browsable.
- **Failures are silent.** `notify_conflicts()` exists in `src/bin/daemon/rclone.rs:143`
  and is `#[allow(dead_code)]` — never called from anywhere. Sync failures land in a TOML
  file nobody reads. Nothing watches for sleep, resume, lock, or network changes.
- **Logs are unusable.** Two files, two timestamp formats, one shared level knob. The rclone
  log here is **1.4 GB** and has no rotation. Errors are raw rclone JSON blobs embedded in
  Rust `tracing` prose.
- **The GUI is the wrong tool.** 2,251 LOC of immediate-mode egui that re-reads the whole
  5.8 MB daemon log *every frame*, `stat()`s every entry of a FUSE directory every frame,
  and forks two `systemctl` processes every 2 seconds on the UI thread.

**Intended outcome:** a single small background daemon that (a) never loses a byte,
(b) keeps the whole tree browsable offline, (c) tells you loudly when something is wrong,
and (d) survives reboots, suspend, lock and network loss without supervision.

### Decisions taken during review

| Question | Decision |
| --- | --- |
| Offline browsing | Our own read-mostly FUSE overlay; cached/cloud-only state visible **in the file manager**, not CLI-only |
| UI | `onedrive-tui` — CLI + ratatui dashboard in one binary. No GUI, no tray. |
| Scope | Full rewrite: SQLite state, hash-based `rclone bisync`, D-Bus lifecycle |
| Conflicts | Both versions always kept; conflict copies live **next to the database in `$HOME`**, and are **also uploaded to OneDrive** |
| Conflict alerting | Standard Plasma notification that **persists for as long as the conflict exists** (§4.3) |
| Version history | No sibling folder. A minimal snapshot in `~/onedrive-keepass/.versions/`, excluded from sync (§4.2) |
| Offline writes | **Queued** in a visible staging folder and replayed on reconnect |
| Dolphin icon emblems | **Dropped.** `user.xdg.tags` gives a Tags column in Dolphin; no KIO plugin. |
| Mount point | **`~/OneDrive`** (replaces `~/onedrive-timm`) |
| Binary names | **`onedrived`** + **`onedrive-tui`** |
| Stopgap | None — build the replacement, then cut over and delete the old daemon |

---

## 2. Diagnosis: why the KeePass data loss happened

Two independent mechanisms, both reachable with the shipped defaults. This is the evidence
that drives the whole design, so it is worth stating precisely.

**(a) `bidirectional` overwrites the remote unconditionally.**
`src/bin/daemon/sync_executor.rs:82-91` runs, in order:

```rust
let blocked = rename_conflicts(...).await?;
if blocked { return Ok(stats); }
stats.add(&run_copy(&local_str, remote, &rule.patterns, CopyMode::Normal, true).await?);  // push
stats.add(&run_copy(remote, &local_str, &rule.patterns, CopyMode::Normal, false).await?); // pull
```

`CopyMode::Normal` adds neither `--update` nor `--ignore-existing`. So the push is a bare
`rclone copy <local> <remote>`: every file whose content differs is overwritten **local → remote,
regardless of which side is newer**. Direction is never chosen. The `local_changed` /
`remote_changed` computation at `:258-270` only answers *"is this a conflict?"*, and every one
of its `continue` branches falls straight through into that destructive push:

- empty baseline (first sync, rule renamed, corrupt TOML, failed save) → remote version destroyed
- path missing from baseline → remote version destroyed
- **only the remote changed** (you edited on your phone) → your remote edit destroyed, silently

**(b) The baseline cannot distinguish the two sides.**
`src/sync_baseline.rs:6` stores exactly one timestamp per path and no hash:

```rust
pub struct SyncBaseline { pub files: HashMap<String, DateTime<Utc>> }
```

Worse, `sync_executor.rs:410` does `if baseline.files.contains_key(name) { continue; }` while
scanning the remote — so for any file present on both sides the stored timestamp is the **local**
mtime and the remote's real `ModTime` is never recorded. `remote_changed` therefore means
*"OneDrive's ModTime differs from the local mtime by >2s"*, which for OneDrive
(`Precision: 1000000000` — 1 second) is a permanently sticky condition. Once sticky, your next
local edit satisfies `local_changed && remote_changed`, and
`tokio::fs::rename(&local_path, &conflict_path)` at `:298` moves `Timm.kdbx` aside — then the
`return` at `:85` means the remote version is **never pulled into the canonical name**. The file
simply vanishes. The GUI tooltip at `views/sync_rule.rs:60` claims the opposite ("No data is lost").

Supporting defects found, all of which the new design must structurally prevent:

| # | Defect | Location |
| --- | --- | --- |
| 1 | Crashed mount is **never retried** — the entry holding backoff state is removed *before* the backoff check | `mount_manager.rs:124-135` |
| 2 | Unbounded `metadata()` await inside `select!` ⇒ a hung FUSE mount **freezes the whole daemon** | `mount_manager.rs:160`, `app.rs:68` |
| 3 | `rclone check` exit status never inspected ⇒ auth failure reads as "no conflicts" ⇒ destructive push | `sync_executor.rs:201-216` |
| 4 | `fetch_remote_meta` returns `Utc::now()` on **every** failure ⇒ manufactures false conflicts | `sync_executor.rs:443-456` |
| 5 | `kill_on_drop(false)` ⇒ a timed-out `rclone copy` keeps running while the retry starts another in the opposite direction | `sync_executor.rs:37-42` |
| 6 | No hashes anywhere — `grep -rni "quickxor\|checksum"` over `src/` matches nothing but `HashMap` | — |
| 7 | `After=/Wants=network-online.target` in a **user** unit is inert; daemon starts before the network | `gui/systemd.rs:26-28` |
| 8 | `Restart=on-failure` only; no watchdog; no sleep/resume/network hooks anywhere | `gui/systemd.rs:34` |
| 9 | `PidLock` is a read-then-write with no `flock` and no identity check ⇒ PID reuse wedges startup forever | `pid_lock.rs:14-19` |
| 10 | `Config::validate()` is **never called at startup**, and covers only 5 fields | `config.rs:109`, `main.rs:34-40` |
| 11 | rclone log never rotated, fixed filename shared by all remotes | `main.rs:96-112`, `rclone.rs:71-76` |
| 12 | Status file written only on change ⇒ no heartbeat, cannot tell "idle" from "wedged" | `status_writer.rs:16` |

---

## 3. Target architecture

One daemon, one CLI. rclone stays as the transport — it is the right tool — but every
destructive invocation is replaced and every child's output is captured.

```
                        ~/.config/onedrive/config.toml
                                    │ (inotify, validated)
                                    ▼
 ┌─────────────────────────── onedrived ───────────────────────────┐
 │  supervisor      capped-backoff restart of every subsystem,     │
 │                  sd_notify READY/WATCHDOG, global per-remote    │
 │                  semaphore, kill_on_drop on every child         │
 │                                                                 │
 │  lifecycle       D-Bus: logind PrepareForSleep + delay inhibit, │
 │                  NetworkManager StateChanged, KDE screensaver   │
 │                  → state machine Online ⇄ Offline / Suspending  │
 │                                                                 │
 │  mount           rclone mount (child) → backing mount, hidden   │
 │                  timeout-guarded liveness probe, rc API control │
 │                                                                 │
 │  odfs            our FUSE → ~/OneDrive  (ALWAYS mounted)        │
 │                  metadata from SQLite, data via backing mount   │
 │                  or VFS cache, xattr state, pin passthrough     │
 │                                                                 │
 │  indexer         rclone lsjson --hash → SQLite, plus            │
 │                  opportunistic updates from odfs readdir        │
 │                                                                 │
 │  pins            rclone bisync per pinned folder → real dirs    │
 │                  in $HOME; checksum compare, both versions      │
 │                  always kept, pre-run snapshot                  │
 │                                                                 │
 │  outbox          offline writes staged on disk, replayed on      │
 │                  reconnect only after verifying the remote      │
 │                  still matches the recorded baseline            │
 │                                                                 │
 │  alerts          org.freedesktop.Notifications (= Plasma),      │
 │                  critical + resident, re-asserted while open    │
 │                                                                 │
 │  log             ONE JSONL stream; rclone's --use-json-log       │
 │                  stdout parsed and merged in; daily rotation    │
 │                                                                 │
 │  ipc             unix socket ← onedrive-tui                     │
 └─────────────────────────────────────────────────────────────────┘
```

### Paths

| Path | Contents |
| --- | --- |
| `~/OneDrive` | **odfs** — the user-facing mount, always present |
| `~/.cache/onedrive/<remote>/mount` | backing `rclone mount`, not for direct use |
| `~/.cache/onedrive/<remote>/vfs` + `vfsMeta` | rclone VFS cache (`--cache-dir`), mirrors remote paths 1:1 — **verified** |
| `~/onedrive-keepass/` | pinned folder — a **real** directory, always complete offline |
| `~/onedrive-keepass/Timm.conflict-remote-<ts>.kdbx` | conflict copies, **next to the database** |
| `~/onedrive-keepass/.versions/<ts>/` | pre-run snapshots, **excluded from sync** (§4.2) |
| `~/OneDrive.pending/` | offline write staging — visible, inspectable in Dolphin |
| `~/.local/state/onedrive/state.db` | SQLite (WAL): index, pins, conflicts, outbox, events |
| `~/.local/state/onedrive/log/onedrived.jsonl` | the single log, daily rotation, 7-day retention |
| `~/.local/state/onedrive/bisync/<remote>/<pin>/` | bisync `--workdir` listings |
| `$XDG_RUNTIME_DIR/onedrive/onedrived.sock` | IPC |

Existing paths are kept where they already work: `~/onedrive-keepass` does not move, and no new
top-level directory is added to `$HOME` except the (optional) `~/OneDrive.pending`.

---

## 4. The data-safety contract

This is the part that must be right. Three rules, enforced structurally rather than by care.

> **R1 — No operation may make the only copy of any byte unreachable.**
> Every conflict keeps both versions. Anything a run could overwrite or delete is snapshotted
> locally first. Nothing is ever deleted outright.
>
> **R2 — Content identity is a hash, never a timestamp.**
> OneDrive exposes `quickxor` (verified: `rclone backend features` → `"Hashes": ["quickxor"]`).
> Timestamps may only *rank* two versions that are already known to differ; they may never
> decide *whether* they differ.
>
> **R3 — Ambiguity aborts the cycle; it never picks a side.**
> A failed listing, a non-zero exit, a missing access sentinel, an unparseable hash — all
> abort and alert. No code path may treat "I don't know" as "no change".

### 4.1 Pinned folders: `rclone bisync`

Replaces `sync_executor.rs` entirely. One invocation per pin, Path1 = local, Path2 = remote:

```sh
rclone bisync ~/onedrive-keepass onedrive-timm:Dokumente/Keepass \
  --compare size,modtime,checksum \      # R2: quickxor decides difference
  --modify-window 1s \                   # OneDrive ModTime precision is exactly 1s
  --ignore-case-sync \                   # OneDrive is CaseInsensitive: true
  --conflict-resolve newer \             # ranks the winner for the canonical name
  --conflict-loser pathname \            # R1: loser is KEPT, renamed, on BOTH sides
  --conflict-suffix conflict-local,conflict-remote \
  --check-access --check-filename .onedrive-check \   # R3: aborts if a side looks empty
  --max-delete 50 \                      # a PERCENTAGE, not a count — see below
  --resilient --recover --max-lock 2m \  # survives interruption without --resync
  --workdir ~/.local/state/onedrive/bisync/onedrive-timm/Keepass \
  --filters-file <generated> \
  --timeout 30s --contimeout 10s --low-level-retries 5 \
  --use-json-log --log-level INFO            # emitted on stderr; see §7
```

Why this is safe where the old code was not:

- `--compare …,checksum` means a file is only considered changed when its **quickxor actually
  differs**. Clock skew and OneDrive's second-granularity ModTime stop being able to cause
  spurious changes — defect (b) and findings 4 and 6 all disappear.
- `--conflict-resolve newer` + `--conflict-loser pathname` means a true conflict normally
  produces `Timm.kdbx` (the newer content) *and* a suffixed copy of the loser.

> **But the canonical name is not guaranteed by rclone, so we guarantee it ourselves.**
> Verified against rclone 1.75.1: when the two sides' mtimes are *exactly equal* and the
> content differs, `newer` cannot pick a winner, so bisync suffixes **both** files and
> leaves the canonical path **absent**:
>
> ```
> Timm.kdbx.conflict-local     = LOCAL-ENTRIES
> Timm.kdbx.conflict-remote    = REMOTE-ENTRIES
> Timm.kdbx                    -- does not exist
> ```
>
> No bytes are lost, but KeePassXC reports its database missing — which is the exact symptom
> that started this rewrite. With OneDrive's 1-second mtime precision and a 1s modify window,
> two edits inside the same second are enough to trigger it. §4.4 therefore makes a post-run
> reconciliation pass mandatory rather than relying on the flag.
- Conflict copies are regular files, so bisync propagates them — they appear on OneDrive too,
  as requested, and you can merge from any machine.
- `--conflict-suffix` is static, so the daemon runs a post-pass that renames any
  `*.conflict-local*` / `*.conflict-remote*` to include an RFC3339 timestamp before recording
  the conflict. Timestamped copies accumulate instead of overwriting each other.
- `--check-access` is the guard against the catastrophic case: if auth fails or the remote
  path resolves empty, the sentinel is missing and bisync **refuses to run** rather than
  mirroring an empty tree over your data.
- `--resilient --recover` means a `kill -9` mid-run does not require a manual `--resync`.
- First run detects an empty `--workdir` and uses `--resync --resync-mode newer`.

**Filters** (generated, written to the workdir) exclude editor noise so a half-written file is
never uploaded, and exclude our own snapshots:
`- .versions/**`, `- *.kdbx.lock`, `- *.lock`, `- *.tmp`, `- *.partial`,
`- .goutputstream*`, `- ~$*`, `- .~lock.*`, then the user's patterns.

> **The access sentinel must not be filtered.** An earlier draft of this list excluded
> `.onedrive-check`; verified against rclone 1.75.1, that makes `--check-access` unable to
> find its own sentinel and **every run aborts** with `Access test failed: Path1 count 0,
> Path2 count 0`. The sentinel is therefore synced like any other file, which is also what
> keeps it present on both sides.

**Pin triggers** — a KeePass save should be on OneDrive in seconds, not 15 minutes:
`interval` (default `5m`) **plus** debounced inotify on the local dir, **plus** on screen lock,
on suspend-prepare, on resume, and on network-up. More frequent syncing is itself a conflict
*reducer*.

### 4.2 Version snapshots — why a small one is still worth keeping

You are right that conflict copies make a *version history* unnecessary: when both sides changed,
both versions are on disk next to each other and a `.kdbx` merge recovers everything. Snapshots
are therefore **not** for keeping old copies of the database.

They exist for the one class of event a conflict copy does **not** cover — when bisync is
*correct* that only one side changed, so there is no conflict and no second copy:

- **a propagated delete.** You (or something else) remove `Timm.kdbx` on one side; bisync
  faithfully removes it on the other. No conflict, no copy, file gone from both sides.
- **a one-sided overwrite that was wrong.** A stale client, a restored-from-backup machine, or
  a sync.com-style "the remote changed" that you did not intend — the other side is replaced
  with no copy kept.

With `--max-delete` and `--check-access` these are unlikely, not impossible. So:

**Design:** drop rclone's `--backup-dir` entirely (it is forbidden from overlapping the synced
path, which is exactly where you want it). Instead the daemon takes its own snapshot
**immediately before each bisync run**, of only those files whose content has changed since the
last snapshot, into:

```
~/onedrive-keepass/.versions/2026-10-08T10-42-13Z/Timm.kdbx
```

`.versions/**` is in the filters file, so it is **never synced** — it stays local, inside the
folder you already have, with no sibling directory and no `.local` detour. Retention defaults to
14 days; for a 76 KB database that is a few hundred KB at most. One toggle turns it off:

```toml
[[remote.pin]]
versions           = true     # false ⇒ no snapshots at all
versions_retention = "14d"
versions_max_size  = "50M"    # skip snapshotting files larger than this
```

My recommendation is to leave it on — it is cheap and it is the only recovery path for a
propagated delete — but it is one line to disable and nothing else depends on it.

### 4.3 Conflicts: what the notification actually looks like

Standard **freedesktop desktop notifications** over D-Bus
(`org.freedesktop.Notifications`). On this machine that name is owned by `plasmashell`
(verified), so these are ordinary **Plasma notifications**: they pop up in the corner, land in
Plasma's notification history, and respect your Do Not Disturb settings. No custom UI, no tray
icon, no toolkit dependency — just `zbus`.

```
┌────────────────────────────────────────────────────┐
│  ⛔  OneDrive — conflict in Keepass                │
│                                                    │
│  Timm.kdbx changed on this laptop and on OneDrive. │
│  Both versions were kept:                          │
│    Timm.kdbx                      (newer, in use)  │
│    Timm.conflict-remote-          (other version)  │
│      20261008T1042.kdbx                            │
│  Merge them in KeePassXC, then delete the extra.   │
│                                                    │
│  [ Open folder ]   [ Details ]   [ Resolved ]      │
└────────────────────────────────────────────────────┘
```

Persistence is the part you asked for, and it is done properly rather than by spamming:

- `urgency = 2` (critical) and hint `resident = true` — Plasma does **not** auto-expire these,
  so it stays on screen until you act on it
- the notification id is stored in SQLite and **re-`Notify`d with the same id** every 30 minutes
  while the conflict row is unresolved, so dismissing it does not make the problem invisible —
  it comes back until it is actually fixed
- `CloseNotification` fires only when the conflict is genuinely resolved
- *Open folder* runs `xdg-open ~/onedrive-keepass`; *Details* opens `onedrive-tui conflicts`;
  *Resolved* marks it resolved after re-verifying only one version remains
- `onedrive-tui status` exits non-zero while any conflict is open, so it can also drive a shell
  prompt indicator

The same persistent treatment applies to: mount down > 2 min, pin not synced > 3 × interval,
auth expired, offline > 24 h, outbox non-empty > 24 h.

### 4.4 Reconciliation: the invariants we enforce ourselves

bisync does the transport. It does not promise the two properties that actually matter to
someone whose password database lives in a pinned folder, so a pass runs after every bisync
and enforces them.

**Invariant 1 — the canonical path always exists.**
If the canonical name is missing but suffixed copies are present (the mtime-tie case in §4.1),
the best copy is *copied* back to the canonical name. "Best" is: larger file, then newer mtime,
then local over remote — a `.kdbx` that has grown has strictly more entries in it, so size is a
better tiebreak than time here. The copy is a copy, not a move: the suffixed version stays
exactly where it is, so nothing depends on this pass having got the choice right.

**Invariant 2 — conflict copies are timestamped and keep their extension.**
rclone appends its suffix after the whole filename, so a conflict on `Timm.kdbx` produces
`Timm.kdbx.conflict-local` (verified). That is doubly unhelpful: the extension is gone, so the
file no longer opens by double-click, and the name is fixed, so the *next* conflict would
overwrite it. The pass renames:

```
Timm.kdbx.conflict-local   ->  Timm.conflict-local-2026-10-08T124530Z.kdbx
Timm.kdbx.conflict-remote  ->  Timm.conflict-remote-2026-10-08T124530Z.kdbx
```

Timestamped copies accumulate instead of overwriting each other, they sit next to the database
as requested, and they still propagate to OneDrive on the next run because they are ordinary
files that the filters do not exclude.

**Ordering.** The pass runs on the local side only, and its renames are picked up and
propagated by the *next* bisync run. It never touches the remote directly, so there is no
window in which the two sides are being modified by two different things.

**What it refuses to do.** It never deletes, never overwrites an existing canonical file, and
never merges. If it cannot establish the invariants it records a conflict and alerts, because a
pass whose failure mode is "quietly do something clever" is not worth having.

### 4.5 Offline writes: the outbox

Queueing is the riskiest feature requested, so it gets the same contract.

1. Write arrives at odfs while offline, path not under a pin → odfs writes the file into
   `~/OneDrive.pending/<remote>/<path>` and inserts an `outbox` row recording the **baseline**
   from the index: `(size, quickxor, mtime)` of the remote version the user started from.
2. Reads of that path while offline return the staged content, so you see your own edit.
3. On reconnect, per queued file: `rclone lsjson --no-traverse --hash <remote>:<path>`.
   - Remote still matches the baseline → `rclone copyto --checksum` the staged file up. Done.
   - Remote has changed → **the canonical remote file is not touched.** The staged file is
     uploaded as `<name>.conflict-local-<ts>.<ext>`, a conflict row is created, and a persistent
     notification fires. Same contract as a pin conflict.
   - Listing fails → leave queued, retry with backoff (R3).
4. Structural operations offline (`unlink`, `rename`, `mkdir`, `rmdir`) are **not** queued —
   they return `EROFS` with a notification. Replaying structural ops against a tree that moved
   underneath you is exactly how mass deletions happen.
5. The outbox is a visible folder plus `onedrive-tui outbox ls|retry|discard`. Nothing is hidden.

---

## 5. odfs — the offline overlay

A read-mostly FUSE filesystem (crate `fuser`) running as threads inside `onedrived`. It is what
makes `~/OneDrive` browsable with no network.

| Operation | Online, backing mount healthy | Offline or mount down |
| --- | --- | --- |
| `lookup` `getattr` `readdir` | passthrough to backing mount, **and write results into SQLite** | served from SQLite index |
| `open` + `read` | passthrough | VFS cache file if complete → serve; in outbox → serve staged; else `ENETDOWN` |
| `write` `create` | passthrough | stage into outbox (§4.5) |
| `unlink` `rename` `mkdir` `rmdir` | passthrough | `EROFS` + notification |
| path under a pin's `remote_path` | routed to the **local pinned dir** | same — pins are real dirs, always complete |
| `getxattr` | synthesised (below) | synthesised |

Three consequences worth calling out:

- **Browsing warms the index for free.** Every online `readdir` updates SQLite, so the paths you
  actually use are always fresh offline, with no extra API calls.
- **"Recently used files offline" needs no new machinery.** `--vfs-cache-mode full` already
  persists contents to `~/.cache/onedrive/<remote>/vfs/<path>`; raising `--vfs-cache-max-age` to
  `30d` is what makes them *stay*. odfs just has to be willing to read them when the mount is
  down. Completeness is checked against `vfsMeta/<path>`, with a conservative fallback of
  "cache file size == indexed size" if that format cannot be parsed.
- **Pins appear in both places.** `~/OneDrive/Dokumente/Keepass` and `~/onedrive-keepass` show
  the same bytes, because odfs routes that subtree to the local dir.

### Showing cached vs cloud-only in the file manager

odfs synthesises extended attributes on every node:

```
user.onedrive.state   = pinned | cached | partial | pending | cloud
user.onedrive.size    = <remote size>
user.onedrive.quickxor= <hash>
user.xdg.tags         = onedrive-cached   (mirror of state)
```

`user.xdg.tags` is the pragmatic trick: Dolphin surfaces it in the Information panel and as a
sortable **Tags** column, so cached-vs-cloud is visible in the file browser with no plugin.
Per-icon emblem overlays would need a separate KIO/Qt plugin — **dropped from scope**, as agreed.
`onedrive-tui ls` and the TUI show the same state in the terminal.

**Risk and fallback:** `~/OneDrive` → backing mount is two FUSE hops, which costs latency. The
read path is a cached-fd passthrough with a worker pool and per-request timeouts, and Phase 3
ends with a measured `find`/`fio` comparison against the bare rclone mount. If the overhead is
not acceptable, `[remote.overlay] enabled = false` exposes the rclone mount directly and you
keep the index, CLI, pins and outbox. The design does not depend on odfs succeeding.

---

## 6. Lifecycle: why it will actually survive a reboot

All four signals are confirmed available on this machine.

| Event | Source | Response |
| --- | --- | --- |
| suspend imminent | logind `PrepareForSleep(true)`, holding an `Inhibit("sleep","delay")` | mark `Suspending`, stop issuing new rclone ops, checkpoint SQLite, release inhibitor — **within the 5 s budget** (`InhibitDelayMaxUSec = 5000000`; a full sync is *not* attempted here) |
| resume | `PrepareForSleep(false)` | probe mount **with a timeout**; `fusermount3 -u` + restart if stale; re-probe network; then `vfs/refresh`, pin sync, index refresh |
| network up/down | NetworkManager `StateChanged` + `Connectivity` | `Offline` ⇒ **zero rclone network calls** (this alone removes most log noise and all retry storms). `Online` ⇒ immediate pin sync + outbox replay |
| screen lock | `org.freedesktop.ScreenSaver` / `org.kde.screensaver` | opportunistic pin sync, so your KeePass edits are pushed before you walk away |
| daemon wedged | systemd `WatchdogSec=60` + `sd_notify(WATCHDOG=1)` | systemd kills and restarts it — the fix for finding 2 |
| mount crashed | supervisor, backoff state held **in the supervisor** | capped jittered backoff `2,5,15,60,300 s`, retried **forever** — the fix for finding 1 |

systemd user unit, shipped by the Nix module instead of being written by a GUI button:

```ini
[Unit]
Description=OneDrive daemon
# no network-online.target: it does not exist in the user manager (finding 7).
# Readiness is decided at runtime from NetworkManager over D-Bus.

[Service]
Type=notify
NotifyAccess=main
ExecStart=%h/.nix-profile/bin/onedrived
ExecStopPost=-/run/wrappers/bin/fusermount3 -u %h/OneDrive
Restart=always
RestartSec=2
StartLimitIntervalSec=0
WatchdogSec=60
Environment=RUST_BACKTRACE=1

[Install]
WantedBy=default.target
```

`loginctl enable-linger` is currently **`Linger=no`**, which means the daemon dies at logout.
`onedrive-tui setup` and the Nix module both set it.

`PidLock` is replaced by a real `flock` on the state dir plus a `/proc/<pid>/comm` identity
check, so a recycled PID can no longer wedge startup forever (finding 9).

---

## 7. Logging you can read

One stream. `~/.local/state/onedrive/log/onedrived.jsonl`, daily rotation, 7-day **and** 200 MB
retention enforced by a pruning task — not a one-shot truncate at startup.

rclone children are spawned with `--use-json-log --log-level <rclone_log_level>` and **no**
`--log-file`. Verified against rclone 1.75.1: `--log-file -` does *not* mean stdout — it creates a
file literally named `-` in the working directory. With the flag omitted, rclone writes its JSON
log to **stderr**, which is what we capture.

The daemon reads that stderr line by line, parses the JSON
(`{"time","level","msg","object","objectType","source"}`), and re-emits each entry into its own
log with `source=rclone remote=… op=… pin=…` fields at a mapped level. Consequences:

- no second log file, no second timestamp format, no 1.4 GB surprise
- `rclone_log_level` is a **separate** config key (default `notice`) from the daemon's own
  `log_level` — debugging a pin no longer turns on full VFS tracing (finding 11)
- every remote is identifiable in every line

Errors carry a **stable code and a one-line fix hint**, which is the actual answer to "logs are
not easily understandable":

```
$ onedrive-tui log --since 1h --level warn
10:40:43  WARN   pin/Keepass   E_OFFLINE       deferred: no network (retry on reconnect)
10:41:02  ERROR  mount/timm    E_AUTH_EXPIRED  OneDrive token rejected
                                               fix: onedrive-tui setup --reauth onedrive-timm
10:41:02  INFO   mount/timm    remount scheduled in 5s (attempt 1)
```

Codes: `E_OFFLINE`, `E_AUTH_EXPIRED`, `E_MOUNT_STALE`, `E_THROTTLED`, `E_CONFLICT`,
`E_CHECK_ACCESS`, `E_DISK_FULL`, `E_OUTBOX_STALE`, `E_INDEX_STALE`.
`onedrive-tui doctor` runs the checks proactively and prints the same hints.

---

## 8. Interface: `onedrive-tui`

One binary, subcommands for scripting and a dashboard for looking.

```
onedrive-tui                        no args ⇒ open the TUI dashboard
onedrive-tui setup                  guided first run: rclone OAuth → mount point → pins
                                    → unit install → enable-linger → first index crawl
onedrive-tui status [--json]        one screen; exit 0 healthy / 1 degraded / 2 broken
onedrive-tui pin add <remote-path> [--local <dir>] | rm | ls | sync [<name>]
onedrive-tui conflicts [ls] | resolve <id> --keep local|remote|both
onedrive-tui outbox ls | retry | discard <id>
onedrive-tui ls <path> | find <pattern>    offline, straight from the index
onedrive-tui index refresh [--full]
onedrive-tui log [-f] [--since 1h] [--level warn] [--remote X] [--pin X]
onedrive-tui doctor
onedrive-tui service install | uninstall | restart ; onedrive-tui mount remount
```

```
$ onedrive-tui status
net     online
mount   ~/OneDrive            ● mounted      up 9d 4h
index   412,330 files         ● fresh        crawled 4m ago
pin     Keepass               ● in sync      2m ago
outbox  empty
log     ~/.local/state/onedrive/log/onedrived.jsonl   12 MB
```

TUI panes (`Tab` to switch, `r` refresh, `Enter` resolve, `q` quit): Overview · Pins · Conflicts ·
Outbox · Log tail. It reads over the IPC socket; if the daemon is down it falls back to opening
SQLite read-only, so it can still tell you *that* the daemon is down and when it last ran.

`onedrive-tui setup` ports the one genuinely good piece of the old GUI:
`src/bin/gui/rclone_config_wizard.rs` drives rclone's own non-interactive state machine
(`rclone config create --non-interactive`, then `config update --continue --state … --result …`)
and auto-answers `config_is_local=true` so rclone hosts the OAuth redirect itself. That logic is
reused with text prompts instead of egui widgets, and with the three bugs fixed:
`kill_on_drop(true)`, an overall timeout, and cancel actually killing the child that is holding
port 53682.

Critically, it is **one command that finishes the job**. Today, completing OAuth is step 1 of 4 —
you then have to find "Add to config", then "Save" in the bottom-right status bar, then "Install
service", also a `small_button` in the bottom-right status bar. That is why creation is hard.

---

## 9. State and config

SQLite (`rusqlite`, bundled, WAL). Replaces `status.toml`, the baseline TOMLs, and
`conflict-resolutions.toml`.

```sql
CREATE TABLE node (                       -- the offline index
  path TEXT PRIMARY KEY, path_ci TEXT NOT NULL, parent TEXT NOT NULL,
  name TEXT NOT NULL, is_dir INTEGER NOT NULL,
  size INTEGER NOT NULL, mtime INTEGER NOT NULL,   -- unix seconds
  quickxor TEXT, seen_gen INTEGER NOT NULL);
CREATE INDEX node_parent ON node(parent);
CREATE INDEX node_ci     ON node(path_ci);

CREATE TABLE access  (path TEXT PRIMARY KEY, last_read INTEGER, reads INTEGER);
CREATE TABLE pin     (name TEXT PRIMARY KEY, remote TEXT, remote_path TEXT, local_path TEXT,
                      state TEXT, last_ok INTEGER, last_err TEXT, last_err_code TEXT);
CREATE TABLE conflict(id INTEGER PRIMARY KEY, kind TEXT, remote TEXT, path TEXT,
                      winner TEXT, loser_copy TEXT, snapshot_dir TEXT,
                      detected_at INTEGER, resolved_at INTEGER, notif_id INTEGER);
CREATE TABLE outbox  (id INTEGER PRIMARY KEY, remote TEXT, path TEXT, staged_path TEXT,
                      base_size INTEGER, base_quickxor TEXT, base_mtime INTEGER,
                      queued_at INTEGER, state TEXT, attempts INTEGER, last_err TEXT);
CREATE TABLE event   (ts INTEGER, level TEXT, code TEXT, subject TEXT, msg TEXT);
CREATE TABLE meta    (k TEXT PRIMARY KEY, v TEXT);   -- heartbeat, crawl gen, schema version
```

`meta.heartbeat` is written every 30 s, so "healthy and idle" is finally distinguishable from
"wedged" (finding 12).

Config — `~/.config/onedrive/config.toml`, `#[serde(deny_unknown_fields)]`, **validated at
startup and on reload** (finding 10), with a real duration parser that accepts `1h30m` and `30d`
(the current `parse_interval_secs` in `defaults.rs:61` rejects both):

```toml
[daemon]
log_level          = "info"
rclone_log_level   = "notice"     # decoupled — see §7
log_retention_days = 7
log_retention_size = "200M"

[[remote]]
name        = "onedrive-timm"
mount_point = "~/OneDrive"
enabled     = true

[remote.mount]
cache_dir                = "~/.cache/onedrive/onedrive-timm"
vfs_cache_max_age        = "30d"      # was 72h — this is what keeps recent files offline
vfs_cache_max_size       = "25G"
vfs_cache_min_free_space = "10G"
dir_cache_time           = "72h"      # safe now: poll_interval + our index back it up
poll_interval            = "30s"      # OneDrive has ChangeNotify: true
transfers                = 4          # was 8
buffer_size              = "8M"
vfs_read_chunk_size      = "32M"

[remote.overlay]
enabled = true                        # false ⇒ expose the rclone mount directly (§5 fallback)

[remote.index]
full_crawl_interval = "24h"
opportunistic       = true
tpslimit            = 10              # stay under OneDrive throttling

[remote.offline]
writes     = "queue"                  # queue | reject | readonly
outbox_dir = "~/OneDrive.pending"

[[remote.pin]]
name                   = "Keepass"
remote_path            = "Dokumente/Keepass"
local_path             = "~/onedrive-keepass"
interval               = "5m"
sync_on_change         = true         # debounced inotify
conflict               = "keep-both"
upload_conflict_copies = true
max_delete_percent     = 50
versions               = true         # ~/onedrive-keepass/.versions/, never synced (§4.2)
versions_retention     = "14d"
versions_max_size      = "50M"
```

Validation additions beyond the current five checks: `vfs_cache_mode` against rclone's enum,
every duration and size string, `transfers` range, duplicate remote/pin names, overlapping mount
points, and the important one — **a pin's `local_path` must not live inside any mount point**
(today nothing stops that, and it makes "local" and "remote" the same storage).

---

## 10. What is reused, what is deleted

**Reused (do not rewrite):**

| From | Why |
| --- | --- |
| `src/bin/gui/rclone_config_wizard.rs` | the rclone `--non-interactive` / `--continue` state-machine driving is correct; port to `onedrive-tui setup` |
| `src/paths.rs::expand_tilde` | keep, extend with XDG state dir helpers |
| `src/config.rs::save` tmp-write + `rename` | the atomic-save pattern is right; keep it |
| `src/bin/daemon/config_watcher.rs` | inotify-on-parent-dir + 200 ms debounce handles atomic renames correctly |
| `src/bin/daemon/rclone.rs::sanitize_flag` | keep the intent; replace with a typed flag builder (no shell is involved, so reject bad *values*, not metacharacters) |
| `src/bin/daemon/signal.rs` | SIGTERM/SIGINT handling |
| `.github/workflows/`, `flake.nix` structure | packaging stays; the module gains the systemd unit it never had |

**Deleted:**

`src/bin/gui/` (all 19 files, 2,251 LOC) · `sync_executor.rs` · `sync_scheduler.rs` ·
`resolution_executor.rs` · `resolution_watcher.rs` · `src/sync_baseline.rs` · `src/resolution.rs` ·
`src/conflict.rs` (the `SyncStrategy` enum — `mirror_down`/`mirror_up`/`newest_wins` all go away;
there is one safe mode) · `src/status.rs` (TOML status → SQLite + IPC) · `src/pid_lock.rs`
(→ `flock`) · deps `eframe`, `resvg`, `open`, and the `accesskit`/`x11`/`glow` graph behind them.

**New deps:** `rusqlite` (bundled) · `fuser` · `zbus` · `ratatui` + `crossterm` · `clap` ·
`humantime`. Dropping the egui graph roughly offsets these; `Cargo.lock` should shrink from 473
crates.

---

## 11. Phases

Each phase is independently shippable and testable. The old daemon keeps running untouched until
Phase 6, so there is no window where nothing works.

| # | Phase | Delivers |
| --- | --- | --- |
| **P0** | Skeleton | workspace layout, config + full validation, SQLite + migrations, JSONL logging + rotation, IPC socket, `Type=notify` + watchdog, `onedrive-tui status` / `doctor`. No data path yet. |
| **P1** | Mount supervisor | `rclone mount` child with the tuned flag set, rc API, timeout-guarded liveness probe, supervisor-held backoff (fixes findings 1, 2, 5), rclone JSON log ingestion, D-Bus lifecycle (sleep / resume / network / lock). |
| **P2** | **Pins — the safety milestone** | `rclone bisync` driver, conflict detection + timestamped copies, pre-run snapshots, persistent Plasma notifications, `onedrive-tui pin` / `conflicts`. **Does not land until the §12 safety matrix is green.** KeePass moves here and the old rule is deleted. |
| **P3** | Index + odfs | `lsjson --hash` crawl, opportunistic readdir updates, FUSE read path, offline listings, VFS-cache serving, xattrs, `onedrive-tui ls` / `find`. Ends with the latency measurement and the overlay go/no-go. |
| **P4** | Outbox | offline write staging, baseline-verified replay, conflict path on divergence, `onedrive-tui outbox`. |
| **P5** | Interface | ratatui TUI, `onedrive-tui setup` end-to-end (OAuth → pins → unit → linger → first crawl). |
| **P6** | Cutover | delete `src/bin/gui/` and the old sync engine, Nix module ships the unit + linger, README rewrite, `0.3.0`. |

---

## 12. Verification

### The sync-safety matrix (gates P2)

The centrepiece. A harness creates a `local`-type rclone remote as a stand-in for OneDrive so the
whole matrix runs in CI without credentials, then asserts on **content hashes**, not file
existence. Every case asserts the same invariant: *no byte of either version is unrecoverable.*

| Case | Expected |
| --- | --- |
| local-only change | propagates up; remote content byte-identical |
| **remote-only change** | propagates down; local content byte-identical *(this is the case that silently destroyed data — `sync_executor.rs:269` → unconditional push)* |
| both sides changed | canonical = newer; loser kept with timestamped suffix **on both sides**; conflict row created; notification asserted |
| **file deleted on one side** | delete propagates, **and** the pre-run snapshot in `.versions/<ts>/` holds the content (§4.2) |
| remote path empties (simulates auth failure) | `--check-access` aborts; **zero local deletions** |
| `kill -9` mid-bisync | next run with `--recover` completes; no loss; no manual `--resync` needed |
| clock skew ±5 min on one side | no data loss — checksum comparison makes mtime non-decisive |
| KeePass save pattern (write temp → rename) ×20 rapid | no torn upload; lock/tmp files never uploaded |
| `.versions/` never leaves the machine | after any run, remote listing contains no `.versions` entry |
| offline → 3 local edits → online | last content wins locally, remote matches, snapshots hold the intermediates |
| offline local edit + remote also changed → online | remote canonical untouched; local staged up as `.conflict-local-<ts>`; conflict row + notification |
| empty/corrupt bisync workdir | treated as first run; `--resync-mode newer`; pre-run snapshot taken; nothing lost |

### Other tests

- **Unit:** duration parser (`1h30m`, `30d`), config validation (incl. pin-inside-mount), index SQL, `vfsMeta` completeness parsing + size fallback, outbox baseline comparison, conflict filename generation, snapshot retention pruning, rclone JSON log → event mapping.
- **odfs:** mount over a fake backing dir in a tempdir; assert `readdir` works with the backing dir **removed** (the offline case), cached reads work, offline writes land in the outbox, structural ops return `EROFS`, xattrs present and correct.
- **Notifications:** assert against a stub `org.freedesktop.Notifications` on a private D-Bus — correct urgency and `resident` hint, same id reused on re-assert, `CloseNotification` only on real resolution. Then one manual check that it renders as expected in Plasma.
- **Lifecycle** (`tests/manual/lifecycle.sh`, run on this laptop): after each of `systemctl suspend` / `nmcli networking off;on` / `kill -9 <rclone>` / `kill -9 <onedrived>` / lock+unlock / full reboot — assert `onedrive-tui status` returns healthy within 60 s, `~/OneDrive` lists, and the KeePass pin is in sync.
- **Offline acceptance:** `nmcli networking off`, then: `ls -R ~/OneDrive | wc -l` matches the index; a previously-read file opens; `~/onedrive-keepass/Timm.kdbx` opens in KeePassXC and saves; an edit outside a pin lands in `~/OneDrive.pending`; `nmcli networking on` → it uploads and the notification clears.
- **Resource budget**, measured after 24 h uptime: `onedrived` RSS **< 40 MB**, rclone child RSS **< 150 MB** (today: 119.9 MB with a **1 GB peak**), idle CPU ≈ 0 — the design is event-driven, with only a 30 s watchdog, a 5 m pin timer and a 24 h crawl; there is no 15 s loop `stat()`ing a network filesystem. Log dir **< 200 MB** (today: 1.4 GB).
- **Acceptance checklist** mapped back to the original complaints: offline browsing ✓, offline recent files ✓, KeePass never loses an entry ✓, persistent conflict alerts ✓, readable logs ✓, one-command setup ✓, survives reboot/sleep/lock/network loss ✓, no GUI ✓.

---

## 13. Open items

Decided: mount point is **`~/OneDrive`**; the daemon binary is **`onedrived`**. The existing
`~/onedrive-timm` mount is retired at cutover (P6) — nothing under it is user data, it is a
mount point, so there is nothing to migrate.

1. **`--conflict-resolve newer` ranks the winner by mtime.** Content *difference* is decided by
   quickxor (R2), so the worst case is "the wrong version holds the canonical name" — and the
   other one is right beside it. Note that `newer` does **not** by itself guarantee a winner
   exists (§4.1), which is why §4.4 reconciles the result rather than trusting the flag.
2. **bisync is flagged "advanced, use with care" by rclone upstream.** Mitigated by
   `--check-access`, `--max-delete`, pre-run snapshots, `--resilient --recover`, and the §12
   matrix. Worth knowing it is the one component where we rely on someone else's correctness.
3. **`versions = true` is my recommendation but your call.** It is the only recovery path for a
   propagated delete (§4.2). Set `false` and `.versions/` is never created.

---

## 14. Implementation status

| Phase | State |
| --- | --- |
| **P0** Skeleton | **landed** — see below |
| **P1** Mount supervisor | **landed** — see below |
| P2 Pins (the safety milestone) | not started |
| P3 Index + odfs | not started |
| P4 Outbox | not started |
| P5 TUI + guided setup | not started |
| P6 Cutover | not started |

### What P0 delivered

Workspace layout: `crates/core` (`onedrive-core`), `crates/daemon` (`onedrived`),
`crates/cli` (`onedrive-tui`). The legacy `onedrive-mount` package remains the
workspace root and still builds until P6.

| Area | Where | Notes |
| --- | --- | --- |
| Error codes + fix hints | `crates/core/src/code.rs` | `E_OFFLINE`, `E_AUTH_EXPIRED`, … each with a one-line fix |
| Duration/size parsing | `crates/core/src/units.rs` | accepts `1h30m` and `30d`, which the old parser rejected; invalid values fail at deserialize time |
| Paths | `crates/core/src/paths.rs` | XDG, state moved to `~/.local/state`; socket path falls back when `sun_path` would overflow |
| Config | `crates/core/src/config.rs` | `deny_unknown_fields`, validated at startup *and* reload, reports every problem at once |
| State | `crates/core/src/state.rs` | SQLite/WAL; replaces `status.toml`, the baseline TOMLs and `conflict-resolutions.toml` |
| Log | `crates/core/src/log.rs` | one JSONL stream, retention by age **and** size, local-time rendering |
| Control protocol | `crates/core/src/ipc.rs` | newline JSON over a unix socket; health roll-up drives the exit code |
| systemd unit | `crates/core/src/unit.rs` | `Type=notify`, `WatchdogSec=60`, `Restart=always` |
| Single instance | `crates/daemon/src/lock.rs` | `flock` — a stale lock is now impossible |
| Readiness/watchdog | `crates/daemon/src/sdnotify.rs` | `READY=1`, `STATUS=`, `WATCHDOG=1` |
| Supervisor loop | `crates/daemon/src/app.rs` | only timers and channels are awaited in the loop |
| CLI | `crates/cli/` | `status`, `doctor`, `log`, `service`, `reload`, `pin ls` |

### Old defects closed by P0

| Defect (§2) | Fix |
| --- | --- |
| 9 — PID-reuse could wedge startup forever | `flock`; the kernel releases it even on SIGKILL |
| 10 — `validate()` never ran at startup, covered 5 fields | runs at startup and reload, covers the whole schema |
| 11 — rclone log never rotated (1.4 GB observed) | retention by age and size, hourly + at startup; `rclone_log_level` decoupled |
| 12 — status written only on change, no heartbeat | 30 s heartbeat in SQLite; a stale heartbeat reads as broken |
| 8 (partly) — `Restart=on-failure`, no watchdog | `Restart=always`, `StartLimitIntervalSec=0`, `WatchdogSec=60` |
| 7 — inert `network-online.target` in a user unit | removed; readiness comes from D-Bus at runtime (P1) |
| Bad config caused a 10 s systemd restart loop | the daemon stays up, records the error, and waits for the file to be fixed |

### Verified on this machine

- `Type=notify` readiness accepted by systemd; `systemctl status` shows our
  health line via `STATUS=`.
- **Watchdog recovery:** `SIGSTOP` the daemon → systemd reports
  `Failed with result 'watchdog'` → restarts it → active again with a new PID.
  This is the recovery path the old daemon lacked when a hung FUSE mount froze
  its event loop.
- **No stale lock:** `SIGKILL` the daemon, leaving its PID in the lock file →
  the next start takes the lock normally.
- Invalid config written while running → daemon stays up, keeps the last good
  config, `status` exits 2 and lists every problem.
- Daemon stopped → `status` falls back to the database, labels itself
  not-live, and exits 2.
- Second instance → refused by name and PID, exit 3.
- 230 workspace tests plus 101 legacy tests pass; `cargo clippy --workspace
  --all-targets -- -D warnings` is clean.

### Not yet true

`status` reports every remote as **not mounted** and the index as **stale**,
because mounting is P1 and the index is P3. That is accurate, not a bug — but it
means `status` exits 1 (degraded) on a correctly configured system until P1/P3
land.

---

## 15. P1 — the mount supervisor

`~/OneDrive` is now a working OneDrive mount, supervised, with rclone's log
folded into ours.

Note that until P3 builds odfs there is nothing to put *in front* of rclone, so
rclone mounts straight at the user-facing path. `mount::rclone_mount_point` is
the single place that decides this, and `OVERLAY_IMPLEMENTED` flips it in P3.
That also means P1 is useful on its own: your files are really there, just
without offline listings.

| Area | Where | Notes |
| --- | --- | --- |
| rclone children | `crates/daemon/src/rclone.rs` | argv built from typed config; `kill_on_drop(true)`; stderr parsed and re-emitted into our stream; messages classified into codes |
| rc control channel | same | `--rc-addr=unix://…` + `rclone rc --unix-socket`, so no TCP port and no HTTP client dependency |
| Supervisor | `crates/daemon/src/mount.rs` | backoff held outside the mount entry; timeout-guarded probes with a single-probe-in-flight guard; graceful `SIGTERM` stop |
| Lifecycle | `crates/daemon/src/lifecycle.rs` | logind `PrepareForSleep` + delay inhibitor, NetworkManager `StateChanged`, screensaver `ActiveChanged` — each optional |
| Mount task | `crates/daemon/src/mountloop.rs` | owns the supervisor so nothing mount-related can block the main loop |

### Old defects closed by P1

| Defect (§2) | Fix |
| --- | --- |
| 1 — a mount that crashed twice was never retried | `Backoff` lives in a map keyed by remote, updated *before* the entry is removed; the ladder caps at 300s and retries forever |
| 2 — a hung FUSE mount froze the whole daemon | every probe is `tokio::time::timeout`-wrapped, runs on the mount task rather than the main loop, and a probe that has not returned is not re-issued |
| 5 — orphaned rclone children | `kill_on_drop(true)` on every child, plus an explicit `SIGTERM`-then-wait on stop |
| 3/4 (partly) — failures read as success | rclone's exit status *and* its log are both inspected; `vfs/refresh` checks the response body, because rclone reports "does not exist" inside an HTTP 200 |
| Mount config edits needed a daemon restart | the generated argv is compared against the running mount's, so an edited setting restarts just that mount |

### Found while building P1

Four things turned up that the plan had wrong or that only appear against a real
remote:

1. **`--log-file -` does not mean stdout.** It creates a file literally named
   `-`. With the flag omitted, rclone's JSON log goes to **stderr** — §7 is
   corrected.
2. **The rc socket overflowed `sun_path`.** Under the cache directory the
   address exceeded 108 bytes and rclone exited with nothing but `bind: invalid
   argument`. Sockets now share one length-guarded directory
   (`paths::choose_socket_dir`), keyed by uid *and* a digest of the runtime path
   so distinct roots cannot collide.
3. **`vfs/refresh` needs no `dir` parameter** to refresh the root. `dir=` and
   `dir=/` both return `{"": "file does not exist"}` inside a *successful*
   response, so getting it wrong failed silently.
4. **`SIGKILL`ing rclone leaves the mount point `ENOTCONN`.** `SIGTERM` makes it
   detach cleanly, so `stop` now terminates gracefully and only forces after 5s.

Two of our own design mistakes also surfaced, both caught by writing the
verification rather than by the compiler:

- An empty directory probed as *healthy*, so a mount point left behind by a dead
  rclone looked fine forever. Reading the directory cannot distinguish that from
  a working empty remote; the device-id check now decides.
- rclone's `info` was demoted to our `debug`, so setting `rclone_log_level =
  "info"` appeared to do nothing unless you also set `log_level = "debug"` — two
  knobs for one intent, which is the confusion this design exists to remove.
  Levels are now preserved.

### Verified against the real `onedrive-timm` remote

In a sandboxed `HOME`, reusing the existing `rclone.conf` read-only:

- mounts in ~15s; `ls ~/OneDrive` lists the real tree; `Dokumente/Keepass/Timm.kdbx`
  reads back its KDBX magic bytes.
- **`SIGKILL` the rclone child** → detected at 3s with `exited with signal: 9`,
  remounting by 6s, fully mounted at 21s. This is finding 1 demonstrated closed.
- **`fusermount3 -uz` under the daemon** → `E_MOUNT_STALE` logged with its fix
  hint, detached, remounted.
- **edit `rclone_log_level` in the config** → inotify reload, mount restarted
  with the new flag ~14s later, no daemon restart and no command needed.
- **`onedrive-tui mount remount`** → the exact command the `E_MOUNT_STALE` hint
  prints; remounts and comes back in ~14s.
- **rclone's log lines appear in our single stream** with `source=rclone` and
  `subject=mount/<remote>`, same format and timestamps as our own lines.
- **`SIGTERM` the daemon** → rclone stops cleanly, mount point detaches (not
  `ENOTCONN`), no orphaned children, socket directory removed.
- NetworkManager state is read over D-Bus at startup (`net online`), and the
  logind and screensaver subscriptions both attach.
- 20 MB daemon RSS, 75 MB rclone RSS (against 119.9 MB resident and a 1 GB peak
  for the old daemon). Log directory 16 KB after all of the above.

### Not yet verified live

**Suspend and resume.** The code path is there — logind `PrepareForSleep` with a
delay inhibitor, a 3s budget inside logind's 5s window, then a settle-and-reprobe
on resume — and the subscription is confirmed to attach, but actually suspending
the machine is too intrusive to do unprompted. To check it:

```sh
systemctl suspend          # then resume, and:
onedrive-tui log --since 10m | grep -E 'suspend|resume'
onedrive-tui status
```

Expect `suspending` before, `resumed from suspend` after, and the mount healthy
again within ~30s.

### Known limitations

- Mount state in `status` can lag reality by up to the 15s reconcile interval.
  It self-corrects; the old design could be wrong indefinitely.
- `status` still reports the index as stale, because there is no index until P3.
  A correctly configured system therefore exits 1 (degraded), not 0.
- Offline reads work only for files already in the VFS cache, and directory
  listings still need the network. That is what P3 fixes.

---

## 16. P2 — bisync semantics, verified before building

Four plan assumptions were wrong, found by probing rclone 1.75.1 with two local
directories as Path1/Path2 (which is also how the safety matrix runs in CI: no
credentials, no network, same code paths).

| # | Assumption | Reality |
| --- | --- | --- |
| 1 | `- .onedrive-check` belongs in the filters | It makes `--check-access` unable to find its own sentinel, so **every run aborts** with `Access test failed: Path1 count 0, Path2 count 0`. The sentinel must be synced like any other file. |
| 2 | `--max-delete 10` is a ceiling of ten files | It is a **percentage**. The abort message reads `too many deletes (>10%, 20 of 21)`. On a one-file pin *any* delete is 100%, so a tight value means legitimate deletes never propagate. Now `max_delete_percent`, default 50 (rclone's own). |
| 3 | A conflict yields `Timm.conflict-remote-<ts>.kdbx` | rclone appends after the whole filename: `Timm.kdbx.conflict-local`. The extension is lost and the name is fixed, so a second conflict would overwrite the first. |
| 4 | `--conflict-resolve newer` always populates the canonical name | **On an exact mtime tie it suffixes both sides and leaves the canonical path absent.** No bytes are lost, but `Timm.kdbx` does not exist — KeePassXC reports its database missing, which is the symptom that started this rewrite. |

Finding 4 is why §4.4 exists. With OneDrive's 1-second mtime precision and a 1s
modify window, two edits in the same second are enough; relying on the flag would
have reproduced the original bug through a different mechanism.

### Confirmed working as the plan assumed

- **A remote-only change propagates down.** This is the case the old
  `sync_executor.rs` destroyed on every cycle, and it is now correct.
- `--check-access` aborts with exit 7 and **zero local deletions** when the
  sentinel is missing on one side.
- `--max-delete` aborts a mass deletion (exit 1, `Safety abort: too many
  deletes`) with all 20 remote files intact.
- `--resilient --recover` survives `kill -9` mid-run: the next run exits 0
  without needing `--resync`.
- `--compare size,modtime,checksum` ignores a pure mtime change.
- Conflict copies propagate to **both** sides, so a conflict can be merged from
  any machine.
- A first run with no prior listings fails loudly (exit 7, "cannot find prior
  Path1 or Path2 listings") rather than guessing — so the resync path is entered
  deliberately, not by accident.
- Success is `"Bisync successful"` in the log plus a `stats` object carrying
  `transfers`, `deletes`, `renames`, `errors` — enough to report without
  scraping prose.

### Three more findings, from building the matrix itself

The probe in the table above was not enough; writing the matrix surfaced three
further behaviours, each of which would have been a real defect.

**5. `--check-access` cannot bootstrap its own sentinel.** On a brand-new pin the
remote folder has no `.onedrive-check`, so the *first* run — the resync, the most
consequential one — aborts with `Path1 count 1, Path2 count 0`. The earlier probe
missed this because it pre-created the sentinel on both sides by hand. `prepare`
now copies the sentinel up before a resync, so the access check protects even the
first run rather than having to be disabled for it.

**6. bisync has a second, independent safety net** that the plan did not know
about: it refuses to sync *to* a side whose listing has become entirely empty,
before it even reaches the access check. In the "remote vanished" scenario this
is what actually fires. But the wording matters enormously:

| rclone says | Means | Right response |
| --- | --- | --- |
| `Empty **current** Path2 listing` | the live side is gone | **refuse** — this is the catastrophic case |
| `Empty **prior** Path1 listing` | *our own* stored baseline is damaged | **resync** — it is our state to rebuild |

Conflating the two would have been bad in both directions. Treating `prior` as a
vanished remote blocks syncing permanently behind an alarming and wrong message;
treating `current` as a damaged baseline would resync over a remote that had
genuinely emptied.

**7. `--modify-window 1s` stops `--conflict-resolve newer` from ranking
sub-second timestamps at all.** Two edits *2.5 seconds apart* on a local
filesystem are treated as a tie, so every conflict lands in the no-winner case:

| mtimes | window | winner? |
| --- | --- | --- |
| sub-second, 2.5s apart | 1s | **no** |
| sub-second, 2.5s apart | none | yes |
| whole-second, 60s apart | 1s | yes |

On real OneDrive this does not arise — its mtime precision is exactly 1 second,
so timestamps are whole seconds and ranking works. It arises only against a local
stand-in with finer precision. The production flags are therefore unchanged, and
the matrix sets whole-second mtimes explicitly to model what OneDrive reports;
`separate_mtimes` carries that reasoning. The genuine tie keeps its own case,
forced with equal timestamps.

Removing the window was considered and rejected: with `modtime` in `--compare`,
dropping it would let OneDrive's second-rounding mark unchanged files as changed
on every run.

### The gate: 15 cases, all green

`crates/daemon/tests/safety_matrix.rs` drives the real `bisync`, `reconcile` and
`snapshot` code against a local `alias` remote standing in for OneDrive — no
credentials, no network, same code paths. (This is why the daemon crate is now a
library as well as a binary: a matrix that exercised a lookalike would prove
nothing.) Assertions are on **content**, never on mere file existence; the old
engine's tests passed while it was destroying data partly because they checked
that a file was present rather than what was in it.

| Case | Asserts |
| --- | --- |
| local-only change | propagates up, no conflict |
| **remote-only change** | propagates down — the case the old engine destroyed every cycle |
| both sides changed | canonical holds the newer content; loser kept, timestamped, extension intact, on both sides |
| **equal mtimes** | the canonical database still exists, both versions recoverable, restore reported |
| propagated delete | delete propagates *and* the snapshot holds the content |
| remote emptied | refused, local side byte-for-byte untouched |
| mass deletion | refused, remote untouched |
| snapshots | `.versions/` never reaches the remote |
| interrupted run | recovers by itself, no manual `--resync` |
| missing baseline | resyncs without discarding the newer side |
| clock skew ±5 min | no invented changes, no conflict |
| KeePass save pattern ×20 | last complete save uploaded; no `.lock` or `.tmp` ever uploaded |
| repeated cycles | both sides stay byte-identical, no drift |
| offline edits then sync | latest wins, intermediates still in snapshots |
| successive conflicts | each gets its own name; earlier copies survive |

---

## 17. P2 complete: scheduler, resolution, and a live verification

The matrix in §16 proved the sync engine safe against a local stand-in. This
section is what was still missing to call P2 *done*, and the live run that
checked it against the real remote rather than only the stand-in.

### What P2 adds beyond the matrix

- **The pin scheduler.** `pins::Scheduler` turns the matrix's single invocations
  into a running system: an interval floor per pin, debounced inotify
  (`pinwatch.rs`) so a save reaches OneDrive in seconds rather than at the next
  tick, and sync triggers on screen-lock, suspend, resume and reconnect.
  `mountloop.rs` owns the `Supervisor` task and wires `Control::SyncPins` /
  `Control::ReloadPins` so `onedrive-tui pin sync` and a config reload reach it.
- **Persistent conflict notifications.** `notify.rs` sends a `critical` +
  `resident` D-Bus notification, keyed so a repeat detection replaces rather
  than stacks, and re-asserted every 30 minutes while the row stays open
  (`notify::REASSERT_INTERVAL`) — dismissing it does not make the problem
  invisible.
- **`onedrive-tui conflicts [ls] | resolve <id> --keep local|remote|both`** —
  the piece the plan's interface section (§8) specified but P2 had not yet
  built. `crates/daemon/src/resolve.rs` finds every copy of a conflicted file by
  parsing the side and timestamp back out of its name (conflicts can
  accumulate across repeated runs; the database only ever remembers the
  first), snapshots everything unconditionally before touching anything, then
  promotes or removes per the choice. `keep both` changes no files at all — it
  only closes the alert, which is the expected answer once a `.kdbx` has been
  merged by hand. A resolved row is not closed by the request handler; it is
  closed by the next `pins::resolve_finished` sweep observing that the
  canonical file exists and the extra copies are gone, so "resolved" always
  means "verified," not "asked for."
- **A deliberately *not*-reused IPC shape.** `Response::Conflicts` is a struct
  variant (`{ conflicts: Vec<ConflictInfo> }`), not a newtype wrapping a bare
  `Vec`. An internally-tagged enum (`#[serde(tag = "result")]`) serializes a
  newtype variant's content merged with the tag object; a JSON array has
  nowhere to put that tag. `Response::Status` only ever looked safe because
  `StatusReport` happens to be a struct. This broke the *first* live
  `onedrive-tui conflicts` call during verification (§ below) and is now a
  regression test (`ipc::tests::a_conflict_list_round_trips_empty_and_populated`).

### Live verification against the real remote

Run against `onedrive-timm` (not the matrix's local stand-in) with a disposable
pin (`_onedrive-p2-test`, not the real KeePass folder) in a sandboxed `$HOME`:

1. Daemon started, mounted `~/OneDrive`, pulled the seeded remote file into the
   pin on the first bisync cycle.
2. Local and remote edited independently (no shared clock); `onedrive-tui pin
   sync` triggered a cycle. rclone's own log showed it correctly picking the
   newer side, renaming the loser, and our reconciliation pass logging `kept
   both versions of a conflicting file`; the renamed, timestamped,
   extension-preserving copy then propagated to the remote on the next cycle,
   and the orphaned raw-named copy on the remote was cleaned up — all exactly
   as §4.1 describes.
3. `onedrive-tui conflicts` listed the conflict with both versions and the
   exact command to resolve it — this is what surfaced the serialization bug
   above, fixed, rebuilt, and reproduced clean.
4. A manual `busctl` `Notify` call with the same hint set confirmed the
   notification call succeeds and returns an id; a QML console warning in
   `plasmashell` at the same timestamp was confirmed unrelated by reproducing
   it with a bare `Notify` carrying none of our hints — cosmetic noise in this
   Plasma version, not a defect in `notify.rs`.
5. `onedrive-tui conflicts resolve <id> --keep remote` removed the local loser
   copy, left the discarded content recoverable under
   `.versions/<ts>-resolve/`, and queued a pin sync — rclone then deleted the
   corresponding file on the remote, and `onedrive-tui conflicts` returned to
   "no conflicts" once the sweep observed it.

No step required touching code not covered by the §16 matrix; the matrix's
guarantees held against the real service.

---

## 18. P3 complete: the offline index, odfs, and the overlay go/no-go

P3 delivers the thing the whole rewrite was triggered by: `~/OneDrive` stays
browsable, and recently-used files stay readable, with no network at all.

### What P3 adds

- **The offline index** (`crates/daemon/src/index.rs`, `onedrive_core::state`'s
  `node` table). A full crawl is one `rclone lsjson --hash -R` per remote,
  generation-stamped so that a deletion upstream — the one event a listing
  can't report directly — is detected by *absence*: anything not seen by the
  newest generation is pruned once the crawl finishes successfully. A crawl
  that fails changes nothing; the old index is still a better answer than no
  offline listing at all. Opportunistic updates (odfs's own live listings)
  stamp the *current* generation rather than a new one, so a partial listing
  can never be mistaken for a complete one and trigger pruning it has no
  business doing — this is the same R3 discipline (§4) applied to browsing
  instead of syncing. A hash recorded by a crawl is never overwritten by an
  opportunistic update's bare `stat()` (`COALESCE` in `Store::upsert_nodes`),
  so the one authoritative signal a crawl adds is never silently downgraded.
- **odfs** (`crates/daemon/src/odfs.rs`), a `fuser`-based (pure Rust, no
  libfuse — it talks to `/dev/fuse` directly via `fusermount3`, exactly how
  `rclone mount` already does, confirmed by `fuser` building and mounting with
  zero extra system dependencies on this machine) read-mostly filesystem
  mounted at the path the user actually browses. Every `lookup`/`getattr`/
  `readdir` tries the real thing first — the backing `rclone mount`, or a
  pin's local directory — and only falls back to the index when that fails;
  whatever the live attempt returns is folded straight back into the index.
  There is deliberately no separate "are we online" flag anywhere in the
  module: the live attempt *is* the online/offline test, which means the two
  can never disagree with each other the way a cached flag could. A path
  under a pin routes to the pin's real local directory unconditionally — pins
  are a plain directory, not a cache, so they are the ground truth for their
  own subtree even with the backing mount completely gone. Writes passthrough
  to the backing mount while it is reachable, exactly as a bare rclone mount
  already behaves; offline, structural writes refuse with `EROFS` rather than
  silently doing nothing or inventing a queue — P4's outbox is what turns that
  refusal into a queued write, and refusing is the honest placeholder until it
  exists. Extended attributes (`user.onedrive.state`, `user.xdg.tags`) give
  Dolphin's Tags column a cached-vs-cloud distinction with no KIO plugin.
- **The supervisor mounts it.** `OVERLAY_IMPLEMENTED` (mount.rs) is now `true`:
  rclone moves to the hidden backing path and odfs takes the user-facing one,
  for every remote with `overlay.enabled` (the default). odfs's lifecycle is
  tracked independently of rclone's `MountEntry` — a flapping backing mount
  must never force an odfs remount, since tolerating exactly that is the
  entire feature. `[remote.overlay] enabled = false` reverts a remote to the
  pre-P3 behaviour, which is also the fallback this section's measurement is
  for.
- **The index is reachable without a live daemon.** `onedrive-tui ls <path>` /
  `find <pattern>` (`crates/cli/src/index_cmd.rs`) open the state database
  read-only, the same pattern `client::status_from_database` already
  established — there is no IPC round trip to answer them, because the entire
  point is that listing the index needs nothing running. `onedrive-tui index
  refresh` now actually crawls, and a periodic check (every 10 minutes,
  comparing against each remote's `full_crawl_interval`) crawls on its own.

### The overlay go/no-go

§5 named the risk plainly: `~/OneDrive` → odfs → the backing mount is two FUSE
hops where a bare rclone mount is one, and said the measurement would decide
whether `[remote.overlay] enabled = false` should be the recommended default
rather than just an escape hatch.

Measured on this machine: a real `onedrived` (release build) managing a real
odfs mount in front of a real backing `rclone mount`, both pointed at an
`alias` remote (local disk, no credentials needed — the same stand-in trick
§16's matrix uses) over a synthetic tree of 2,000 files across 50 directories,
against a second bare `rclone mount` of the identical tree as the baseline.
`fio` was not available on this machine; `find` plus direct Python
`os.stat`/`os.listdir`/file-read timings stood in for it.

| Operation | Bare rclone mount | Through odfs | Overhead |
| --- | --- | --- | --- |
| `find` the whole tree (2,000 files) | ~26 ms | ~108 ms | +~82 ms |
| List one directory (40 entries) | 0.41 ms | 3.68 ms | +3.3 ms |
| `stat` one file | 0.06 ms | 0.08 ms | +0.02 ms |
| Read one whole small file (~2 KB) | 0.64 ms | 1.22 ms | +0.58 ms |

The relative overhead looks large (2–9×) but the absolute numbers do not: every
single-operation figure stays under 4 ms, an order of magnitude below the
~100 ms a human notices as "slow." The whole-tree `find` is the one case where
the extra hop is visible at all in absolute terms, and even there it is a
tenth of a second for listing every file in the tree at once — not an
operation a file manager performs on every click. None of this measures real
OneDrive network latency (typically tens to hundreds of milliseconds per API
call, dwarfing a sub-millisecond local FUSE hop), so on the real remote the
odfs overhead is expected to be a smaller fraction of each operation's total
time than it is here against local disk, not a larger one.

**Verdict: go.** The overlay stays enabled by default. `[remote.overlay]
enabled = false` remains available for a machine where this trade looks
different, but nothing in this measurement argues for flipping the default.

## 19. P4 complete: the outbox

P4 is §4.5's offline-write queue: a write that arrives with no network does
not get refused and it does not get silently lost — it is staged on disk,
visibly, and replayed against the remote only after re-checking that the
remote still matches what the edit started from.

### What P4 adds

- **The outbox table** (`onedrive_core::state::OutboxRecord`, `crates/core/src/state.rs`):
  one row per staged write — `remote`, `path`, `staged_path`, and a baseline
  (`base_size`/`base_quickxor`/`base_mtime`) recording what the remote looked
  like when the edit started. `queue_outbox` is idempotent per `(remote,
  path)`: a second offline edit to the same path before replay rewrites the
  staged bytes but keeps the *first* edit's baseline, since that is the
  version replay must still check against. `remove_outbox` is a hard
  `DELETE` — there is no terminal `'done'` state to accumulate.
- **Staging and replay** (`crates/daemon/src/outbox.rs`). `stage_write` is
  synchronous — it writes the staged file and queues the row, called directly
  from odfs with no network involved. `replay` is the async half, one
  `rclone lsjson --no-traverse --hash` per queued file (verified, like P2's
  bisync flags, against both a local `alias` stand-in and the real
  `onedrive-timm` remote, so the "directory not found" string the `Missing`
  case matches on is confirmed wording, not an assumption) followed by one of:
  - remote still matches the baseline → `rclone copyto --checksum` uploads the
    staged file to the canonical path, the row is removed, the staged file is
    deleted;
  - remote has changed → the canonical file is **not touched**; the staged
    edit uploads as a timestamped `<name>.conflict-local-<ts>` copy (the same
    `reconcile::conflict_copy_name` P2 uses), a conflict row is inserted, a
    persistent notification fires with the outbox's staging folder as the
    "open folder" target, and — unlike the plain-upload case — the staged
    file is kept on disk, so the edit is recoverable both as the uploaded
    copy and locally;
  - the listing itself fails (auth, network, a typo'd remote) → the row stays
    queued, `last_err`/`attempts` record why, nothing is guessed at (R3).

  A `None` baseline (nothing known to exist — a brand-new file created
  offline) only matches a remote that is still `Missing`; a remote that has
  since grown *something* the index never saw counts as changed, never as
  "safe to create over."

- **odfs routes every write-shaped operation through it**
  (`crates/daemon/src/odfs.rs`). The live-first design from P3 gains one more
  tier, ahead of the backing mount: `resolve`, `read`, and `children` all
  check for an outbox-staged file first, because a not-yet-replayed offline
  edit is the authoritative content for that path even if the backing mount
  happens to be reachable again mid-session — it does not have the edit yet.
  `write` falls back to staging (seeded with the best-known current content,
  via a live backing read or the VFS cache, falling back to empty — safe
  either way, since replay always re-verifies before uploading) only once a
  direct write to the backing path has actually failed; `create` does the
  same for a brand-new file, except a real `EEXIST` name collision, which is
  never staged over. `children` merges in outbox rows for files that exist
  nowhere else yet, so a file created entirely offline shows up in a
  directory listing. Structural operations — `unlink`/`rename`/`mkdir`/
  `rmdir` — are deliberately untouched by any of this and still return
  `EROFS` offline, exactly as the plan calls for: replaying a delete or a
  rename against a tree that moved underneath you is how mass deletions
  happen, so those stay refused rather than queued.
- **Replay runs on reconnect** (`crates/daemon/src/mountloop.rs`): the
  `Event::NetworkOnline` handler calls `replay_outbox_all` for every enabled
  remote, right after the existing pin sync. `onedrive-tui outbox retry
  [<remote>]` drives the same function on demand through a new
  `Control::OutboxRetry` message, handled by the same supervisor task — the
  identical pattern `Control::SyncPins`/`Control::IndexRefresh` already use.
- **The CLI** (`crates/cli/src/main.rs`, `render.rs`): `onedrive-tui outbox`
  (bare, or `ls`) lists every queued write with a non-zero exit while
  anything is queued — the same "non-destructive default, exit code drives a
  prompt" convention `conflicts` established. `outbox retry [<remote>]` asks
  the daemon to replay now. `outbox discard <id>` removes one row and deletes
  its staged file, answered synchronously (same reasoning as
  `conflicts resolve`: it is the one outbox operation that deletes something,
  so the caller is told what actually happened rather than that it was
  queued).

### A Send constraint worth recording

`outbox::replay`/`replay_one` take `app: &Arc<App>`, not `store: &Store`
directly, and this is not a style choice. `rusqlite::Connection` — and so
`onedrive_core::state::Store` — is `Send` but not `Sync` (documented on
`App`'s own `store` field). `replay` is async and awaits twice per row (the
remote check, the upload); any reference alive across either await becomes
part of the generated future's captured state, and `mountloop::run` is itself
spawned with `tokio::spawn`, which requires the whole future to be `Send`.
A bare `&Store` parameter satisfies that requirement nowhere — `&Store: Send`
needs `Store: Sync`, which does not hold — so the first version of this
wiring failed to compile with exactly that error. Routing every store access
through `App::with_store`, which locks and releases `App`'s `Mutex<Store>`
for one short synchronous burst at a time and never across an `.await`, is
the fix: `&Arc<App>` is `Send` (the rest of the daemon already relies on
this), so the captured state across each await is sound. `outbox.rs`'s own
tests build a minimal `App` the same way `app.rs`'s existing test helper
does, rather than a bare in-memory `Store`, for the same reason.

### Verification

- **Unit, `onedrive_core::state`**: 6 tests for the outbox table — idempotent
  queueing, restaging preserves the original baseline, failure recording,
  hard-delete on resolution, oldest-first ordering across remotes.
- **Unit, `outbox.rs`** (7 tests, against the same `alias`-remote stand-in
  P2's matrix uses, so no credentials are needed and nothing destructive ever
  reaches a real remote): staging writes the file and queues a row;
  restaging rewrites bytes without touching the baseline; a brand-new file
  with no conflict replays as a plain upload; an unchanged remote replays as
  a plain overwrite; **the core safety case** — a remote that changed while
  the edit was staged becomes a timestamped conflict copy, with the
  canonical remote file verified untouched, the conflict copy verified
  present with the offline content, and the staged local copy verified to
  still exist; a remote that cannot be checked leaves the row queued with the
  error recorded; `matches_baseline` treats "nothing" as a baseline too
  (direct unit test, not just exercised indirectly).
- **Unit, `odfs.rs`** (2 new tests, real FUSE mounts via `fuser::spawn_mount`
  over a tempdir backing directory, same harness P3's tests use): an offline
  write to a file the index already knows about stages instead of failing,
  is served back out on the very next read, and records a baseline matching
  the content last seen online (verified via a second connection onto the
  same on-disk `state.db` — safe because of the WAL mode and 5s
  `busy_timeout` `Store::open` already sets, the same thing that lets the
  CLI read live state while the daemon writes it); a file created entirely
  offline is readable back and appears in a directory listing, where before
  P4 it would not exist anywhere at all. A pre-existing test
  (`file_creation_stages_offline_but_mkdir_is_still_refused`, renamed from a
  P3 test that asserted the opposite) now asserts the split explicitly:
  `create` stages, `mkdir` still refuses.
- **Unit, `app.rs`/`ipc.rs`/`render.rs`/CLI parsing** (6 + 2 + 2 + 2 tests):
  the three new `Request`/`Response` variants round-trip over the wire
  exactly like every existing one; listing, discarding (row and staged file
  both verified gone), and an unknown id are all covered; retrying queues the
  expected `Control::OutboxRetry` and rejects an unknown remote name, the
  same validation `MountRemount`/`PinSync` already apply; the rendered
  listing names both the id and the exact commands to type next, and a
  failed row's error surfaces rather than being hidden.
- **Full workspace**: `cargo build --workspace`, `cargo test --workspace`
  (482 tests across every lib, integration suite, and doctest in
  `onedrive-core`, `onedrived`, and `onedrive-tui` — including the 15-case
  safety matrix — all green), `cargo clippy --workspace --all-targets`
  clean, `cargo fmt --all` clean.

## 20. P5 complete: the dashboard and guided setup

P5 is §8's interface: a `ratatui` dashboard for `onedrive-tui` with no
arguments, and `onedrive-tui setup` as the one command that finishes the job
— OAuth → mount point → pins → config written → service installed → linger
enabled → first crawl queued — replacing the old GUI's four disconnected
steps spread across three screen regions.

### What P5 adds

- **The dashboard** (`crates/cli/src/tui.rs`). Five panes — Overview, Pins,
  Conflicts, Outbox, Log — `Tab`/`Shift+Tab` to switch, `↑`/`↓`/`j`/`k` to
  select, `r` to refresh now, `Enter` to act, `q`/`Esc`/`Ctrl-C` to quit.
  Overview reuses `render::status` verbatim, so the dashboard can never
  disagree with `onedrive-tui status` about what "healthy" means — there is
  only one function that decides that, called from both places. Pins,
  Conflicts, and Outbox are `List` widgets built from the same single-line
  renderers (`render::pin_line`/`conflict_line`/`outbox_line`, new in this
  phase, also reused by `pin ls`) rather than a second copy of the
  formatting logic. The Log pane reuses `log_cmd`'s rendering via a new
  `log_cmd::tail` helper. It refreshes on its own every two seconds and
  immediately on `r`; `Enter` performs exactly one safe, unambiguous action
  per pane — `ConflictResolve { keep: Both }` on the selected conflict (the
  same thing the plan's §4.3 notification's "Resolved" button does: close
  the alert, touch no files) and `OutboxRetry` scoped to the selected row's
  remote — never anything a confirmation dialog would be needed for, since
  there is no dialog here to second-guess a worse choice. All of the pane
  cycling, selection clamping, and `Enter` → request mapping is pure
  (`Pane::next`/`prev`, `move_selection`, `enter_request`) and unit tested
  without a terminal; only the actual `crossterm`/`ratatui` paint calls are
  untested glue, the same split `render.rs`'s pure string functions versus
  `service.rs`'s untested `systemctl` calls already established.
- **A real terminal-corruption bug, found by actually running it.** Manual
  verification (via `tmux send-keys`/`capture-pane`, since this is a true
  interactive terminal app with no headless equivalent to `cargo run` —
  redirecting stdin to a file does not work, because `crossterm` talks to
  `/dev/tty` directly for raw-mode control precisely so stdin redirection
  *doesn't* break it, which also means it doesn't *feed* it) surfaced that
  raw mode clears the terminal's own `ISIG` flag: pressing Ctrl-C while the
  dashboard is up never reaches the process as `SIGINT` at all, it arrives
  as an ordinary key event. The dashboard did not handle that key, so Ctrl-C
  silently did nothing — not corrupting, but not the universally expected
  behaviour either, so it is now handled explicitly as quit. Separately, a
  panic mid-draw would have left raw mode and the alternate screen engaged
  — `run` now installs a panic hook that restores the terminal before
  chaining to the default hook, verified by confirming the shell is usable
  immediately after both a normal `q` quit and a `Ctrl-C` quit (a command
  typed right after exits 0 and echoes correctly, rather than the terminal
  needing a manual `reset`).
- **`onedrive-tui setup`** (`crates/cli/src/setup.rs`) and **the rclone
  wizard** (`crates/cli/src/rclone_wizard.rs`, ported from the old GUI's
  `src/bin/gui/rclone_config_wizard.rs`, which got the
  `config create/update --non-interactive` + `--continue --state --result`
  protocol right). Two fixes found by driving a real `onedrive`-type
  creation against this machine's rclone far enough to reach the
  `config_is_local` and `config_token` questions and a rejected token —
  no credentials or network call of ours needed for any of it:
  - every step now runs under a timeout (`STEP_TIMEOUT`, generous
    `OAUTH_TIMEOUT` for an answer of `"true"`) instead of blocking forever
    on a hung child, the `kill_on_drop`-style fix the plan called for,
    adapted to `std::process` since the CLI has no async runtime;
  - a non-empty `Error` alongside a non-empty `State` is **not** fatal —
    verified against a real rejected OAuth token, rclone re-offers the
    identical question rather than ending the conversation. The ported GUI
    code treated any `Error` as a hard `WizardStep::Error`, which would have
    ended the wizard on a single typo'd answer.

  The interactive loop also drops the GUI's `config_is_local` auto-answer
  (it always answered `true`, assuming a local browser): a terminal tool is
  just as likely to be run over SSH with no browser, so every question,
  including that one, is now presented uniformly — help text, examples,
  default — and the user answers it themselves, exactly as rclone's own
  interactive prompt would ask. `setup` composes the collected remote name,
  mount point, and any pins into a config via `toml::from_str` into
  `onedrive_core::config::Config` (reusing the exact deserialization path a
  hand-written `config.toml` goes through, including every `#[serde(default
  = …)]`), then `Config::validate` and `Config::save` before installing the
  service and queuing the first crawl. `setup --reauth <remote>` runs just
  the OAuth half via `rclone config update --non-interactive --all`,
  against an existing remote — this is what `Code::AuthExpired`'s hint
  (`onedrive-tui setup --reauth <remote>`), already baked into `code.rs`
  from an earlier phase, actually points at.

### Verification

- **Unit** (14 new tests): `tui.rs` — pane cycling wraps both directions,
  selection clamps rather than wrapping past either end or going negative on
  an empty list, only Conflicts/Outbox are selectable, `Enter` maps to
  exactly the right request for each pane and is a no-op everywhere else
  including an out-of-range selection. `rclone_wizard.rs` — the real
  create → decline-browser → token-paste → rejected-token sequence above,
  run against this machine's actual rclone with a throwaway `--config` path
  (never the real `~/.config/rclone/rclone.conf`), asserting the question
  names, that the rejected answer's state is unchanged, and that it does not
  read as done. `log_cmd.rs` — `tail` returns only the most recent N lines
  and is an empty list rather than an error with no log directory yet.
  `render.rs` — the three new single-line renderers (`pin_line`,
  `conflict_line`, `outbox_line`) each fit one row and show the failure
  case. `main.rs` — `setup --reauth <remote>` parses.
- **Manual, interactive** (`tmux send-keys`/`capture-pane`, the only way to
  exercise a real terminal app's actual key handling and screen output):
  every pane renders and is reachable by `Tab`; `q` and `Ctrl-C` both quit
  cleanly, confirmed by a command typed immediately afterward in the same
  pane running and echoing normally rather than the terminal being left in
  raw mode or the alternate screen.
- **Full workspace**: `cargo build --workspace`, `cargo test --workspace`
  (496 tests, all green — the one transient failure seen in an
  `onedrived` safety-matrix test tied to a real `kill -9` and subprocess
  timing did not reproduce on a clean re-run, and is unrelated to anything
  this phase touched), `cargo clippy --workspace --all-targets` clean,
  `cargo fmt --all` clean.

### Known gap

`onedrive-tui setup`'s pin-adding loop and the OAuth wizard's prompts are
genuinely interactive — they read from stdin in a loop, which is correct for
what they are but means (unlike every other command in this project) they
have no automated end-to-end test driving the whole `setup` flow start to
finish; only their components (`rclone_wizard`'s protocol driving, the
config-composing-and-validating step, `service::install`, each independently)
are. Exercising the full guided flow against a real "create a new OneDrive
remote" would need an actual Microsoft account and browser redirect, which is
out of reach for an automated test and was not attempted.

## 21. P6 complete: cutover

`0.3.0`. The old GUI, its daemon, and the sync engine that caused §2's data
loss are deleted, not kept around as a fallback — `onedrived` + `onedrive-tui`
are now the only thing in this repository.

### What P6 removed

- **`src/`** — the 0.2.x library and both its binaries: `src/bin/gui/` (the
  egui config editor and status viewer, 19 files), `src/bin/daemon/`
  (`sync_executor.rs`, `sync_scheduler.rs`, `resolution_executor.rs`,
  `resolution_watcher.rs`, `mount_manager.rs`, and everything else §2's
  diagnosis is about), and the shared lib (`config.rs`, `conflict.rs`,
  `sync_baseline.rs`, `status.rs`, `pid_lock.rs`, `paths.rs`, `defaults.rs`) —
  57 files in total, ~6,500 lines.
- **the root-level `tests/`** — eight integration test files that existed only
  to test the code above.
- **`assets/`** — the GUI's icon and `.desktop` entry.
- **`nix/default.nix`** — a non-flake packaging path that shelled out to
  pre-built GUI binaries; superseded by `flake.nix`'s own derivation, which it
  was never wired into.
- From **`flake.nix`**: the legacy `nixosModule`/`services.onedrive-mount`,
  the X11/GL-linked `package` derivation and its `wrapProgram`'d
  `guiWrapper`, and the `gui`/`legacy-daemon` apps. `runtimeLibs`/
  `runtimeLibPath`/the X11 `buildInputs` are gone from the dev shell too —
  nothing in the surviving crates dlopens anything.
- From **CI** (`.github/workflows/ci.yml`): the `libx11-dev`/`libxcursor-dev`/
  …/`libgl1-mesa-dev` apt packages and the separate "Clippy (legacy GUI +
  daemon)"/"Test (legacy GUI + daemon)" steps. **`release.yml`** is a full
  rewrite: it built `onedrive-mount`/`onedrive-mountd` for nix/glibc/musl
  before; it now builds `onedrived`/`onedrive-tui` the same three ways, with
  the X11 runtime-dep installation step gone from `build-glibc` and
  `build-musl` able to statically link *both* binaries now, not just the
  daemon (neither needs libfuse or a GUI toolkit to begin with).
- The root `Cargo.toml`'s `[package]`/`[dependencies]`/`[features]`/`[[bin]]`
  sections — it is now a pure virtual workspace manifest, `[workspace]` +
  `[workspace.dependencies]` only. `Cargo.lock` lost every dependency that
  existed only for the GUI (`eframe`, `resvg`, `open`, `dirs`) or the old
  daemon (`tokio-util`, the `nix` crate, `notify-rust`'s predecessors) — 238
  locked packages now, down from before.

### A discovery made while verifying this, not caused by it

Running `nix build .#onedrive` as part of verifying the cutover — the first
time this session a build depended on **git's view of the tree** rather than
the raw filesystem — surfaced that most of the source written since P1
(`crates/daemon/src/lib.rs` itself, `odfs.rs`, `index.rs`, `pins.rs`,
`outbox.rs`, `bisync.rs`, `notify.rs`, `reconcile.rs`, `resolve.rs`,
`snapshot.rs`, `pinwatch.rs`, the whole of `crates/daemon/tests/`, and
`crates/cli/src/{index_cmd,tui,setup,rclone_wizard}.rs`) had never actually
been `git add`ed across any prior session — plain `cargo build`/`cargo test`
never cared, since they read the real filesystem, but Nix flakes restrict a
local dirty repository's source to git-tracked content, so the sandbox build
failed with `can't find lib onedrived at path .../src/lib.rs` despite the
file plainly existing. Fixed with `git add -A` (staged, not committed — nothing
in this project is committed without being asked). Worth recording because it
is exactly the kind of gap that stays invisible until something other than
`cargo` looks at the tree.

### The Nix sandbox cannot mount FUSE

The same `nix build` run surfaced the only other new finding: 11 of
`onedrived`'s tests — `odfs`'s real-mount tests and `mount`'s
reconcile/remount tests, the ones that call `fuser::spawn_mount` against a
real `/dev/fuse` — fail inside Nix's build sandbox with "No such file or
directory," because the sandbox has neither the device node nor the
privilege to create one. This is a packaging constraint, not a code bug: the
same 11 pass under plain `cargo test --workspace` (confirmed — they are
exactly 11 of the 226 `onedrived` tests, 215 others unaffected) and under
CI's non-Nix `check` job, both of which run on a real kernel with a real
`/dev/fuse`. `flake.nix`'s `onedrive` derivation now lists exactly those 11
test names under `checkFlags = [ "--skip=..." ... ]` — by exact name rather
than by module prefix, so `nix build`'s check phase keeps every pure-logic
`odfs`/`mount` test (inode table, pin-path resolution, xattr synthesis, the
backoff/health-reporting logic) and only skips the handful that need a mount
no sandbox can give them.

### Verification

- `nix flake check --no-build`: evaluates cleanly, including the NixOS module.
- `nix build .#onedrive`: builds both binaries and runs the workspace test
  suite inside the sandbox (minus the 11 FUSE tests above) — this is the
  first time in the project this specific path (a clean Nix build from the
  flake, not `cargo` directly) was actually exercised, and it is now part of
  CI's `nix` job, unchanged from before this phase.
- `onedrived --version`, `onedrive-tui --version`, both run from the Nix
  build output directly.
- The CI smoke-test script (`.github/workflows/ci.yml`, untouched by this
  phase beyond removing the legacy steps) replayed locally end to end: version
  flags, `--print-unit` contains `Type=notify`, `--check-config` correctly
  fails with no config and succeeds once `config.example.toml` is in place,
  the daemon starts and answers `onedrive-tui status --json` with
  `"source": "daemon"`, `doctor` exits 1 on the expected CI-box warnings
  (no rclone remote actually authenticated, no systemd unit installed, no
  linger), a second daemon instance is correctly refused, and `SIGTERM`
  stops it cleanly.
- **Full workspace**: `cargo build --workspace`, `cargo test --workspace`
  (448 tests across `onedrive-core`, `onedrived`, and `onedrive-tui`,
  including the 15-case safety matrix — all green; down from 496 only
  because the deleted legacy package's own tests are gone with it),
  `cargo clippy --workspace --all-targets` clean, `cargo fmt --all` clean.

### What is still manual

A `v0.3.0` tag is what is left to actually ship this.

### A real CI failure, and what it actually was

Once this was pushed, GitHub Actions' `check` job failed 14 of the 15 safety
matrix tests with an unhelpfully bare `failed [E_INTERNAL]: rclone bisync
exited with exit status: 1`. Every local run — including in parallel, which
is how `cargo test` already runs by default — passed, so this was an
environment difference, not a flaky test. The actual cause, confirmed via
Ubuntu's own package metadata rather than assumed: `ci.yml`/`release.yml`
installed rclone with `apt-get install rclone`, which on `ubuntu-latest`
(24.04) gives **1.60.1** — Ubuntu's package trails upstream by years.
`bisync.rs`'s own `--conflict-resolve`/`--conflict-loser`/`--recover` flags
were only added in rclone **1.74.0**; on 1.60.1, every invocation hit an
immediate Cobra flag-parse error (`Error: unknown flag: --conflict-resolve`),
printed as plain text before rclone's JSON logger even exists, exit code 1.

That also explains why the failure was so hard to read: `classify_failure`'s
`last_error_line` only recognises **JSON** log lines at error level or above,
so a plain-text pre-logger error found nothing and fell through to a bare
`"rclone bisync exited with {status}"` — technically correct, useless in
practice. Fixed in both directions:

- **`ci.yml`/`release.yml`** now install rclone via `rclone.org/install.sh`
  (always current) instead of apt, keeping only `fuse3` from the package
  manager.
- **`bisync.rs`'s `classify_failure`** gained a second fallback,
  `first_nonempty_line`, tried when `last_error_line` finds no JSON: the
  first non-empty line of raw stderr (Cobra prints the error before the
  usage dump it appends, the same reason `last_error_line` already prefers
  the first JSON match over the last). Two new tests
  (`a_non_json_stderr_still_surfaces_its_real_message`,
  `empty_stderr_falls_back_to_the_bare_status`) cover it — the daemon's own
  code was never wrong here, but it was unnecessarily opaque about a real
  rclone-side error, and now isn't.

### A second CI failure, from the fix for the first

Fixing the above and pushing again surfaced a different failure in the same
job: the "Smoke test the binaries" step's second `onedrived --check-config`
call reported `no config file` immediately after the step had just written
one, exit code 4 (`exit::BAD_CONFIG`). Reproduced locally in one line rather
than guessed at: setting `XDG_CONFIG_HOME` to an arbitrary directory before
running the exact same script reproduces it exactly, and `unset`-ting it
(alongside `XDG_STATE_HOME`/`XDG_CACHE_HOME`) fixes it. The cause:
`Paths::from_env`'s XDG handling is correct per spec — an already-set
`XDG_CONFIG_HOME` wins over `$HOME/.config` — but the smoke-test step only
overrode `HOME` and `XDG_RUNTIME_DIR`, not the other three XDG variables, so
if any of them is set in the runner's ambient environment (plausible on a
shared GitHub-hosted image), the step's `mkdir`/`cp` and the daemon's own
lookup land in two different places. Fixed by explicitly unsetting
`XDG_CONFIG_HOME`/`XDG_STATE_HOME`/`XDG_CACHE_HOME` before exporting the
overrides, in both `ci.yml`'s smoke test and `flake.nix`'s `preCheck`
(the latter as a defensive match, not because it was observed failing there
— Nix's build sandbox is normally stripped of ambient host env vars, so this
specific bug is unlikely to reach it, but nothing guarantees that forever).

### A third CI failure, from the fix for the second

Fixing that and pushing again got further — `onedrived --check-config`
correctly found the just-written config this time — but the same step then
died silently right after printing a perfectly valid `status --json` blob,
exit code 2, nothing from `log` or `doctor` at all. Reproduced locally by
giving the mount supervisor a few seconds to settle before calling `status`
(`sleep 3` between the readiness-poll loop and the status check): the
pipeline

```sh
"$BIN/onedrive-tui" status --json | tee /dev/stderr | grep -q '"source": "daemon"'
```

fails outright, identically. The cause: `status --json`'s own exit code *is*
the overall health roll-up (0 healthy / 1 degraded / 2 broken) — by design,
so a shell script can key off it — not "did this call succeed." The example
config's `onedrive-timm` remote has no real rclone credentials (correctly so,
for a smoke test that must run with no secrets), so the mount and pin
genuinely settle into `Failed` within this window; `MountState::Failed` maps
to `Health::Broken` (confirmed against the `render.rs` test
`a_failed_mount_breaks_and_an_unmounted_one_degrades`), so `status --json`
exits 2 even though it printed exactly what it should have. Piped straight
into `grep` under `pipefail`, that 2 propagates and `set -e` kills the step
before `log -n 5` or `doctor` ever run — both of which were therefore never
the problem, contrary to what the missing output suggested. Fixed by
capturing the JSON into a variable first (`|| true` so the capture itself
can't trip `-e` on the health-derived exit code) and asserting on it
separately — the pipeline's exit code was never what this step meant to
check; `grep`'s own, now-correctly-isolated, is.
