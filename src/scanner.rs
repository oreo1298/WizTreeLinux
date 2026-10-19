//! Fast, parallel directory scanner.
//!
//! The scanner works on any filesystem the kernel can mount (ext4, btrfs,
//! xfs, zfs, f2fs, bcachefs, ntfs3, exfat, vfat, NFS, SMB, ...) because it
//! only relies on generic syscalls, chosen to do as little work per entry as
//! possible:
//!
//! * directories are read with raw `getdents64` into a reusable buffer;
//! * every entry is examined with one `statx` relative to its parent's file
//!   descriptor (no path resolution, no automount, no forced sync on network
//!   filesystems);
//! * sub-directories are processed in parallel on a work-stealing pool.
//!
//! Hidden files are not special in any way: everything `getdents64` returns
//! is recorded. Symlinks are never followed. Hard links are counted once.

use crate::mounts::{self, Mount};
use crate::tree::{flag, Kind, NodeData, NodeId, ScanInfo, Tree, TreeBuilder, NONE};
use rayon::prelude::*;
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawDir, StatxAttributes, StatxFlags};
use rustix::io::Errno;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CStr, OsStr};
use std::fmt;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;

#[derive(Clone, Copy, Debug)]
pub struct ScanOptions {
    /// Do not descend into other mounted filesystems.
    pub one_filesystem: bool,
    /// Worker threads; 0 picks a default based on the CPU count.
    pub threads: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions { one_filesystem: true, threads: 0 }
    }
}

/// Live counters, updated by the scanner and read by the UI.
#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    /// Allocated bytes found so far.
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
    cancel: AtomicBool,
    current: Mutex<PathBuf>,
}

impl Progress {
    pub fn cancel(&self) {
        self.cancel.store(true, Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Relaxed)
    }

    /// A recently visited directory, for display.
    pub fn current(&self) -> PathBuf {
        self.current.lock().map(|p| p.clone()).unwrap_or_default()
    }

    pub fn set_current(&self, path: &Path) {
        if let Ok(mut cur) = self.current.try_lock() {
            cur.clear();
            cur.push(path);
        }
    }
}

#[derive(Debug)]
pub enum ScanError {
    Cancelled,
    Failed(String),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::Cancelled => f.write_str("The scan was cancelled"),
            ScanError::Failed(msg) => f.write_str(msg),
        }
    }
}

const MAX_RECORDED_ERRORS: usize = 500;
const DIRENT_BUF_SIZE: usize = 64 * 1024;
const HARDLINK_SHARDS: usize = 64;
/// Deeper directories are reported as unreadable instead of risking the
/// worker threads' stacks. Real systems never get close to this.
const MAX_DEPTH: u32 = 2048;

pub fn default_threads() -> usize {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    // Metadata reads block on I/O, so oversubscribing the CPUs keeps the
    // disk queue full on cold caches without hurting warm-cache scans.
    (cpus * 2).clamp(4, 64)
}

/// Raises the soft open-file limit to the hard limit; the scanner keeps one
/// descriptor open per directory level on every worker.
pub fn raise_fd_limit() {
    use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};
    let lim = getrlimit(Resource::Nofile);
    if let (Some(cur), max) = (lim.current, lim.maximum) {
        let target = max.unwrap_or(1 << 20).min(1 << 20);
        if target > cur {
            let _ = setrlimit(Resource::Nofile, Rlimit { current: Some(target), maximum: max });
        }
    }
}

pub fn error_message(err: Errno) -> String {
    let msg = std::io::Error::from_raw_os_error(err.raw_os_error()).to_string();
    match msg.find(" (os error") {
        Some(i) => msg[..i].to_string(),
        None => msg,
    }
}

pub(crate) struct Meta {
    pub kind: Kind,
    size: u64,
    alloc: u64,
    mtime: i64,
    nlink: u64,
    ino: u64,
    pub dev: u64,
    mnt_id: Option<u64>,
    pub mount_root: Option<bool>,
}

impl Meta {
    /// Whether this directory is the root of a different mount than a
    /// parent on device `parent_dev`.
    pub fn is_mount_boundary(&self, parent_dev: u64) -> bool {
        self.mount_root.unwrap_or(self.dev != parent_dev)
    }
}

