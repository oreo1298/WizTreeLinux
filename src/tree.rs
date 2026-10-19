//! Compact in-memory representation of a scanned directory tree.
//!
//! All nodes live in one flat arena (`Vec<Node>`, 56 bytes per entry) and all
//! names live in one byte buffer, so a scan of millions of files needs only a
//! handful of allocations. Children of a directory are stored contiguously,
//! which makes iterating them cache friendly.
//!
//! The shape of the tree never changes after a scan. Deleting an item from
//! the UI only flags it (and its descendants) as deleted and subtracts its
//! sizes from its ancestors; the mutable fields are atomics so the tree can be
//! shared between the UI thread and background workers without locking.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::hash::{BuildHasherDefault, Hasher};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::time::Duration;

pub type NodeId = u32;
pub type ExtId = u16;

/// Sentinel for "no node".
pub const NONE: NodeId = u32::MAX;
/// The scanned folder itself is always node 0.
pub const ROOT: NodeId = 0;
/// Extension id used for files without an extension.
pub const NO_EXT: ExtId = 0;

/// Upper bound on the number of distinct extensions that get their own id.
const MAX_EXTS: usize = u16::MAX as usize;
/// Extensions longer than this are not considered real extensions.
const MAX_EXT_LEN: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Kind {
    File = 0,
    Dir = 1,
    Symlink = 2,
    /// Sockets, FIFOs, device nodes.
    Other = 3,
}

impl Kind {
    pub fn from_u8(v: u8) -> Kind {
        match v {
            1 => Kind::Dir,
            2 => Kind::Symlink,
            3 => Kind::Other,
            _ => Kind::File,
        }
    }
}

/// Per-node flag bits.
pub mod flag {
    /// The directory (or parts of it) could not be read.
    pub const ERROR: u8 = 1;
    /// A mount point that was not descended into.
    pub const MOUNT: u8 = 2;
    /// An additional hard link to an inode that was already counted.
    pub const HARDLINK: u8 = 4;
    /// Removed by the user after the scan.
    pub const DELETED: u8 = 8;
    /// Mount point of a virtual filesystem (procfs, sysfs, ...).
    pub const VIRTUAL: u8 = 16;
}

/// Which size is used for sorting, percentages and the treemap.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SizeMode {
    /// Space actually used on disk (`st_blocks * 512`).
    #[default]
    Allocated,
    /// Apparent file size (`st_size`).
    Logical,
}

#[repr(C)]
pub struct Node {
    /// Logical size; for directories the sum over the subtree.
    size: AtomicU64,
    /// Allocated size; for directories the sum over the subtree.
    alloc: AtomicU64,
    mtime: i64,
    /// `(offset << 16) | len` into `Tree::names`.
    name: u64,
    parent: NodeId,
    first_child: NodeId,
    child_count: u32,
    /// Number of non-directory entries in the subtree.
    files: AtomicU32,
    /// Number of directories in the subtree (excluding the node itself).
    dirs: AtomicU32,
    ext: ExtId,
    kind: Kind,
    flags: AtomicU8,
}

const _: () = assert!(std::mem::size_of::<Node>() == 56);

/// Plain values of a node, used to build trees.
#[derive(Clone, Copy, Debug)]
pub struct NodeData {
    pub kind: Kind,
    pub flags: u8,
    pub size: u64,
    pub alloc: u64,
    pub mtime: i64,
    pub files: u32,
    pub dirs: u32,
}

/// Information about how a tree was produced.
#[derive(Clone, Debug, Default)]
pub struct ScanInfo {
    pub duration: Duration,
    /// Total number of errors (unreadable folders, failed `stat` calls).
    pub error_count: u64,
    /// The first few errors, for display.
    pub errors: Vec<(PathBuf, String)>,
    pub one_filesystem: bool,
    pub elevated: bool,
}

