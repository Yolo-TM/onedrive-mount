//! odfs — the read-mostly offline overlay mounted at the path the user browses.
//!
//! What makes `~/OneDrive` listable with no network, and what the plan (§5)
//! calls "browsing warms the index for free": every operation tries the real
//! thing first and only falls back to the SQLite index when that fails, and
//! whatever the live path returns is folded straight back into the index. So
//! there is no separate "are we online" flag anywhere in this module — the
//! live attempt itself is the online/offline test, which means it can never
//! disagree with reality the way a cached flag could.
//!
//! Three kinds of path, routed differently:
//!
//! - **The root** (`""`) is synthesised; it has no node row.
//! - **A path under a pin's `remote_path`** routes straight to the pin's real
//!   local directory. Pins are a plain directory on disk, not a cache, so they
//!   are the ground truth for their own subtree even when the rest of the
//!   remote is unreachable — this is what makes `~/OneDrive/Dokumente/Keepass`
//!   and `~/onedrive-keepass` show the same bytes.
//! - **Everything else** goes through the backing `rclone mount`
//!   (`Paths::backing_mount`) first, then the index, then — for file content
//!   only — the rclone VFS cache file directly, so a file read once stays
//!   readable offline even with the backing mount down.
//!
//! Writes follow the plan's §5 table: passthrough to the backing mount while
//! it is reachable (exactly as a bare rclone mount already behaves); when it
//! is not, the write is staged into the outbox (`crate::outbox`) instead of
//! refused, and replayed against the remote on reconnect. Structural ops
//! (`unlink`/`rename`/`mkdir`/`rmdir`) are not staged — those still return
//! `EROFS` offline, per the plan's explicit exclusion.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::index;
use crate::outbox;
use chrono::{DateTime, Utc};
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, Generation, INodeNo, LockOwner, OpenFlags,
    ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite,
    ReplyXattr, Request,
};
use onedrive_rc_core::config::RemoteConfig;
use onedrive_rc_core::paths::{Paths, join_guarded};
use onedrive_rc_core::state::{NodeInput, NodeRecord, Store};

/// How long the kernel may cache an entry/attr before asking again. Short on
/// purpose: every lookup already re-stats the live path when it can, so a
/// long TTL would only serve the kernel stale data for no benefit.
const TTL: Duration = Duration::from_secs(1);

const ROOT_PATH: &str = "";

// ---------------------------------------------------------------------------
// Inode table
// ---------------------------------------------------------------------------

/// Maps FUSE inode numbers to remote-relative paths and back.
///
/// Purely in-memory and rebuilt on every mount — inodes have no reason to
/// survive a remount, and the kernel's `forget` calls are not acted on, which
/// is a deliberate trade: this table only grows for as long as one mount runs,
/// in exchange for never having a path resolve to the wrong, reused inode.
#[derive(Debug, Default)]
struct InodeTable {
    next: u64,
    path_to_ino: HashMap<String, u64>,
    ino_to_path: HashMap<u64, String>,
}

impl InodeTable {
    fn new() -> Self {
        let mut t = Self {
            next: 2,
            ..Default::default()
        };
        t.path_to_ino.insert(ROOT_PATH.to_string(), 1);
        t.ino_to_path.insert(1, ROOT_PATH.to_string());
        t
    }

    fn path_of(&self, ino: u64) -> Option<String> {
        self.ino_to_path.get(&ino).cloned()
    }

    /// The inode for a path, allocating a new one if this path has never been
    /// seen in this mount's lifetime.
    fn ino_for(&mut self, path: &str) -> u64 {
        if let Some(&ino) = self.path_to_ino.get(path) {
            return ino;
        }
        let ino = self.next;
        self.next += 1;
        self.path_to_ino.insert(path.to_string(), ino);
        self.ino_to_path.insert(ino, path.to_string());
        ino
    }
}