static STATX_UNSUPPORTED: AtomicBool = AtomicBool::new(false);

const STATX_MASK: StatxFlags = StatxFlags::TYPE
    .union(StatxFlags::MODE)
    .union(StatxFlags::NLINK)
    .union(StatxFlags::INO)
    .union(StatxFlags::SIZE)
    .union(StatxFlags::BLOCKS)
    .union(StatxFlags::MTIME)
    .union(StatxFlags::MNT_ID);

fn kind_from_type(t: FileType) -> Kind {
    match t {
        FileType::RegularFile => Kind::File,
        FileType::Directory => Kind::Dir,
        FileType::Symlink => Kind::Symlink,
        _ => Kind::Other,
    }
}

fn make_dev(major: u32, minor: u32) -> u64 {
    ((major as u64) << 32) | minor as u64
}

fn meta_from_statx(st: &rustix::fs::Statx) -> Meta {
    let mount_root = st
        .stx_attributes_mask
        .contains(StatxAttributes::MOUNT_ROOT)
        .then(|| st.stx_attributes.contains(StatxAttributes::MOUNT_ROOT));
    Meta {
        kind: kind_from_type(FileType::from_raw_mode(st.stx_mode.into())),
        size: st.stx_size,
        alloc: st.stx_blocks.saturating_mul(512),
        mtime: st.stx_mtime.tv_sec,
        nlink: st.stx_nlink as u64,
        ino: st.stx_ino,
        dev: make_dev(st.stx_dev_major, st.stx_dev_minor),
        mnt_id: (st.stx_mask & StatxFlags::MNT_ID.bits() != 0).then_some(st.stx_mnt_id),
        mount_root,
    }
}

#[allow(clippy::unnecessary_cast)]
fn meta_from_stat(st: &rustix::fs::Stat) -> Meta {
    let dev = st.st_dev as u64;
    Meta {
        kind: kind_from_type(FileType::from_raw_mode(st.st_mode as _)),
        size: st.st_size.max(0) as u64,
        alloc: (st.st_blocks.max(0) as u64).saturating_mul(512),
        mtime: st.st_mtime as i64,
        nlink: st.st_nlink as u64,
        ino: st.st_ino as u64,
        dev: make_dev(rustix::fs::major(dev), rustix::fs::minor(dev)),
        mnt_id: None,
        mount_root: None,
    }
}

/// `stat` of `name` inside `dirfd`, without following symlinks or
/// triggering automounts.
pub(crate) fn stat_at(dirfd: BorrowedFd<'_>, name: &CStr) -> Result<Meta, Errno> {
    if !STATX_UNSUPPORTED.load(Relaxed) {
        let flags = AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT | AtFlags::STATX_DONT_SYNC;
        match rustix::fs::statx(dirfd, name, flags, STATX_MASK) {
            Ok(st) => return Ok(meta_from_statx(&st)),
            Err(Errno::NOSYS) => STATX_UNSUPPORTED.store(true, Relaxed),
            Err(e) => return Err(e),
        }
    }
    let st = rustix::fs::statat(dirfd, name, AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT)?;
    Ok(meta_from_stat(&st))
}

fn stat_fd(fd: BorrowedFd<'_>) -> Result<Meta, Errno> {
    if !STATX_UNSUPPORTED.load(Relaxed) {
        match rustix::fs::statx(fd, c"", AtFlags::EMPTY_PATH, STATX_MASK) {
            Ok(st) => return Ok(meta_from_statx(&st)),
            Err(Errno::NOSYS) => STATX_UNSUPPORTED.store(true, Relaxed),
            Err(e) => return Err(e),
        }
    }
    Ok(meta_from_stat(&rustix::fs::fstat(fd)?))
}

pub(crate) fn open_dir(parent: BorrowedFd<'_>, name: &CStr) -> Result<OwnedFd, Errno> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY;
    let mut attempts = 0;
    loop {
        match rustix::fs::openat(parent, name, flags, Mode::empty()) {
            Err(Errno::INTR) => continue,
            // Out of descriptors: other workers release theirs as they
            // finish, so wait a moment instead of failing.
            Err(Errno::MFILE | Errno::NFILE) if attempts < 200 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            other => return other,
        }
    }
}