/// Aggregated statistics for one file extension.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExtStat {
    pub ext: ExtId,
    pub size: u64,
    pub alloc: u64,
    pub count: u64,
}

impl ExtStat {
    pub fn get(&self, mode: SizeMode) -> u64 {
        match mode {
            SizeMode::Allocated => self.alloc,
            SizeMode::Logical => self.size,
        }
    }
}

pub struct Tree {
    nodes: Vec<Node>,
    names: Vec<u8>,
    exts: Vec<Box<str>>,
    pub root_path: PathBuf,
    pub info: ScanInfo,
}

impl Tree {
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[inline]
    fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    /// Raw name bytes. The root's name is its full path.
    pub fn name(&self, id: NodeId) -> &[u8] {
        let packed = self.node(id).name;
        let off = (packed >> 16) as usize;
        let len = (packed & 0xffff) as usize;
        &self.names[off..off + len]
    }

    pub fn name_os(&self, id: NodeId) -> &OsStr {
        OsStr::from_bytes(self.name(id))
    }

    pub fn display_name(&self, id: NodeId) -> String {
        String::from_utf8_lossy(self.name(id)).into_owned()
    }

    pub fn path(&self, id: NodeId) -> PathBuf {
        let mut parts = Vec::new();
        let mut cur = id;
        while cur != ROOT {
            parts.push(cur);
            cur = self.node(cur).parent;
        }
        let mut path = self.root_path.clone();
        for &p in parts.iter().rev() {
            path.push(self.name_os(p));
        }
        path
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        let p = self.node(id).parent;
        (p != NONE).then_some(p)
    }

    /// All children, including deleted ones.
    pub fn children(&self, id: NodeId) -> std::ops::Range<NodeId> {
        let n = self.node(id);
        n.first_child..n.first_child + n.child_count
    }