/// `"Keepass"` + `"Timm.kdbx"` → `"Keepass/Timm.kdbx"`; `""` + `"Timm.kdbx"` →
/// `"Timm.kdbx"`. The root has no name component to join onto.
fn join_rel(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

// ---------------------------------------------------------------------------
// Pin routing
// ---------------------------------------------------------------------------

struct PinRoute {
    /// Remote-relative path the pin covers, no trailing slash. `""` would mean
    /// the whole remote is pinned, which is legal but unusual.
    remote_path: String,
    local: PathBuf,
}

impl PinRoute {
    /// The absolute local path for `path`, if `path` is this pin or inside it.
    fn resolve(&self, path: &str) -> Option<PathBuf> {
        let suffix = if self.remote_path.is_empty() {
            path
        } else if path == self.remote_path {
            ""
        } else {
            let rest = path.strip_prefix(&self.remote_path)?;
            rest.strip_prefix('/')?
        };
        join_guarded(&self.local, suffix)
    }
}

// ---------------------------------------------------------------------------
// Attribute synthesis
// ---------------------------------------------------------------------------

fn epoch_secs(secs: i64) -> SystemTime {
    if secs <= 0 {
        UNIX_EPOCH
    } else {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    }
}

fn attr_from_node(ino: u64, node: &NodeRecord, uid: u32, gid: u32) -> FileAttr {
    let mtime = epoch_secs(node.mtime.timestamp());
    FileAttr {
        ino: INodeNo(ino),
        size: node.size,
        blocks: node.size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind: if node.is_dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        },
        perm: if node.is_dir { 0o755 } else { 0o644 },
        nlink: if node.is_dir { 2 } else { 1 },
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn split_for_attr(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((p, n)) => (p, n),
        None => ("", path),
    }
}

fn node_input_from_metadata(path: &str, meta: &std::fs::Metadata) -> NodeInput {
    NodeInput {
        path: path.to_string(),
        is_dir: meta.is_dir(),
        size: if meta.is_dir() { 0 } else { meta.len() },
        mtime: meta
            .modified()
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(|_| Utc::now()),
        quickxor: None,
    }
}

// ---------------------------------------------------------------------------
// Filesystem
// ---------------------------------------------------------------------------

pub struct Odfs {
    remote: String,
    remote_cfg: RemoteConfig,
    backing: PathBuf,
    paths: Paths,
    pins: Vec<PinRoute>,
    store: Mutex<Store>,
    inodes: Mutex<InodeTable>,
    uid: u32,
    gid: u32,
}

impl Odfs {
    pub fn new(remote: &RemoteConfig, paths: &Paths) -> anyhow::Result<Self> {
        let store = Store::open(&paths.state_db())?;
        let pins = remote
            .pins
            .iter()
            .filter(|p| p.enabled)
            .map(|p| PinRoute {
                remote_path: p.remote_path.trim_matches('/').to_string(),
                local: p.local_path(paths),
            })
            .collect();

        Ok(Self {
            remote: remote.name.clone(),
            remote_cfg: remote.clone(),
            backing: paths.backing_mount(&remote.name),
            paths: paths.clone(),
            pins,
            store: Mutex::new(store),
            inodes: Mutex::new(InodeTable::new()),
            // SAFETY: getuid/getgid never fail.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        })
    }

    /// Where this path's content would live in rclone's VFS cache, if at all.
    fn vfs_cache_candidate(&self, path: &str) -> Option<PathBuf> {
        self.paths.vfs_cached_file(&self.remote, path)
    }

    fn pin_local(&self, path: &str) -> Option<PathBuf> {
        self.pins.iter().find_map(|p| p.resolve(path))
    }

    fn backing_path(&self, path: &str) -> Option<PathBuf> {
        join_guarded(&self.backing, path)
    }

    /// Where an offline write for this path is staged, if one is already
    /// queued. While a row exists this is the authoritative content for the
    /// path — our own edit has not reached the remote yet, so the backing
    /// mount (even if it happens to be reachable again right now) must not
    /// be allowed to look like it already has it.
    fn outbox_staged(&self, path: &str) -> Option<PathBuf> {
        let store = self.store.lock().ok()?;
        let row = store.outbox_for(&self.remote, path).ok().flatten()?;
        Some(PathBuf::from(row.staged_path))
    }

    /// Best-known current content for `path`, to seed a staged copy before
    /// layering an offline edit on top. Never fails: an unknown file just
    /// seeds empty, which is still safe — replay re-checks the remote before
    /// ever overwriting anything, so a wrong seed costs a conflict, not data.
    fn best_effort_read(&self, path: &str) -> Vec<u8> {
        if let Some(backing) = self.backing_path(path)
            && let Ok(data) = std::fs::read(&backing)
        {
            return data;
        }
        if let Some(cache) = self.vfs_cache_candidate(path)
            && let Ok(data) = std::fs::read(&cache)
        {
            return data;
        }
        Vec::new()
    }

    /// Stage `path` for offline replay, seeded with its best-known current
    /// content, baselined against whatever `resolve` can still establish
    /// (the index, most likely, since backing is presumably down right now).
    fn stage_offline(&self, path: &str) -> Option<PathBuf> {
        if let Some(existing) = self.outbox_staged(path) {
            return Some(existing);
        }
        let baseline = self.resolve(path).map(|r| self.as_node_record(path, &r));
        let seed = self.best_effort_read(path);
        let store = self.store.lock().ok()?;
        outbox::stage_write(
            &store,
            &self.remote_cfg,
            &self.paths,
            path,
            &seed,
            baseline.as_ref(),
            Utc::now(),
        )
        .ok()
    }

    /// Stage a brand-new file: no seed content and no baseline, since nothing
    /// is known to exist at this path.
    fn stage_offline_new(&self, path: &str) -> Option<PathBuf> {
        let store = self.store.lock().ok()?;
        outbox::stage_write(
            &store,
            &self.remote_cfg,
            &self.paths,
            path,
            &[],
            None,
            Utc::now(),
        )
        .ok()
    }

    /// Resolve one entry, live-first: a real `stat()` against the backing
    /// mount when it succeeds (and is folded back into the index), the index
    /// otherwise. This one function is both "are we online" and "serve the
    /// answer" — there is nothing else deciding that question.
    fn resolve(&self, path: &str) -> Option<NodeRecordOrLive> {
        if let Some(local) = self.pin_local(path)
            && let Ok(meta) = std::fs::symlink_metadata(&local)
        {
            return Some(NodeRecordOrLive::Live(node_input_from_metadata(
                path, &meta,
            )));
        }

        // An offline edit not yet replayed is the authoritative content for
        // this path, even if the backing mount happens to be reachable again
        // right now — it does not have our edit yet.
        if let Some(staged) = self.outbox_staged(path)
            && let Ok(meta) = std::fs::symlink_metadata(&staged)
        {
            return Some(NodeRecordOrLive::Live(node_input_from_metadata(
                path, &meta,
            )));
        }

        if let Some(backing) = self.backing_path(path)
            && let Ok(meta) = std::fs::symlink_metadata(&backing)
        {
            let input = node_input_from_metadata(path, &meta);
            if let Ok(store) = self.store.lock() {
                let _ = index::record_listing(&store, &self.remote, std::slice::from_ref(&input));
            }
            return Some(NodeRecordOrLive::Live(input));
        }

        let store = self.store.lock().ok()?;
        store
            .node(&self.remote, path)
            .ok()
            .flatten()
            .map(NodeRecordOrLive::Indexed)
    }

    fn find_child(&self, parent: &str, name: &str) -> Option<(String, NodeRecordOrLive)> {
        // Pins and the live backing listing both already carry exact names;
        // try a direct live stat under the exact, as-typed name first (the
        // common case), then fall back to the index's case-insensitive match
        // to find the canonical path, then resolve *that* path live-or-indexed.
        let direct = join_rel(parent, name);
        if let Some(found) = self.resolve(&direct) {
            return Some((direct, found));
        }

        let store = self.store.lock().ok()?;
        let node = store
            .find_child(&self.remote, parent, name)
            .ok()
            .flatten()?;
        drop(store);
        let path = node.path.clone();
        Some((path, NodeRecordOrLive::Indexed(node)))
    }

    fn attr_for(&self, ino: u64, path: &str, resolved: &NodeRecordOrLive) -> FileAttr {
        attr_from_node(
            ino,
            &self.as_node_record(path, resolved),
            self.uid,
            self.gid,
        )
    }

    /// Finishes a successful `create()` once the file exists somewhere
    /// `resolve` can see it (pin, backing, or freshly outbox-staged).
    fn reply_created(&self, path: &str, reply: fuser::ReplyCreate) {
        let ino = self.inodes.lock().unwrap().ino_for(path);
        if let Some(resolved) = self.resolve(path) {
            let attr = self.attr_for(ino, path, &resolved);
            reply.created(
                &TTL,
                &attr,
                Generation(0),
                FileHandle(0),
                fuser::FopenFlags::empty(),
            );
        } else {
            reply.error(Errno::EIO);
        }
    }

    /// Both branches of [`NodeRecordOrLive`] carry the same information; this
    /// is the one place that flattens them into an owned [`NodeRecord`], for
    /// anything that needs one (attributes, and an outbox baseline).
    fn as_node_record(&self, path: &str, resolved: &NodeRecordOrLive) -> NodeRecord {
        match resolved {
            NodeRecordOrLive::Indexed(node) => node.clone(),
            NodeRecordOrLive::Live(input) => NodeRecord {
                path: path.to_string(),
                parent: split_for_attr(path).0.to_string(),
                name: split_for_attr(path).1.to_string(),
                is_dir: input.is_dir,
                size: input.size,
                mtime: input.mtime,
                quickxor: input.quickxor.clone(),
            },
        }
    }

    fn root_attr(&self) -> FileAttr {
        FileAttr {
            ino: INodeNo::ROOT,
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// Children of `parent`, live-first exactly like [`Self::resolve`]: a real
    /// `read_dir` of the backing mount (or the pin directory) when it
    /// succeeds, folded into the index; the index's own children otherwise.
    /// Pins aside (an offline write is never staged for a pinned path — it
    /// goes straight to the pin's real directory), any outbox-staged file
    /// under `parent` is merged in afterwards, since a file created entirely
    /// offline exists in neither backing nor the index yet.
    fn children(&self, parent: &str) -> Vec<(String, NodeRecordOrLive)> {
        if let Some(local) = self.pin_local(parent)
            && let Ok(entries) = std::fs::read_dir(&local)
        {
            return self.live_children(parent, entries);
        }

        let mut out = if let Some(backing) = self.backing_path(parent)
            && let Ok(entries) = std::fs::read_dir(&backing)
        {
            self.live_children(parent, entries)
        } else {
            let Ok(store) = self.store.lock() else {
                return Vec::new();
            };
            store
                .children(&self.remote, parent)
                .unwrap_or_default()
                .into_iter()
                .map(|n| (n.path.clone(), NodeRecordOrLive::Indexed(n)))
                .collect()
        };

        self.merge_outbox_children(parent, &mut out);
        out
    }

    fn merge_outbox_children(&self, parent: &str, out: &mut Vec<(String, NodeRecordOrLive)>) {
        let Ok(store) = self.store.lock() else {
            return;
        };
        let Ok(rows) = store.open_outbox() else {
            return;
        };
        for row in rows {
            if row.remote != self.remote {
                continue;
            }
            if split_for_attr(&row.path).0 != parent {
                continue;
            }
            if out.iter().any(|(p, _)| p == &row.path) {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(&row.staged_path) else {
                continue;
            };
            let input = node_input_from_metadata(&row.path, &meta);
            out.push((row.path.clone(), NodeRecordOrLive::Live(input)));
        }
    }

    fn live_children(
        &self,
        parent: &str,
        entries: std::fs::ReadDir,
    ) -> Vec<(String, NodeRecordOrLive)> {
        let mut out = Vec::new();
        let mut inputs = Vec::new();
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(meta) = entry.metadata() else { continue };
            let path = join_rel(parent, &name);
            let input = node_input_from_metadata(&path, &meta);
            inputs.push(input.clone());
            out.push((path, NodeRecordOrLive::Live(input)));
        }
        if let Ok(store) = self.store.lock() {
            let _ = index::record_listing(&store, &self.remote, &inputs);
        }
        out
    }
}

/// Either a stored index row, or a just-obtained live `stat()` not yet (or no
/// longer) backed by one — both carry everything an attribute needs.
enum NodeRecordOrLive {
    Indexed(NodeRecord),
    Live(NodeInput),
}

impl NodeRecordOrLive {
    fn is_dir(&self) -> bool {
        match self {
            Self::Indexed(n) => n.is_dir,
            Self::Live(n) => n.is_dir,
        }
    }
}

impl Filesystem for Odfs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_path) = self.inodes.lock().unwrap().path_of(u64::from(parent)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };

        match self.find_child(&parent_path, name) {
            Some((path, resolved)) => {
                let ino = self.inodes.lock().unwrap().ino_for(&path);
                let attr = self.attr_for(ino, &path, &resolved);
                reply.entry(&TTL, &attr, Generation(0));
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let ino = u64::from(ino);
        if ino == 1 {
            reply.attr(&TTL, &self.root_attr());
            return;
        }
        let Some(path) = self.inodes.lock().unwrap().path_of(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.resolve(&path) {
            Some(resolved) => reply.attr(&TTL, &self.attr_for(ino, &path, &resolved)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let ino = u64::from(ino);
        let Some(parent_path) = self.inodes.lock().unwrap().path_of(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino, FileType::Directory, ".".to_string()),
            (ino, FileType::Directory, "..".to_string()),
        ];
        for (path, resolved) in self.children(&parent_path) {
            let child_ino = self.inodes.lock().unwrap().ino_for(&path);
            let kind = if resolved.is_dir() {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            let name = split_for_attr(&path).1.to_string();
            entries.push((child_ino, kind, name));
        }

        for (i, (child_ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(child_ino), (i + 1) as u64, kind, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let ino = u64::from(ino);
        if ino == 1 || self.inodes.lock().unwrap().path_of(ino).is_some() {
            reply.opened(FileHandle(0), fuser::FopenFlags::empty());
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let Some(path) = self.inodes.lock().unwrap().path_of(u64::from(ino)) else {
            reply.error(Errno::ENOENT);
            return;
        };

        if let Some(local) = self.pin_local(&path)
            && let Ok(data) = read_range(&local, offset, size)
        {
            reply.data(&data);
            return;
        }

        // A not-yet-replayed offline edit is the authoritative content, same
        // reasoning as `resolve`'s staged check.
        if let Some(staged) = self.outbox_staged(&path)
            && let Ok(data) = read_range(&staged, offset, size)
        {
            reply.data(&data);
            return;
        }

        if let Some(backing) = self.backing_path(&path)
            && let Ok(data) = read_range(&backing, offset, size)
        {
            reply.data(&data);
            return;
        }

        // Last resort: the rclone VFS cache file, only trusted when it is at
        // least as large as what the index last recorded — a conservative
        // stand-in for parsing rclone's own `vfsMeta` sidecar, which this
        // phase does not implement.
        let indexed_size = self
            .store
            .lock()
            .ok()
            .and_then(|s| s.node(&self.remote, &path).ok().flatten())
            .map(|n| n.size);
        if let Some(cache_path) = self.vfs_cache_candidate(&path)
            && let Ok(meta) = std::fs::metadata(&cache_path)
            && indexed_size.is_none_or(|expected| meta.len() >= expected)
            && let Ok(data) = read_range(&cache_path, offset, size)
        {
            reply.data(&data);
            return;
        }

        reply.error(Errno::ENETDOWN);
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let Some(path) = self.inodes.lock().unwrap().path_of(u64::from(ino)) else {
            reply.error(Errno::ENOENT);
            return;
        };

        // Already staged: subsequent writes to an offline edit land on the
        // staged copy, not the (unreachable) backing path.
        if let Some(staged) = self.outbox_staged(&path) {
            return write_target(&staged, data, offset, reply);
        }

        if let Some(local) = self.pin_local(&path) {
            return write_target(&local, data, offset, reply);
        }

        if let Some(backing) = self.backing_path(&path)
            && let Ok(file) = std::fs::OpenOptions::new().write(true).open(&backing)
        {
            return match file.write_at(data, offset) {
                Ok(n) => reply.written(n as u32),
                Err(_) => reply.error(Errno::EIO),
            };
        }

        // Backing is unreachable: stage the write into the outbox instead of
        // refusing it (§4.4). `stage_offline` seeds the staged file with the
        // best-known current content, so this call lays the specific
        // (offset, data) write on top of that seed.
        let Some(staged) = self.stage_offline(&path) else {
            reply.error(Errno::EROFS);
            return;
        };
        write_target(&staged, data, offset, reply);
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        let Some(parent_path) = self.inodes.lock().unwrap().path_of(u64::from(parent)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = join_rel(&parent_path, name);

        let target = self.pin_local(&path).or_else(|| self.backing_path(&path));
        let opened = target.as_ref().map(|target| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(target)
        });

        match opened {
            Some(Ok(_)) => self.reply_created(&path, reply),
            // A real name collision is never ours to paper over by staging a
            // duplicate, reachable or not.
            Some(Err(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                reply.error(Errno::EEXIST);
            }
            // Backing unreachable, or no route at all (outside any pin with
            // the mount down): stage a brand-new file instead of refusing it.
            _ => {
                if self.stage_offline_new(&path).is_some() {
                    self.reply_created(&path, reply);
                } else {
                    reply.error(Errno::EROFS);
                }
            }
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let Some(parent_path) = self.inodes.lock().unwrap().path_of(u64::from(parent)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(newparent_path) = self.inodes.lock().unwrap().path_of(u64::from(newparent)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let (Some(name), Some(newname)) = (name.to_str(), newname.to_str()) else {
            reply.error(Errno::EINVAL);
            return;
        };

        let from = join_rel(&parent_path, name);
        let to = join_rel(&newparent_path, newname);

        // Each side resolves to a route independently (pin or backing); a
        // rename across routes has no single filesystem call that could do
        // it atomically, so it is refused rather than approximated with a
        // copy-then-delete that could lose data on a crash mid-way.
        let from_pin = self.pin_local(&from);
        let to_pin = self.pin_local(&to);
        let target = match (from_pin, to_pin) {
            (Some(f), Some(t)) => Some((f, t)),
            (None, None) => match (self.backing_path(&from), self.backing_path(&to)) {
                (Some(f), Some(t)) => Some((f, t)),
                _ => None,
            },
            _ => {
                reply.error(Errno::EXDEV);
                return;
            }
        };

        let Some((from_target, to_target)) = target else {
            reply.error(Errno::EROFS);
            return;
        };

        match std::fs::rename(&from_target, &to_target) {
            Ok(()) => {
                // The old path's inode now means the new path; if anyone
                // still holds the old one, keeping both names resolvable to
                // the same object is more surprising than useful here, so the
                // table is simply updated in place.
                let mut inodes = self.inodes.lock().unwrap();
                if let Some(ino) = inodes.path_to_ino.remove(&from) {
                    inodes.path_to_ino.insert(to.clone(), ino);
                    inodes.ino_to_path.insert(ino, to);
                }
                reply.ok();
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => reply.error(Errno::ENOENT),
            Err(_) => reply.error(Errno::EROFS),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        self.structural(parent, name, reply_as_entry(reply), |target| {
            std::fs::create_dir(target)
        });
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.structural_empty(parent, name, reply, |p| std::fs::remove_file(p));
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.structural_empty(parent, name, reply, |p| std::fs::remove_dir(p));
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let Some(path) = self.inodes.lock().unwrap().path_of(u64::from(ino)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(value) = self.xattr_value(&path, name) else {
            reply.error(Errno::ENODATA);
            return;
        };
        if size == 0 {
            reply.size(value.len() as u32);
        } else if (value.len() as u32) <= size {
            reply.data(value.as_bytes());
        } else {
            reply.error(Errno::ERANGE);
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let names = [
            "user.onedrive.state\0",
            "user.onedrive.size\0",
            "user.xdg.tags\0",
        ];
        let joined: String = names.concat();
        let _ = ino;
        if size == 0 {
            reply.size(joined.len() as u32);
        } else if (joined.len() as u32) <= size {
            reply.data(joined.as_bytes());
        } else {
            reply.error(Errno::ERANGE);
        }
    }
}

impl Odfs {
    /// Shared body for `mkdir`/structural ops that return a `ReplyEntry`.
    fn structural(
        &self,
        parent: INodeNo,
        name: &OsStr,
        reply: ReplyEntryLike,
        op: impl FnOnce(&Path) -> std::io::Result<()>,
    ) {
        let Some(parent_path) = self.inodes.lock().unwrap().path_of(u64::from(parent)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = join_rel(&parent_path, name);
        let target = self.pin_local(&path).or_else(|| self.backing_path(&path));
        let Some(target) = target else {
            reply.error(Errno::EROFS);
            return;
        };

        match op(&target) {
            Ok(()) => {
                let ino = self.inodes.lock().unwrap().ino_for(&path);
                match self.resolve(&path) {
                    Some(resolved) => reply.entry(&self.attr_for(ino, &path, &resolved)),
                    None => reply.error(Errno::EIO),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => reply.error(Errno::EEXIST),
            Err(_) => reply.error(Errno::EROFS),
        }
    }

    fn structural_empty(
        &self,
        parent: INodeNo,
        name: &OsStr,
        reply: ReplyEmpty,
        op: impl FnOnce(&Path) -> std::io::Result<()>,
    ) {
        let Some(parent_path) = self.inodes.lock().unwrap().path_of(u64::from(parent)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = join_rel(&parent_path, name);
        let target = self.pin_local(&path).or_else(|| self.backing_path(&path));
        let Some(target) = target else {
            reply.error(Errno::EROFS);
            return;
        };

        match op(&target) {
            Ok(()) => reply.ok(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => reply.error(Errno::ENOENT),
            Err(_) => reply.error(Errno::EROFS),
        }
    }

    /// Synthesised extended attributes: `user.onedrive.state` /
    /// `user.onedrive.size` / `user.xdg.tags`, the latter a mirror of state so
    /// Dolphin's Tags column shows cached-vs-cloud with no plugin (§5).
    fn xattr_value(&self, path: &str, name: &OsStr) -> Option<String> {
        let name = name.to_str()?;
        let state = self.onedrive_state(path);
        match name {
            "user.onedrive.state" => Some(state.to_string()),
            "user.xdg.tags" => Some(format!("onedrive-{state}")),
            "user.onedrive.size" => self
                .resolve(path)
                .map(|r| match r {
                    NodeRecordOrLive::Indexed(n) => n.size,
                    NodeRecordOrLive::Live(n) => n.size,
                })
                .map(|s| s.to_string()),
            _ => None,
        }
    }

    /// Mount this filesystem. Blocks the calling thread until unmounted; the
    /// caller is expected to run it on its own thread, same as `rclone mount`
    /// runs in its own process.
    pub fn mount(self, mount_point: &Path) -> std::io::Result<()> {
        let mut config = fuser::Config::default();
        config
            .mount_options
            .push(fuser::MountOption::FSName("onedrive-rc".into()));
        fuser::mount(self, mount_point, &config)
    }

    /// Mount in the background, for callers (the supervisor, tests) that need
    /// to keep running while it serves. Dropping the returned session
    /// unmounts.
    pub fn spawn(self, mount_point: &Path) -> std::io::Result<fuser::BackgroundSession> {
        let mut config = fuser::Config::default();
        config
            .mount_options
            .push(fuser::MountOption::FSName("onedrive-rc".into()));
        fuser::spawn_mount(self, mount_point, &config)
    }

    fn onedrive_state(&self, path: &str) -> &'static str {
        if self.pin_local(path).is_some() {
            return "pinned";
        }
        if let Some(cache) = self.vfs_cache_candidate(path)
            && cache.exists()
        {
            return "cached";
        }
        "cloud"
    }
}

/// A tiny wrapper so [`Odfs::structural`] can serve both `mkdir`'s
/// `ReplyEntry` and (in principle) other entry-shaped replies with one body.
struct ReplyEntryLike(ReplyEntry, Duration);

fn reply_as_entry(reply: ReplyEntry) -> ReplyEntryLike {
    ReplyEntryLike(reply, TTL)
}

impl ReplyEntryLike {
    fn entry(self, attr: &FileAttr) {
        self.0.entry(&self.1, attr, Generation(0));
    }
    fn error(self, err: Errno) {
        self.0.error(err);
    }
}

/// Opens `target` for writing and applies one `(offset, data)` write,
/// replying on its behalf. Shared by every branch of [`Odfs::write`] — pin,
/// backing, and outbox-staged all write the same way, they only differ in
/// which path they resolve to.
fn write_target(target: &Path, data: &[u8], offset: u64, reply: ReplyWrite) {
    match std::fs::OpenOptions::new().write(true).open(target) {
        Ok(file) => match file.write_at(data, offset) {
            Ok(n) => reply.written(n as u32),
            Err(_) => reply.error(Errno::EIO),
        },
        Err(_) => reply.error(Errno::EROFS),
    }
}

fn read_range(path: &Path, offset: u64, size: u32) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if offset >= len {
        return Ok(Vec::new());
    }
    let want = (size as u64).min(len - offset) as usize;
    let mut buf = vec![0u8; want];
    file.read_exact_at(&mut buf, offset)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_rel_handles_the_root() {
        assert_eq!(join_rel("", "a.txt"), "a.txt");
        assert_eq!(join_rel("Keepass", "a.txt"), "Keepass/a.txt");
    }

    #[test]
    fn split_for_attr_finds_parent_and_name() {
        assert_eq!(split_for_attr("a.txt"), ("", "a.txt"));
        assert_eq!(split_for_attr("Keepass/a.txt"), ("Keepass", "a.txt"));
    }

    #[test]
    fn inode_table_assigns_stable_ids_and_starts_at_the_root() {
        let mut t = InodeTable::new();
        assert_eq!(t.path_of(1).as_deref(), Some(""));

        let a = t.ino_for("a.txt");
        let a_again = t.ino_for("a.txt");
        let b = t.ino_for("b.txt");
        assert_eq!(a, a_again, "the same path must keep the same inode");
        assert_ne!(a, b);
        assert_ne!(a, 1, "must not collide with the root");
        assert_eq!(t.path_of(a).as_deref(), Some("a.txt"));
    }

    #[test]
    fn a_pin_covering_the_path_resolves_under_its_local_root() {
        let pin = PinRoute {
            remote_path: "Dokumente/Keepass".into(),
            local: PathBuf::from("/home/t/onedrive-keepass"),
        };
        assert_eq!(
            pin.resolve("Dokumente/Keepass/Timm.kdbx"),
            Some(PathBuf::from("/home/t/onedrive-keepass/Timm.kdbx"))
        );
        assert_eq!(
            pin.resolve("Dokumente/Keepass"),
            Some(PathBuf::from("/home/t/onedrive-keepass"))
        );
        assert_eq!(pin.resolve("Dokumente/Other"), None);
        assert_eq!(pin.resolve("Dokumente/KeepassExtra/x"), None);
    }

    /// The bug a naive `starts_with` on raw strings would produce: a pin at
    /// "Keepass" must not also claim "KeepassExtra".
    #[test]
    fn pin_matching_respects_path_boundaries() {
        let pin = PinRoute {
            remote_path: "Keepass".into(),
            local: PathBuf::from("/local"),
        };
        assert!(pin.resolve("KeepassExtra/file").is_none());
        assert!(pin.resolve("KeepassExtra").is_none());
    }

    #[test]
    fn a_pin_rooted_at_the_remote_root_covers_everything() {
        let pin = PinRoute {
            remote_path: String::new(),
            local: PathBuf::from("/local"),
        };
        assert_eq!(
            pin.resolve("a/b.txt"),
            Some(PathBuf::from("/local/a/b.txt"))
        );
    }

    #[test]
    fn pin_resolution_refuses_to_escape_its_local_root() {
        let pin = PinRoute {
            remote_path: "Keepass".into(),
            local: PathBuf::from("/local"),
        };
        // Can't actually occur via FUSE path components, but the guard must
        // hold regardless of how a path string was built.
        assert_eq!(pin.resolve("Keepass/../../etc/passwd"), None);
    }

    #[tokio::test]
    async fn a_live_backing_read_is_folded_into_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("backing");
        std::fs::create_dir_all(&backing).unwrap();
        std::fs::write(backing.join("notes.txt"), "hello").unwrap();

        let paths = Paths::rooted(dir.path().join("home"));
        let remote: onedrive_rc_core::config::Config = toml::from_str(&format!(
            r#"
            [[remote]]
            name = "od"
            mount_point = "{}/OneDrive"
            "#,
            dir.path().display()
        ))
        .unwrap();

        let fs = Odfs::new(&remote.remotes[0], &paths).unwrap();
        // Point the backing mount at our tempdir directly rather than the
        // real `.cache/onedrive-rc/od/mount` layout, matching how the backing
        // mount would appear once P1's supervisor actually mounts there.
        let fs = Odfs { backing, ..fs };

        let resolved = fs.resolve("notes.txt").expect("must resolve live");
        assert!(matches!(resolved, NodeRecordOrLive::Live(_)));

        // And it must now be in the index too, for when backing disappears.
        let node = fs.store.lock().unwrap().node("od", "notes.txt").unwrap();
        assert!(node.is_some(), "a live hit must be recorded into the index");
    }

    #[tokio::test]
    async fn once_backing_is_gone_the_index_still_answers() {
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("backing");
        std::fs::create_dir_all(&backing).unwrap();
        std::fs::write(backing.join("notes.txt"), "hello").unwrap();

        let paths = Paths::rooted(dir.path().join("home"));
        let remote: onedrive_rc_core::config::Config = toml::from_str(&format!(
            r#"
            [[remote]]
            name = "od"
            mount_point = "{}/OneDrive"
            "#,
            dir.path().display()
        ))
        .unwrap();

        let fs = Odfs::new(&remote.remotes[0], &paths).unwrap();
        let fs = Odfs {
            backing: backing.clone(),
            ..fs
        };
        fs.resolve("notes.txt"); // warms the index

        // The backing mount vanishes (simulating it going down).
        std::fs::remove_dir_all(&backing).unwrap();

        let resolved = fs
            .resolve("notes.txt")
            .expect("the index must still know it");
        assert!(matches!(resolved, NodeRecordOrLive::Indexed(_)));
    }

    #[tokio::test]
    async fn a_pin_path_resolves_from_its_local_directory_even_with_backing_gone() {
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("backing");
        let pin_local = dir.path().join("onedrive-keepass");
        std::fs::create_dir_all(&backing).unwrap();
        std::fs::create_dir_all(&pin_local).unwrap();
        std::fs::write(pin_local.join("Timm.kdbx"), "entries").unwrap();

        let paths = Paths::rooted(dir.path().join("home"));
        let remote: onedrive_rc_core::config::Config = toml::from_str(&format!(
            r#"
            [[remote]]
            name = "od"
            mount_point = "{root}/OneDrive"
            [[remote.pin]]
            name = "Keepass"
            remote_path = "Dokumente/Keepass"
            local_path = "{local}"
            "#,
            root = dir.path().display(),
            local = pin_local.display(),
        ))
        .unwrap();

        let fs = Odfs::new(&remote.remotes[0], &paths).unwrap();
        let fs = Odfs { backing, ..fs };

        let resolved = fs
            .resolve("Dokumente/Keepass/Timm.kdbx")
            .expect("a pin must resolve even with no backing mount at all");
        assert!(matches!(resolved, NodeRecordOrLive::Live(_)));
    }

    #[test]
    fn xattrs_report_pinned_vs_cloud_and_the_size() {
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("backing");
        let pin_local = dir.path().join("onedrive-keepass");
        std::fs::create_dir_all(&backing).unwrap();
        std::fs::create_dir_all(&pin_local).unwrap();
        std::fs::write(backing.join("notes.txt"), "hello").unwrap();
        std::fs::write(pin_local.join("Timm.kdbx"), "entries!!").unwrap();

        let paths = Paths::rooted(dir.path().join("home"));
        let remote: onedrive_rc_core::config::Config = toml::from_str(&format!(
            r#"
            [[remote]]
            name = "od"
            mount_point = "{root}/OneDrive"
            [[remote.pin]]
            name = "Keepass"
            remote_path = "Dokumente/Keepass"
            local_path = "{local}"
            "#,
            root = dir.path().display(),
            local = pin_local.display(),
        ))
        .unwrap();
        let fs = Odfs::new(&remote.remotes[0], &paths).unwrap();
        let fs = Odfs { backing, ..fs };

        assert_eq!(
            fs.xattr_value(
                "Dokumente/Keepass/Timm.kdbx",
                OsStr::new("user.onedrive.state")
            ),
            Some("pinned".to_string())
        );
        assert_eq!(
            fs.xattr_value("notes.txt", OsStr::new("user.onedrive.state")),
            Some("cloud".to_string())
        );
        assert_eq!(
            fs.xattr_value("notes.txt", OsStr::new("user.xdg.tags")),
            Some("onedrive-cloud".to_string())
        );
        assert_eq!(
            fs.xattr_value("notes.txt", OsStr::new("user.onedrive.size")),
            Some("5".to_string())
        );
        assert_eq!(
            fs.xattr_value("notes.txt", OsStr::new("user.unrelated")),
            None
        );
    }

    /// Waits for `mount_point` to actually be mounted — `spawn_mount` returns
    /// once the kernel accepts the mount, but a listing done in the very next
    /// instruction has occasionally beaten the session thread in CI.
    fn wait_for_mount(mount_point: &Path) {
        for _ in 0..50 {
            if std::fs::read_dir(mount_point).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("mount at {} never became ready", mount_point.display());
    }

    #[test]
    fn a_real_mount_lists_and_reads_the_backing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("backing");
        let mount_point = dir.path().join("mnt");
        std::fs::create_dir_all(&backing).unwrap();
        std::fs::create_dir_all(&mount_point).unwrap();
        std::fs::write(backing.join("notes.txt"), "hello from backing").unwrap();
        std::fs::create_dir_all(backing.join("sub")).unwrap();

        let paths = Paths::rooted(dir.path().join("home"));
        let remote: onedrive_rc_core::config::Config = toml::from_str(&format!(
            r#"[[remote]]
            name = "od"
            mount_point = "{root}/OneDrive""#,
            root = dir.path().display(),
        ))
        .unwrap();
        let fs = Odfs::new(&remote.remotes[0], &paths).unwrap();
        let fs = Odfs { backing, ..fs };

        let session = fs.spawn(&mount_point).expect("mounting must succeed");
        wait_for_mount(&mount_point);

        let mut names: Vec<String> = std::fs::read_dir(&mount_point)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["notes.txt", "sub"]);

        let content = std::fs::read_to_string(mount_point.join("notes.txt")).unwrap();
        assert_eq!(content, "hello from backing");

        drop(session);
    }

    /// The acceptance case from the plan's §12 offline checklist: a listing
    /// obtained once stays available through the *same* mount after the
    /// backing directory disappears, because the live readdir warmed the
    /// index on the way through.
    #[test]
    fn readdir_survives_the_backing_directory_disappearing() {
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("backing");
        let mount_point = dir.path().join("mnt");
        std::fs::create_dir_all(&backing).unwrap();
        std::fs::create_dir_all(&mount_point).unwrap();
        std::fs::write(backing.join("notes.txt"), "entries").unwrap();

        let paths = Paths::rooted(dir.path().join("home"));
        let remote: onedrive_rc_core::config::Config = toml::from_str(&format!(
            r#"[[remote]]
            name = "od"
            mount_point = "{root}/OneDrive""#,
            root = dir.path().display(),
        ))
        .unwrap();
        let fs = Odfs::new(&remote.remotes[0], &paths).unwrap();
        // A real rclone mount would have already downloaded this into its VFS
        // cache the first time it was read; reproduce that side effect so the
        // fallback path is exercised against the real layout, not a guess.
        let cache_file = paths.vfs_cached_file("od", "notes.txt").unwrap();
        std::fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        std::fs::write(&cache_file, "entries").unwrap();
        let fs = Odfs {
            backing: backing.clone(),
            ..fs
        };

        let session = fs.spawn(&mount_point).expect("mounting must succeed");
        wait_for_mount(&mount_point);

        // Warm the index via a live listing.
        let warm: Vec<_> = std::fs::read_dir(&mount_point).unwrap().collect();
        assert_eq!(warm.len(), 1);

        // The backing directory is gone — the offline case.
        std::fs::remove_dir_all(&backing).unwrap();

        let names: Vec<String> = std::fs::read_dir(&mount_point)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["notes.txt"], "the index must still list it");

        let content = std::fs::read_to_string(mount_point.join("notes.txt")).unwrap();
        assert_eq!(content, "entries", "and the VFS cache must still serve it");

        drop(session);
    }

    /// §4.4's offline-write case for a file the index already knows about: the
    /// write lands in the outbox instead of failing, is served straight back
    /// out on the very next read, and records a baseline against the content
    /// seen while still online.
    #[test]
    fn an_offline_write_to_a_known_file_is_staged_and_served_on_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("backing");
        let mount_point = dir.path().join("mnt");
        std::fs::create_dir_all(&backing).unwrap();
        std::fs::create_dir_all(&mount_point).unwrap();
        std::fs::write(backing.join("notes.txt"), "hello").unwrap();

        let paths = Paths::rooted(dir.path().join("home"));
        let remote: onedrive_rc_core::config::Config = toml::from_str(&format!(
            r#"[[remote]]
            name = "od"
            mount_point = "{root}/OneDrive""#,
            root = dir.path().display(),
        ))
        .unwrap();
        let fs = Odfs::new(&remote.remotes[0], &paths).unwrap();
        let fs = Odfs {
            backing: backing.clone(),
            ..fs
        };

        let session = fs.spawn(&mount_point).expect("mounting must succeed");
        wait_for_mount(&mount_point);

        // Warm the index with the online content, then go offline.
        let warm = std::fs::read_to_string(mount_point.join("notes.txt")).unwrap();
        assert_eq!(warm, "hello");
        std::fs::remove_dir_all(&backing).unwrap();

        std::fs::OpenOptions::new()
            .write(true)
            .open(mount_point.join("notes.txt"))
            .and_then(|mut f| std::io::Write::write_all(&mut f, b"bye"))
            .expect("an offline write must stage, not fail");

        let content = std::fs::read_to_string(mount_point.join("notes.txt")).unwrap();
        assert_eq!(content, "bye", "a read-back must see our own staged edit");

        // `fs` was consumed by `spawn`; reopen the same on-disk state.db to
        // inspect what the write left behind.
        let store = Store::open(&paths.state_db()).unwrap();
        let row = store
            .outbox_for("od", "notes.txt")
            .unwrap()
            .expect("the write must be queued in the outbox");
        assert_eq!(
            row.base_size,
            Some(5),
            "the baseline must be the content last seen online, not the edit"
        );

        drop(session);
    }

    /// §4.4's other offline-write case: a file created entirely offline has no
    /// backing and no index row at all, yet it must still be readable back and
    /// visible in a directory listing — the only way to see it is the outbox.
    #[test]
    fn a_file_created_entirely_offline_is_readable_and_shows_up_in_readdir() {
        let dir = tempfile::tempdir().unwrap();
        // Deliberately never created: nothing backs this remote at all.
        let backing = dir.path().join("backing-does-not-exist");
        let mount_point = dir.path().join("mnt");
        std::fs::create_dir_all(&mount_point).unwrap();

        let paths = Paths::rooted(dir.path().join("home"));
        let remote: onedrive_rc_core::config::Config = toml::from_str(&format!(
            r#"[[remote]]
            name = "od"
            mount_point = "{root}/OneDrive""#,
            root = dir.path().display(),
        ))
        .unwrap();
        let fs = Odfs::new(&remote.remotes[0], &paths).unwrap();
        let fs = Odfs { backing, ..fs };

        let session = fs.spawn(&mount_point).expect("mounting must succeed");
        wait_for_mount(&mount_point);

        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(mount_point.join("new.txt"))
            .and_then(|mut f| std::io::Write::write_all(&mut f, b"offline content"))
            .expect("creating a new file offline must stage, not fail");

        let content = std::fs::read_to_string(mount_point.join("new.txt")).unwrap();
        assert_eq!(content, "offline content");

        let names: Vec<String> = std::fs::read_dir(&mount_point)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["new.txt"],
            "a file that exists only in the outbox must still be listed"
        );

        drop(session);
    }

    /// With no VFS cache for a path at all, an offline read is refused rather
    /// than inventing content — the honest answer when nothing ever cached it.
    #[test]
    fn a_read_with_nothing_cached_at_all_fails_rather_than_inventing_content() {
        // Print fuser's own warnings with a failure, e.g. "Reply not sent for
        // operation N, replying with I/O error", which is how a handler that
        // dropped its reply turns into a bare EIO. Without this the failure
        // seen on CI shows the errno but not why.
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::WARN)
            .try_init();

        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("backing");
        let mount_point = dir.path().join("mnt");
        std::fs::create_dir_all(&backing).unwrap();
        std::fs::create_dir_all(&mount_point).unwrap();
        std::fs::write(backing.join("notes.txt"), "entries").unwrap();

        let paths = Paths::rooted(dir.path().join("home"));
        let remote: onedrive_rc_core::config::Config = toml::from_str(&format!(
            r#"[[remote]]
            name = "od"
            mount_point = "{root}/OneDrive""#,
            root = dir.path().display(),
        ))
        .unwrap();
        let fs = Odfs::new(&remote.remotes[0], &paths).unwrap();
        let fs = Odfs {
            backing: backing.clone(),
            ..fs
        };

        let session = fs.spawn(&mount_point).expect("mounting must succeed");
        wait_for_mount(&mount_point);

        // List (warms the index with size/mtime, no content), then go offline
        // without ever having read the content through this mount.
        let _ = std::fs::read_dir(&mount_point).unwrap().collect::<Vec<_>>();
        std::fs::remove_dir_all(&backing).unwrap();

        let err = std::fs::read_to_string(mount_point.join("notes.txt")).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENETDOWN), "{err:?}");

        drop(session);
    }

    /// Structural writes degrade to `EROFS` rather than silently succeeding or
    /// hanging once the backing mount cannot be reached at all — P4's outbox
    /// is what will turn this into a queued write; until then, refusing is
    /// the safe default.
    #[test]
    fn file_creation_stages_offline_but_mkdir_is_still_refused() {
        let dir = tempfile::tempdir().unwrap();
        // Deliberately never created: nothing backs this remote at all.
        let backing = dir.path().join("backing-does-not-exist");
        let mount_point = dir.path().join("mnt");
        std::fs::create_dir_all(&mount_point).unwrap();

        let paths = Paths::rooted(dir.path().join("home"));
        let remote: onedrive_rc_core::config::Config = toml::from_str(&format!(
            r#"[[remote]]
            name = "od"
            mount_point = "{root}/OneDrive""#,
            root = dir.path().display(),
        ))
        .unwrap();
        let fs = Odfs::new(&remote.remotes[0], &paths).unwrap();
        let fs = Odfs { backing, ..fs };

        let session = fs.spawn(&mount_point).expect("mounting must succeed");
        wait_for_mount(&mount_point);

        // A brand-new file is staged into the outbox rather than refused
        // (§4.4) — this is the one write-shaped op that is not structural.
        std::fs::File::create(mount_point.join("new.txt"))
            .expect("create stages offline instead of failing");

        let err = std::fs::create_dir(mount_point.join("new-dir")).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EROFS), "{err:?}");

        drop(session);
    }
}