thread_local! {
    static DIRENT_BUF: RefCell<Vec<MaybeUninit<u8>>> =
        RefCell::new(vec![MaybeUninit::uninit(); DIRENT_BUF_SIZE]);
}

/// One directory entry while scanning.
struct Entry {
    sub: Option<Box<DirScan>>,
    size: u64,
    alloc: u64,
    mtime: i64,
    files: u32,
    dirs: u32,
    /// Offset of the NUL-terminated name in the parent's `names`.
    name_off: u32,
    name_len: u16,
    kind: Kind,
    flags: u8,
}

/// Scanned contents of one directory.
struct DirScan {
    /// NUL-terminated names of all entries.
    names: Vec<u8>,
    entries: Vec<Entry>,
}

#[derive(Default)]
struct Totals {
    size: u64,
    alloc: u64,
    files: u32,
    dirs: u32,
}

struct Ctx<'a> {
    opts: ScanOptions,
    progress: &'a Progress,
    root: PathBuf,
    mounts: Vec<Mount>,
    mounts_by_id: HashMap<u64, usize>,
    /// Mounts whose contents are part of this scan.
    entered: Mutex<Vec<Mount>>,
    hardlinks: Vec<Mutex<HashSet<(u64, u64)>>>,
    errors: Mutex<Vec<(PathBuf, String)>>,
    name_bytes: AtomicU64,
    tick: AtomicU32,
}

impl Ctx<'_> {
    fn error(&self, path: &Path, err: Errno) {
        self.progress.errors.fetch_add(1, Relaxed);
        if let Ok(mut errors) = self.errors.lock() {
            if errors.len() < MAX_RECORDED_ERRORS {
                errors.push((path.to_path_buf(), error_message(err)));
            }
        }
    }

    /// Records an inode with several links; returns false if it was
    /// already counted through another link.
    fn first_link(&self, dev: u64, ino: u64) -> bool {
        let shard = ((ino ^ dev.rotate_left(17)) as usize) % HARDLINK_SHARDS;
        self.hardlinks[shard].lock().map(|mut s| s.insert((dev, ino))).unwrap_or(true)
    }

    /// Counts an error without recording its details.
    fn count_error(&self) {
        self.progress.errors.fetch_add(1, Relaxed);
    }

    fn mount_of(&self, meta: &Meta, path: impl FnOnce() -> PathBuf) -> Option<&Mount> {
        if let Some(i) = meta.mnt_id.and_then(|id| self.mounts_by_id.get(&id)) {
            return Some(&self.mounts[*i]);
        }
        let path = path();
        self.mounts.iter().rev().find(|m| m.mount_point == path)
    }

    /// Decides whether to descend into another filesystem mounted inside the
    /// scan. Bind mounts of folders that are scanned anyway, container
    /// overlays and compressed images would only count data twice.
    fn should_enter(&self, mount: &Mount) -> bool {
        if matches!(mount.fstype.as_str(), "overlay" | "squashfs" | "erofs") {
            return false;
        }
        let Ok(mut entered) = self.entered.lock() else { return true };
        for e in entered.iter().filter(|e| e.dev == mount.dev) {
            if let Ok(rest) = Path::new(&mount.root).strip_prefix(&e.root) {
                if e.mount_point.join(rest).starts_with(&self.root) {
                    return false;
                }
            }
        }
        entered.push(mount.clone());
        true
    }
}