    /// Children that have not been deleted.
    pub fn live_children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.children(id).filter(move |&c| !self.is_deleted(c))
    }

    pub fn has_live_children(&self, id: NodeId) -> bool {
        self.live_children(id).next().is_some()
    }

    pub fn kind(&self, id: NodeId) -> Kind {
        self.node(id).kind
    }

    pub fn is_dir(&self, id: NodeId) -> bool {
        self.node(id).kind == Kind::Dir
    }

    pub fn flags(&self, id: NodeId) -> u8 {
        self.node(id).flags.load(Relaxed)
    }

    pub fn has_flag(&self, id: NodeId, f: u8) -> bool {
        self.flags(id) & f != 0
    }

    pub fn is_deleted(&self, id: NodeId) -> bool {
        self.has_flag(id, flag::DELETED)
    }

    pub fn logical_size(&self, id: NodeId) -> u64 {
        self.node(id).size.load(Relaxed)
    }

    pub fn alloc_size(&self, id: NodeId) -> u64 {
        self.node(id).alloc.load(Relaxed)
    }

    pub fn size(&self, id: NodeId, mode: SizeMode) -> u64 {
        match mode {
            SizeMode::Allocated => self.alloc_size(id),
            SizeMode::Logical => self.logical_size(id),
        }
    }

    /// Size this node contributes to its parent (zero for extra hard links).
    pub fn counted_size(&self, id: NodeId, mode: SizeMode) -> u64 {
        if self.has_flag(id, flag::HARDLINK | flag::DELETED) {
            0
        } else {
            self.size(id, mode)
        }
    }

    pub fn mtime(&self, id: NodeId) -> i64 {
        self.node(id).mtime
    }

    pub fn file_count(&self, id: NodeId) -> u64 {
        self.node(id).files.load(Relaxed) as u64
    }

    pub fn dir_count(&self, id: NodeId) -> u64 {
        self.node(id).dirs.load(Relaxed) as u64
    }

    pub fn ext(&self, id: NodeId) -> ExtId {
        self.node(id).ext
    }

    /// Lower-case extension without the dot, "" for none.
    pub fn ext_name(&self, ext: ExtId) -> &str {
        self.exts.get(ext as usize).map(|s| &**s).unwrap_or("")
    }

    pub fn ext_count(&self) -> usize {
        self.exts.len()
    }

    /// `id` and all its ancestors, starting with the root.
    pub fn chain(&self, id: NodeId) -> Vec<NodeId> {
        let mut v = vec![id];
        let mut cur = id;
        while let Some(p) = self.parent(cur) {
            v.push(p);
            cur = p;
        }
        v.reverse();
        v
    }

    /// Removes a node from the totals after it was deleted on disk.
    ///
    /// Returns the ancestors whose values changed, or `None` if the node
    /// cannot be deleted (the root, or already deleted).
    pub fn mark_deleted(&self, id: NodeId) -> Option<Vec<NodeId>> {
        if id == ROOT || self.is_deleted(id) {
            return None;
        }
        let n = self.node(id);
        let counted = !self.has_flag(id, flag::HARDLINK);
        let size = if counted { n.size.load(Relaxed) } else { 0 };
        let alloc = if counted { n.alloc.load(Relaxed) } else { 0 };
        let is_dir = n.kind == Kind::Dir;
        let files = n.files.load(Relaxed) + u32::from(!is_dir);
        let dirs = n.dirs.load(Relaxed) + u32::from(is_dir);

        // Flag the whole subtree so searches and statistics skip it.
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            self.node(cur).flags.fetch_or(flag::DELETED, Relaxed);
            stack.extend(self.children(cur));
        }

        let mut changed = Vec::new();
        let mut cur = n.parent;
        while cur != NONE {
            let a = self.node(cur);
            a.size.fetch_sub(size.min(a.size.load(Relaxed)), Relaxed);
            a.alloc.fetch_sub(alloc.min(a.alloc.load(Relaxed)), Relaxed);
            a.files.fetch_sub(files.min(a.files.load(Relaxed)), Relaxed);
            a.dirs.fetch_sub(dirs.min(a.dirs.load(Relaxed)), Relaxed);
            changed.push(cur);
            cur = a.parent;
        }
        Some(changed)
    }

    /// Size statistics per extension, largest first.
    pub fn ext_stats(&self, mode: SizeMode) -> Vec<ExtStat> {
        let mut stats: Vec<ExtStat> =
            (0..self.exts.len()).map(|i| ExtStat { ext: i as ExtId, ..Default::default() }).collect();
        for n in &self.nodes {
            if n.kind == Kind::Dir {
                continue;
            }
            let f = n.flags.load(Relaxed);
            if f & flag::DELETED != 0 {
                continue;
            }
            let s = &mut stats[n.ext as usize];
            s.count += 1;
            if f & flag::HARDLINK == 0 {
                s.size += n.size.load(Relaxed);
                s.alloc += n.alloc.load(Relaxed);
            }
        }
        stats.retain(|s| s.count > 0);
        stats.sort_unstable_by(|a, b| b.get(mode).cmp(&a.get(mode)).then(b.count.cmp(&a.count)));
        stats
    }

    /// The largest non-directory entries matching `query`.
    ///
    /// Returns the matches (largest first, at most `limit`) and the total
    /// number of matching files.
    pub fn top_files(&self, query: &Query, mode: SizeMode, limit: usize) -> (Vec<NodeId>, u64) {
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;

        let mut heap: BinaryHeap<Reverse<(u64, NodeId)>> = BinaryHeap::with_capacity(limit + 1);
        let mut total = 0u64;
        let mut lower_buf = String::new();
        for (i, n) in self.nodes.iter().enumerate() {
            if n.kind == Kind::Dir || n.flags.load(Relaxed) & flag::DELETED != 0 {
                continue;
            }
            let id = i as NodeId;
            if !query.matches(self, id, &mut lower_buf) {
                continue;
            }
            total += 1;
            let size = match mode {
                SizeMode::Allocated => n.alloc.load(Relaxed),
                SizeMode::Logical => n.size.load(Relaxed),
            };
            if heap.len() < limit {
                heap.push(Reverse((size, id)));
            } else if let Some(Reverse((min, _))) = heap.peek() {
                if size > *min {
                    heap.pop();
                    heap.push(Reverse((size, id)));
                }
            }
        }
        let mut v: Vec<(u64, NodeId)> = heap.into_iter().map(|Reverse(x)| x).collect();
        v.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        (v.into_iter().map(|(_, id)| id).collect(), total)
    }
}