fn scan_dir(ctx: &Ctx, fd: OwnedFd, path: &Path, dev: u64, depth: u32) -> (Box<DirScan>, Totals, u8) {
    let mut scan = Box::new(DirScan { names: Vec::new(), entries: Vec::new() });
    if ctx.progress.is_cancelled() {
        return (scan, Totals::default(), 0);
    }

    let mut dir_flags = 0u8;
    let mut stat_failed = false;
    let mut subdirs: Vec<(usize, u64)> = Vec::new();
    let mut new_files = 0u64;
    let mut new_bytes = 0u64;
    let DirScan { names, entries } = &mut *scan;

    DIRENT_BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        let mut dir = RawDir::new(fd.as_fd(), &mut buf[..]);
        loop {
            let ent = match dir.next() {
                None => break,
                Some(Ok(ent)) => ent,
                Some(Err(Errno::INTR)) => continue,
                Some(Err(err)) => {
                    dir_flags |= flag::ERROR;
                    ctx.error(path, err);
                    break;
                }
            };
            let cname = ent.file_name();
            let bytes = cname.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let name_off = names.len();
            names.extend_from_slice(bytes);
            names.push(0);
            let mut e = Entry {
                sub: None,
                size: 0,
                alloc: 0,
                mtime: 0,
                files: 0,
                dirs: 0,
                name_off: name_off as u32,
                name_len: bytes.len().min(u16::MAX as usize) as u16,
                kind: kind_from_type(ent.file_type()),
                flags: 0,
            };
            match stat_at(fd.as_fd(), cname) {
                Ok(m) => {
                    e.kind = m.kind;
                    e.mtime = m.mtime;
                    e.alloc = m.alloc;
                    if m.kind == Kind::Dir {
                        let boundary = m.is_mount_boundary(dev);
                        if !boundary {
                            subdirs.push((entries.len(), m.dev));
                        } else if ctx.opts.one_filesystem {
                            e.flags |= flag::MOUNT;
                        } else {
                            match ctx.mount_of(&m, || path.join(OsStr::from_bytes(bytes))) {
                                Some(mount) if mounts::is_virtual_fs(&mount.fstype) => {
                                    e.flags |= flag::MOUNT | flag::VIRTUAL;
                                }
                                Some(mount) if !ctx.should_enter(mount) => e.flags |= flag::MOUNT,
                                _ => subdirs.push((entries.len(), m.dev)),
                            }
                        }
                    } else {
                        e.size = m.size;
                        new_files += 1;
                        if m.nlink > 1 {
                            e.flags |= flag::LINKED;
                        }
                        if m.nlink > 1 && !ctx.first_link(m.dev, m.ino) {
                            e.flags |= flag::HARDLINK;
                        } else {
                            new_bytes += m.alloc;
                        }
                    }
                }
                Err(err) => {
                    // Every failure counts; only the first per folder is listed.
                    if !stat_failed {
                        stat_failed = true;
                        dir_flags |= flag::ERROR;
                        ctx.error(&path.join(OsStr::from_bytes(bytes)), err);
                    } else {
                        ctx.count_error();
                    }
                    if e.kind == Kind::Dir {
                        e.flags |= flag::ERROR;
                    } else {
                        new_files += 1;
                    }
                }
            }
            entries.push(e);
        }
    });

    let p = ctx.progress;
    p.files.fetch_add(new_files, Relaxed);
    p.dirs.fetch_add(1, Relaxed);
    p.bytes.fetch_add(new_bytes, Relaxed);
    ctx.name_bytes.fetch_add((names.len() - entries.len()) as u64, Relaxed);
    if ctx.tick.fetch_add(1, Relaxed) % 64 == 0 {
        p.set_current(path);
    }

    // Recurse. The directory descriptor stays open so children can be
    // opened relative to it.
    let names_ref: &[u8] = names;
    let entries_ref: &[Entry] = entries;
    let work = |&(idx, cdev): &(usize, u64)| {
        let e = &entries_ref[idx];
        let off = e.name_off as usize;
        let len = e.name_len as usize;
        let cname = CStr::from_bytes_with_nul(&names_ref[off..=off + len]).ok();
        let cpath = path.join(OsStr::from_bytes(&names_ref[off..off + len]));
        if depth >= MAX_DEPTH {
            ctx.error(&cpath, Errno::NAMETOOLONG);
            return (idx, None);
        }
        match cname.map(|c| open_dir(fd.as_fd(), c)) {
            Some(Ok(cfd)) => (idx, Some(scan_dir(ctx, cfd, &cpath, cdev, depth + 1))),
            Some(Err(err)) => {
                ctx.error(&cpath, err);
                (idx, None)
            }
            None => (idx, None),
        }
    };
    let results: Vec<_> =
        if subdirs.len() <= 1 { subdirs.iter().map(work).collect() } else { subdirs.par_iter().map(work).collect() };
    drop(fd);

    for (idx, res) in results {
        let e = &mut entries[idx];
        match res {
            Some((sub, t, f)) => {
                e.size = t.size;
                e.alloc = e.alloc.saturating_add(t.alloc);
                e.files = t.files;
                e.dirs = t.dirs;
                e.flags |= f;
                if !sub.entries.is_empty() {
                    e.sub = Some(sub);
                }
            }
            None => e.flags |= flag::ERROR,
        }
    }

    let mut t = Totals::default();
    for e in entries.iter() {
        if e.flags & flag::HARDLINK == 0 {
            t.size = t.size.saturating_add(e.size);
            t.alloc = t.alloc.saturating_add(e.alloc);
        }
        if e.kind == Kind::Dir {
            t.dirs = t.dirs.saturating_add(1).saturating_add(e.dirs);
            t.files = t.files.saturating_add(e.files);
        } else {
            t.files = t.files.saturating_add(1);
        }
    }
    names.shrink_to_fit();
    entries.shrink_to_fit();
    (scan, t, dir_flags)
}

/// Moves the scanned directories into the flat arena, breadth first so that
/// the children of every directory end up next to each other, largest first.
fn flatten(b: &mut TreeBuilder, root: NodeId, scan: Box<DirScan>) {
    let mut queue = VecDeque::new();
    queue.push_back((root, scan));
    while let Some((id, scan)) = queue.pop_front() {
        let DirScan { names, mut entries } = *scan;
        entries.sort_unstable_by(|a, b| b.alloc.cmp(&a.alloc).then(b.size.cmp(&a.size)));
        let first = b.len() as NodeId;
        let count = entries.len() as u32;
        for e in entries {
            let off = e.name_off as usize;
            let name = &names[off..off + e.name_len as usize];
            let data = NodeData {
                kind: e.kind,
                flags: e.flags,
                size: e.size,
                alloc: e.alloc,
                mtime: e.mtime,
                files: e.files,
                dirs: e.dirs,
            };
            let nid = b.push(id, name, data);
            if let Some(sub) = e.sub {
                queue.push_back((nid, sub));
            }
        }
        b.set_children(id, first, count);
    }
}

/// Scans `path` and everything below it.
pub fn scan(path: &Path, opts: ScanOptions, progress: &Progress) -> Result<Tree, ScanError> {
    let start = Instant::now();
    let fail = |what: &str, e: &dyn fmt::Display| ScanError::Failed(format!("{what} “{}”: {e}", path.display()));
    let root_path = std::fs::canonicalize(path).map_err(|e| fail("Cannot open", &e))?;
    let fd = rustix::fs::open(&root_path, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| fail("Cannot open", &error_message(e)))?;
    let meta = stat_fd(fd.as_fd()).map_err(|e| fail("Cannot read", &error_message(e)))?;

    let mounts = mounts::read_mounts();
    let mounts_by_id = mounts.iter().enumerate().map(|(i, m)| (m.id, i)).collect();
    let root_mount = meta
        .mnt_id
        .and_then(|id| mounts.iter().find(|m| m.id == id))
        .or_else(|| mounts::mount_for_path(&mounts, &root_path))
        .cloned();
    let ctx = Ctx {
        opts,
        progress,
        root: root_path.clone(),
        mounts,
        mounts_by_id,
        entered: Mutex::new(root_mount.into_iter().collect()),
        hardlinks: (0..HARDLINK_SHARDS).map(|_| Mutex::new(HashSet::new())).collect(),
        errors: Mutex::new(Vec::new()),
        name_bytes: AtomicU64::new(0),
        tick: AtomicU32::new(0),
    };

    let threads = if opts.threads == 0 { default_threads() } else { opts.threads };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .stack_size(32 << 20)
        .thread_name(|i| format!("dendro-scan-{i}"))
        .build()
        .map_err(|e| ScanError::Failed(format!("Cannot start worker threads: {e}")))?;
    progress.set_current(&root_path);
    let (scan, totals, flags) = pool.install(|| scan_dir(&ctx, fd, &root_path, meta.dev, 0));
    if progress.is_cancelled() {
        // Free the partial result on the pool as well; deep trees take a while.
        pool.install(move || drop(scan));
        return Err(ScanError::Cancelled);
    }
    drop(pool);
    let scan_time = start.elapsed();

    let root_name = root_path.as_os_str().as_bytes();
    let nodes = 1 + totals.files as usize + totals.dirs as usize;
    let name_bytes = ctx.name_bytes.load(Relaxed) as usize + root_name.len();
    let mut b = TreeBuilder::with_capacity(nodes, name_bytes);
    let root = b.push(
        NONE,
        root_name,
        NodeData {
            kind: Kind::Dir,
            flags,
            size: totals.size,
            alloc: totals.alloc.saturating_add(meta.alloc),
            mtime: meta.mtime,
            files: totals.files,
            dirs: totals.dirs,
        },
    );
    flatten(&mut b, root, scan);
    if std::env::var_os("DENDRO_DEBUG").is_some() {
        eprintln!("scan {:?}, build {:?}", scan_time, start.elapsed() - scan_time);
    }

    let info = ScanInfo {
        duration: start.elapsed(),
        error_count: progress.errors.load(Relaxed),
        errors: ctx.errors.into_inner().unwrap_or_default(),
        one_filesystem: opts.one_filesystem,
        elevated: rustix::process::geteuid().is_root(),
    };
    let tree = b.finish(root_path, info);
    release_free_memory();
    Ok(tree)
}