/// A file-name filter for [`Tree::top_files`].
#[derive(Clone, Debug, Default)]
pub struct Query {
    /// Lower-case substring to look for in the name.
    text: String,
    /// Match files with exactly this extension (lower-case, no dot).
    ext: Option<String>,
}

impl Query {
    /// Parses user input: `*.mkv` or `.mkv` selects an extension, anything
    /// else is a case-insensitive substring of the file name.
    pub fn parse(input: &str) -> Query {
        let input = input.trim();
        let ext_part = input.strip_prefix("*.").or_else(|| input.strip_prefix('.'));
        if let Some(ext) = ext_part {
            if !ext.is_empty() && !ext.contains(['.', ' ', '*', '/']) {
                return Query { text: String::new(), ext: Some(ext.to_lowercase()) };
            }
        }
        Query { text: input.to_lowercase(), ext: None }
    }

    pub fn for_ext(ext: &str) -> Query {
        Query { text: String::new(), ext: Some(ext.to_lowercase()) }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.ext.is_none()
    }

    fn matches(&self, tree: &Tree, id: NodeId, buf: &mut String) -> bool {
        if let Some(ext) = &self.ext {
            if tree.ext_name(tree.ext(id)) != ext {
                return false;
            }
        }
        if self.text.is_empty() {
            return true;
        }
        let name = tree.name(id);
        if name.is_ascii() && self.text.is_ascii() {
            let needle = self.text.as_bytes();
            name.len() >= needle.len() && name.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
        } else {
            buf.clear();
            buf.push_str(&String::from_utf8_lossy(name).to_lowercase());
            buf.contains(&self.text)
        }
    }
}

/// Returns the extension of a file name (without the dot), if it has a
/// plausible one. Dot files like `.bashrc` have no extension.
pub fn extension_of(name: &[u8]) -> Option<&[u8]> {
    let dot = name.iter().rposition(|&c| c == b'.')?;
    if dot == 0 {
        return None;
    }
    let ext = &name[dot + 1..];
    if ext.is_empty() || ext.len() > MAX_EXT_LEN || ext.iter().any(|&c| c <= b' ') {
        return None;
    }
    Some(ext)
}

/// Small, fast hasher for the short extension strings.
#[derive(Default)]
struct FxHasher(u64);