/// The temporary per-directory buffers were allocated by many worker
/// threads; give the freed memory back to the system instead of letting it
/// sit in per-thread malloc arenas.
fn release_free_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: malloc_trim only releases unused heap memory.
        unsafe {
            malloc_trim(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{SizeMode, ROOT};
    use std::fs;
    use std::os::unix::fs::symlink;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dendro-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn find(tree: &Tree, parent: NodeId, name: &str) -> NodeId {
        tree.children(parent).find(|&c| tree.name(c) == name.as_bytes()).unwrap_or_else(|| panic!("{name} missing"))
    }

    #[test]
    fn scans_everything() {
        let dir = temp_dir("scan");
        fs::write(dir.join("big.bin"), vec![1u8; 100_000]).unwrap();
        fs::write(dir.join(".hidden"), b"secret").unwrap();
        fs::create_dir_all(dir.join(".config/deep/er")).unwrap();
        fs::write(dir.join(".config/deep/er/x.TXT"), vec![2u8; 5000]).unwrap();
        fs::create_dir(dir.join("empty")).unwrap();
        fs::hard_link(dir.join("big.bin"), dir.join("big-link.bin")).unwrap();
        symlink("big.bin", dir.join("link")).unwrap();
        symlink("/nonexistent", dir.join("dangling")).unwrap();
        fs::write(dir.join("name with spaces & ünïcödé"), b"x").unwrap();

        let progress = Progress::default();
        let tree = scan(&dir, ScanOptions::default(), &progress).unwrap();

        assert_eq!(tree.root_path, fs::canonicalize(&dir).unwrap());
        assert_eq!(tree.file_count(ROOT), 7);
        assert_eq!(tree.dir_count(ROOT), 4);
        assert_eq!(tree.len(), 12);
        let hidden = find(&tree, ROOT, ".hidden");
        assert_eq!(tree.logical_size(hidden), 6);
        let cfg = find(&tree, ROOT, ".config");
        assert!(tree.is_dir(cfg));
        assert_eq!(tree.logical_size(cfg), 5000);
        assert_eq!(tree.kind(find(&tree, ROOT, "link")), Kind::Symlink);
        assert_eq!(tree.kind(find(&tree, ROOT, "dangling")), Kind::Symlink);
        find(&tree, ROOT, "name with spaces & ünïcödé");

        // Hard links are counted once.
        let a = find(&tree, ROOT, "big.bin");
        let b = find(&tree, ROOT, "big-link.bin");
        assert_eq!(tree.has_flag(a, flag::HARDLINK) as u8 + tree.has_flag(b, flag::HARDLINK) as u8, 1);
        let total: u64 = tree.children(ROOT).map(|c| tree.counted_size(c, SizeMode::Logical)).sum();
        assert_eq!(total, tree.logical_size(ROOT));
        assert!(tree.logical_size(ROOT) < 200_000);

        // Children are sorted by allocated size, largest first.
        let sizes: Vec<u64> = tree.children(ROOT).map(|c| tree.alloc_size(c)).collect();
        assert!(sizes.windows(2).all(|w| w[0] >= w[1]));
        assert_eq!(progress.errors.load(Relaxed), 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reports_unreadable_dirs() {
        if rustix::process::geteuid().is_root() {
            return; // root can read everything
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("perm");
        fs::create_dir(dir.join("locked")).unwrap();
        fs::write(dir.join("locked/f"), b"x").unwrap();
        fs::set_permissions(dir.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let progress = Progress::default();
        let tree = scan(&dir, ScanOptions::default(), &progress).unwrap();
        let locked = find(&tree, ROOT, "locked");
        assert!(tree.has_flag(locked, flag::ERROR));
        assert_eq!(tree.info.error_count, 1);
        fs::set_permissions(dir.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn wide_and_deep() {
        let dir = temp_dir("wide");
        for i in 0..50 {
            let d = dir.join(format!("d{i}"));
            fs::create_dir(&d).unwrap();
            for j in 0..40 {
                fs::write(d.join(format!("f{j}.dat")), vec![0u8; j * 10]).unwrap();
            }
        }
        let mut deep = dir.join("deep");
        for i in 0..200 {
            deep.push(format!("level{i}"));
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("bottom"), b"hello").unwrap();
        let tree = scan(&dir, ScanOptions { one_filesystem: true, threads: 8 }, &Progress::default()).unwrap();
        assert_eq!(tree.file_count(ROOT), 50 * 40 + 1);
        assert_eq!(tree.dir_count(ROOT), 50 + 1 + 200);
        let expected: u64 = 50 * (0..40).map(|j| j * 10).sum::<u64>() + 5;
        assert_eq!(tree.logical_size(ROOT), expected);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bind_mounts_are_not_counted_twice() {
        if !rustix::process::geteuid().is_root() {
            return;
        }
        let dir = temp_dir("bind");
        let outside = temp_dir("bind-outside");
        fs::create_dir_all(dir.join("data")).unwrap();
        fs::write(dir.join("data/f"), vec![1u8; 100_000]).unwrap();
        fs::write(outside.join("g"), vec![2u8; 50_000]).unwrap();
        fs::create_dir(dir.join("again")).unwrap();
        fs::create_dir(dir.join("ext")).unwrap();
        let bind = |src: &Path, dst: &Path| {
            std::process::Command::new("mount").arg("--bind").arg(src).arg(dst).status().is_ok_and(|s| s.success())
        };
        if !bind(&dir.join("data"), &dir.join("again")) {
            return; // not allowed to mount here
        }
        assert!(bind(&outside, &dir.join("ext")));
        let opts = ScanOptions { one_filesystem: false, threads: 2 };
        let tree = scan(&dir, opts, &Progress::default());
        let _ = std::process::Command::new("umount").arg(dir.join("again")).status();
        let _ = std::process::Command::new("umount").arg(dir.join("ext")).status();
        let tree = tree.unwrap();
        // data/f once, plus the bind mount of a folder outside the scan.
        assert_eq!(tree.logical_size(ROOT), 150_000);
        assert!(tree.has_flag(find(&tree, ROOT, "again"), flag::MOUNT));
        assert!(!tree.has_flag(find(&tree, ROOT, "ext"), flag::MOUNT));
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn cancel() {
        let p = Progress::default();
        p.cancel();
        assert!(matches!(scan(Path::new("/usr"), ScanOptions::default(), &p), Err(ScanError::Cancelled)));
    }

    #[test]
    fn missing_root() {
        assert!(matches!(
            scan(Path::new("/definitely/not/here"), ScanOptions::default(), &Progress::default()),
            Err(ScanError::Failed(_))
        ));
    }
}