impl Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
        }
    }

    fn write_usize(&mut self, n: usize) {
        self.0 = (self.0.rotate_left(5) ^ n as u64).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Incrementally builds a [`Tree`].
pub struct TreeBuilder {
    nodes: Vec<Node>,
    names: Vec<u8>,
    exts: Vec<Box<str>>,
    ext_ids: HashMap<Box<[u8]>, ExtId, BuildHasherDefault<FxHasher>>,
}

impl TreeBuilder {
    pub fn with_capacity(nodes: usize, name_bytes: usize) -> Self {
        TreeBuilder {
            nodes: Vec::with_capacity(nodes),
            names: Vec::with_capacity(name_bytes),
            exts: vec!["".into()],
            ext_ids: HashMap::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    fn intern_ext(&mut self, name: &[u8], kind: Kind) -> ExtId {
        if kind == Kind::Dir {
            return NO_EXT;
        }
        let Some(ext) = extension_of(name) else {
            return NO_EXT;
        };
        let mut lower = [0u8; MAX_EXT_LEN];
        let lower = &mut lower[..ext.len()];
        lower.copy_from_slice(ext);
        lower.make_ascii_lowercase();
        if let Some(&id) = self.ext_ids.get(&*lower) {
            return id;
        }
        if self.exts.len() >= MAX_EXTS {
            return NO_EXT;
        }
        let id = self.exts.len() as ExtId;
        self.exts.push(String::from_utf8_lossy(lower).into());
        self.ext_ids.insert(lower.into(), id);
        id
    }

    /// Appends a node; children must be registered with [`set_children`].
    ///
    /// [`set_children`]: TreeBuilder::set_children
    pub fn push(&mut self, parent: NodeId, name: &[u8], d: NodeData) -> NodeId {
        let id = self.nodes.len() as NodeId;
        let off = self.names.len() as u64;
        let len = name.len().min(0xffff);
        self.names.extend_from_slice(&name[..len]);
        let ext = self.intern_ext(name, d.kind);
        self.nodes.push(Node {
            size: AtomicU64::new(d.size),
            alloc: AtomicU64::new(d.alloc),
            mtime: d.mtime,
            name: (off << 16) | len as u64,
            parent,
            first_child: 0,
            child_count: 0,
            files: AtomicU32::new(d.files),
            dirs: AtomicU32::new(d.dirs),
            ext,
            kind: d.kind,
            flags: AtomicU8::new(d.flags),
        });
        id
    }

    pub fn set_children(&mut self, id: NodeId, first: NodeId, count: u32) {
        let n = &mut self.nodes[id as usize];
        n.first_child = first;
        n.child_count = count;
    }

    pub fn finish(self, root_path: PathBuf, info: ScanInfo) -> Tree {
        Tree { nodes: self.nodes, names: self.names, exts: self.exts, root_path, info }
    }
}

/// Raw access used by the privileged-helper wire format.
pub(crate) mod raw {
    use super::*;

    pub struct RawNode {
        pub size: u64,
        pub alloc: u64,
        pub mtime: i64,
        pub name: u64,
        pub parent: u32,
        pub first_child: u32,
        pub child_count: u32,
        pub files: u32,
        pub dirs: u32,
        pub ext: u16,
        pub kind: u8,
        pub flags: u8,
    }

    pub fn nodes(tree: &Tree) -> impl Iterator<Item = RawNode> + '_ {
        tree.nodes.iter().map(|n| RawNode {
            size: n.size.load(Relaxed),
            alloc: n.alloc.load(Relaxed),
            mtime: n.mtime,
            name: n.name,
            parent: n.parent,
            first_child: n.first_child,
            child_count: n.child_count,
            files: n.files.load(Relaxed),
            dirs: n.dirs.load(Relaxed),
            ext: n.ext,
            kind: n.kind as u8,
            flags: n.flags.load(Relaxed),
        })
    }

    pub fn names(tree: &Tree) -> &[u8] {
        &tree.names
    }

    pub fn exts(tree: &Tree) -> &[Box<str>] {
        &tree.exts
    }

    /// Reassembles a tree, validating every index so that malformed input
    /// cannot cause out-of-bounds access later.
    pub fn assemble(
        raw: Vec<RawNode>,
        names: Vec<u8>,
        exts: Vec<Box<str>>,
        root_path: PathBuf,
        info: ScanInfo,
    ) -> Result<Tree, &'static str> {
        let count = raw.len() as u64;
        if count == 0 {
            return Err("empty tree");
        }
        let mut nodes = Vec::with_capacity(raw.len());
        for (i, r) in raw.into_iter().enumerate() {
            let off = r.name >> 16;
            let len = r.name & 0xffff;
            if off + len > names.len() as u64 {
                return Err("name out of range");
            }
            // Parents always precede their children, which also rules out cycles.
            let parent_ok = if i == 0 { r.parent == NONE } else { (r.parent as usize) < i };
            let children_ok = r.child_count == 0 || r.first_child as usize > i;
            if !parent_ok
                || !children_ok
                || r.first_child as u64 + r.child_count as u64 > count
                || r.ext as usize >= exts.len().max(1)
            {
                return Err("index out of range");
            }
            nodes.push(Node {
                size: AtomicU64::new(r.size),
                alloc: AtomicU64::new(r.alloc),
                mtime: r.mtime,
                name: r.name,
                parent: r.parent,
                first_child: r.first_child,
                child_count: r.child_count,
                files: AtomicU32::new(r.files),
                dirs: AtomicU32::new(r.dirs),
                ext: r.ext,
                kind: Kind::from_u8(r.kind),
                flags: AtomicU8::new(r.flags),
            });
        }
        let exts = if exts.is_empty() { vec!["".into()] } else { exts };
        Ok(Tree { nodes, names, exts, root_path, info })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(kind: Kind, size: u64) -> NodeData {
        NodeData { kind, flags: 0, size, alloc: size, mtime: 0, files: 0, dirs: 0 }
    }

    fn sample() -> Tree {
        // root/{a.txt (10), sub/{b.TXT (5), c (1)}}
        let mut b = TreeBuilder::with_capacity(5, 64);
        let root = b.push(NONE, b"/r", NodeData { files: 3, dirs: 1, ..data(Kind::Dir, 16) });
        let a = b.push(root, b"a.txt", data(Kind::File, 10));
        let sub = b.push(root, b"sub", NodeData { files: 2, ..data(Kind::Dir, 6) });
        b.set_children(root, a, 2);
        let bb = b.push(sub, b"b.TXT", data(Kind::File, 5));
        b.push(sub, b".c", data(Kind::File, 1));
        b.set_children(sub, bb, 2);
        b.finish(PathBuf::from("/r"), ScanInfo::default())
    }

    #[test]
    fn extensions() {
        assert_eq!(extension_of(b"a.txt"), Some(&b"txt"[..]));
        assert_eq!(extension_of(b"a.tar.gz"), Some(&b"gz"[..]));
        assert_eq!(extension_of(b".bashrc"), None);
        assert_eq!(extension_of(b"noext"), None);
        assert_eq!(extension_of(b"trailing."), None);
        let t = sample();
        assert_eq!(t.ext(1), t.ext(3), "extensions are case-insensitive");
        assert_eq!(t.ext_name(t.ext(1)), "txt");
        assert_eq!(t.ext(4), NO_EXT);
    }

    #[test]
    fn paths_and_deletion() {
        let t = sample();
        assert_eq!(t.path(3), PathBuf::from("/r/sub/b.TXT"));
        assert_eq!(t.chain(3), vec![0, 2, 3]);
        let changed = t.mark_deleted(2).unwrap();
        assert_eq!(changed, vec![0]);
        assert_eq!(t.alloc_size(ROOT), 10);
        assert_eq!(t.file_count(ROOT), 1);
        assert_eq!(t.dir_count(ROOT), 0);
        assert!(t.is_deleted(3) && t.is_deleted(4));
        assert!(t.mark_deleted(2).is_none());
        assert!(t.mark_deleted(ROOT).is_none());
        assert_eq!(t.live_children(ROOT).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn queries() {
        let t = sample();
        let (ids, total) = t.top_files(&Query::parse(""), SizeMode::Allocated, 2);
        assert_eq!((ids, total), (vec![1, 3], 3));
        let (ids, _) = t.top_files(&Query::parse("*.TXT"), SizeMode::Allocated, 10);
        assert_eq!(ids, vec![1, 3]);
        let (ids, _) = t.top_files(&Query::parse("B.t"), SizeMode::Allocated, 10);
        assert_eq!(ids, vec![3]);
        let stats = t.ext_stats(SizeMode::Allocated);
        assert_eq!(stats[0].alloc, 15);
        assert_eq!(stats[0].count, 2);
    }
}
