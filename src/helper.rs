//! Scanning with administrator rights.
//!
//! The GUI never runs as root. Instead it starts a copy of its own binary
//! through `pkexec` in "scan helper" mode. The helper scans (read-only) and
//! streams progress and the finished tree back over its stdout using a small
//! binary format. Closing the pipe makes the helper stop.

use crate::scanner::{self, Progress, ScanError, ScanOptions};
use crate::tree::{raw, ScanInfo, Tree};
use std::ffi::OsString;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

pub const HELPER_ARG: &str = "--scan-helper";

const MAGIC: &[u8; 8] = b"DENDRO01";
const MSG_PROGRESS: u8 = b'P';
const MSG_TREE: u8 = b'T';
const MSG_ERROR: u8 = b'E';

/// Upper bound for any single length field; protects against garbage input.
const MAX_BLOB: u64 = 1 << 40;

fn put_u64(w: &mut impl Write, v: u64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn put_bytes(w: &mut impl Write, b: &[u8]) -> io::Result<()> {
    put_u64(w, b.len() as u64)?;
    w.write_all(b)
}

fn get_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn get_bytes(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let len = get_u64(r)?;
    if len > MAX_BLOB {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "length out of range"));
    }
    let mut v = Vec::new();
    r.take(len).read_to_end(&mut v)?;
    if v.len() as u64 != len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(v)
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

pub fn write_tree(w: &mut impl Write, tree: &Tree) -> io::Result<()> {
    w.write_all(MAGIC)?;
    put_bytes(w, tree.root_path.as_os_str().as_bytes())?;
    let info = &tree.info;
    put_u64(w, info.duration.as_micros() as u64)?;
    put_u64(w, info.error_count)?;
    put_u64(w, u64::from(info.one_filesystem))?;
    put_u64(w, info.errors.len() as u64)?;
    for (path, msg) in &info.errors {
        put_bytes(w, path.as_os_str().as_bytes())?;
        put_bytes(w, msg.as_bytes())?;
    }
    let exts = raw::exts(tree);
    put_u64(w, exts.len() as u64)?;
    for e in exts {
        put_bytes(w, e.as_bytes())?;
    }
    put_bytes(w, raw::names(tree))?;
    put_u64(w, tree.len() as u64)?;
    let mut rec = [0u8; 56];
    for n in raw::nodes(tree) {
        rec[0..8].copy_from_slice(&n.size.to_le_bytes());
        rec[8..16].copy_from_slice(&n.alloc.to_le_bytes());
        rec[16..24].copy_from_slice(&n.mtime.to_le_bytes());
        rec[24..32].copy_from_slice(&n.name.to_le_bytes());
        rec[32..36].copy_from_slice(&n.parent.to_le_bytes());
        rec[36..40].copy_from_slice(&n.first_child.to_le_bytes());
        rec[40..44].copy_from_slice(&n.child_count.to_le_bytes());
        rec[44..48].copy_from_slice(&n.files.to_le_bytes());
        rec[48..52].copy_from_slice(&n.dirs.to_le_bytes());
        rec[52..54].copy_from_slice(&n.ext.to_le_bytes());
        rec[54] = n.kind;
        rec[55] = n.flags;
        w.write_all(&rec)?;
    }
    Ok(())
}

pub fn read_tree(r: &mut impl Read) -> io::Result<Tree> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(bad("unexpected data from the scan helper"));
    }
    let root_path = PathBuf::from(OsString::from_vec(get_bytes(r)?));
    let duration = Duration::from_micros(get_u64(r)?);
    let error_count = get_u64(r)?;
    let one_filesystem = get_u64(r)? != 0;
    let n_errors = get_u64(r)?;
    if n_errors > 100_000 {
        return Err(bad("too many errors"));
    }
    let mut errors = Vec::new();
    for _ in 0..n_errors {
        let p = PathBuf::from(OsString::from_vec(get_bytes(r)?));
        let m = String::from_utf8_lossy(&get_bytes(r)?).into_owned();
        errors.push((p, m));
    }
    let n_exts = get_u64(r)?;
    if n_exts > 1 << 16 {
        return Err(bad("too many extensions"));
    }
    let mut exts = Vec::new();
    for _ in 0..n_exts {
        exts.push(String::from_utf8_lossy(&get_bytes(r)?).into());
    }
    let names = get_bytes(r)?;
    let count = get_u64(r)?;
    if count > u32::MAX as u64 {
        return Err(bad("too many nodes"));
    }
    let mut nodes = Vec::with_capacity(count as usize);
    let mut rec = [0u8; 56];
    let u32_at = |rec: &[u8; 56], i: usize| u32::from_le_bytes(rec[i..i + 4].try_into().unwrap());
    let u64_at = |rec: &[u8; 56], i: usize| u64::from_le_bytes(rec[i..i + 8].try_into().unwrap());
    for _ in 0..count {
        r.read_exact(&mut rec)?;
        nodes.push(raw::RawNode {
            size: u64_at(&rec, 0),
            alloc: u64_at(&rec, 8),
            mtime: u64_at(&rec, 16) as i64,
            name: u64_at(&rec, 24),
            parent: u32_at(&rec, 32),
            first_child: u32_at(&rec, 36),
            child_count: u32_at(&rec, 40),
            files: u32_at(&rec, 44),
            dirs: u32_at(&rec, 48),
            ext: u16::from_le_bytes([rec[52], rec[53]]),
            kind: rec[54],
            flags: rec[55],
        });
    }
    let info = ScanInfo { duration, error_count, errors, one_filesystem, elevated: true };
    raw::assemble(nodes, names, exts, root_path, info).map_err(bad)
}

fn write_progress(w: &mut impl Write, p: &Progress) -> io::Result<()> {
    w.write_all(&[MSG_PROGRESS])?;
    put_u64(w, p.files.load(Relaxed))?;
    put_u64(w, p.dirs.load(Relaxed))?;
    put_u64(w, p.bytes.load(Relaxed))?;
    put_u64(w, p.errors.load(Relaxed))?;
    put_bytes(w, p.current().as_os_str().as_bytes())?;
    w.flush()
}

/// Entry point of `dendro --scan-helper [--one-fs] [--threads N] -- PATH`.
pub fn run(args: &[OsString]) -> i32 {
    let mut opts = ScanOptions { one_filesystem: false, threads: 0 };
    let mut path = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("--one-fs") => opts.one_filesystem = true,
            Some("--threads") => {
                opts.threads = it.next().and_then(|t| t.to_str()?.parse().ok()).unwrap_or(0);
            }
            Some("--") => {
                path = it.next().cloned();
                break;
            }
            _ => path = Some(a.clone()),
        }
    }
    let Some(path) = path else {
        eprintln!("usage: dendro {HELPER_ARG} [--one-fs] -- PATH");
        return 2;
    };
    scanner::raise_fd_limit();

    let progress = Arc::new(Progress::default());
    let (tx, rx) = std::sync::mpsc::channel();
    let p2 = progress.clone();
    let path = PathBuf::from(path);
    std::thread::spawn(move || {
        let _ = tx.send(scanner::scan(&path, opts, &p2));
    });

    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(1 << 20, stdout.lock());
    if out.write_all(MAGIC).and_then(|_| out.flush()).is_err() {
        return 1;
    }
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(tree)) => {
                let ok = out.write_all(&[MSG_TREE]).and_then(|_| write_tree(&mut out, &tree)).and_then(|_| out.flush());
                return if ok.is_ok() { 0 } else { 1 };
            }
            Ok(Err(e)) => {
                let _ = out.write_all(&[MSG_ERROR]).and_then(|_| put_bytes(&mut out, e.to_string().as_bytes()));
                let _ = out.flush();
                return 1;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // The GUI went away (or the user cancelled): stop scanning.
                if write_progress(&mut out, &progress).is_err() {
                    progress.cancel();
                    return 1;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return 1,
        }
    }
}

/// Path of this program for pkexec. After a package upgrade replaced the
/// binary, /proc reports the old one as "… (deleted)"; the new one lives
/// at the same place.
fn program_path() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    if let Some(stripped) = exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        let path = PathBuf::from(stripped);
        if path.is_file() {
            return Ok(path);
        }
    }
    Ok(exe)
}

/// Finds `pkexec` in the usual places.
pub fn pkexec_path() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(["/usr/bin", "/bin", "/usr/local/bin"].map(PathBuf::from));
    dirs.into_iter().map(|d| d.join("pkexec")).find(|p| p.is_file())
}

/// Runs a scan through `pkexec`, mirroring the helper's progress into
/// `progress`. Blocks until the scan finishes, fails or is cancelled.
pub fn scan_elevated(path: &std::path::Path, opts: ScanOptions, progress: &Progress) -> Result<Tree, ScanError> {
    let pkexec = pkexec_path().ok_or_else(|| {
        ScanError::Failed("Scanning as administrator needs “pkexec” (part of polkit), which is not installed.".into())
    })?;
    let exe = program_path().map_err(|e| ScanError::Failed(format!("Cannot locate the program: {e}")))?;
    let mut cmd = Command::new(pkexec);
    cmd.arg(exe).arg(HELPER_ARG);
    if opts.one_filesystem {
        cmd.arg("--one-fs");
    }
    if opts.threads > 0 {
        cmd.arg("--threads").arg(opts.threads.to_string());
    }
    cmd.arg("--").arg(path);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ScanError::Failed(format!("Cannot start pkexec: {e}")))?;

    let mut stderr = child.stderr.take().unwrap();
    let stderr_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });

    let stdout = child.stdout.take().unwrap();
    let mut r = BufReader::with_capacity(1 << 20, stdout);
    let read_all = |r: &mut BufReader<std::process::ChildStdout>| -> io::Result<Option<Result<Tree, String>>> {
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(bad("unexpected output from the scan helper"));
        }
        loop {
            if progress.is_cancelled() {
                return Ok(None);
            }
            let mut tag = [0u8; 1];
            r.read_exact(&mut tag)?;
            match tag[0] {
                MSG_PROGRESS => {
                    progress.files.store(get_u64(r)?, Relaxed);
                    progress.dirs.store(get_u64(r)?, Relaxed);
                    progress.bytes.store(get_u64(r)?, Relaxed);
                    progress.errors.store(get_u64(r)?, Relaxed);
                    let cur = PathBuf::from(OsString::from_vec(get_bytes(r)?));
                    progress.set_current(&cur);
                }
                MSG_TREE => return Ok(Some(Ok(read_tree(r)?))),
                MSG_ERROR => return Ok(Some(Err(String::from_utf8_lossy(&get_bytes(r)?).into_owned()))),
                _ => return Err(bad("unexpected message from the scan helper")),
            }
        }
    };
    // While the password dialog is open nothing arrives on the pipe, but
    // pkexec still runs with our user id and can be stopped. (Once the helper
    // runs as root this fails harmlessly; it then stops by itself when the
    // pipe is closed.) The child is reaped only after the watcher is done,
    // so its pid cannot be reused in between.
    let done = AtomicBool::new(false);
    let pid = rustix::process::Pid::from_raw(child.id() as i32);
    let result = std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Relaxed) {
                if progress.is_cancelled() {
                    if let Some(pid) = pid {
                        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
                    }
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let result = read_all(&mut r);
        done.store(true, Relaxed);
        result
    });
    // Closing our end makes the helper exit on its next progress update.
    drop(r);
    let status = child.wait().ok();
    let stderr = stderr_thread.join().unwrap_or_default();

    match result {
        Ok(Some(Ok(mut tree))) => {
            tree.info.elevated = true;
            Ok(tree)
        }
        Ok(Some(Err(msg))) => Err(ScanError::Failed(msg)),
        Ok(None) => Err(ScanError::Cancelled),
        Err(_) if progress.is_cancelled() => Err(ScanError::Cancelled),
        Err(e) => {
            let code = status.and_then(|s| s.code());
            let detail = stderr.trim();
            Err(ScanError::Failed(match code {
                Some(126) => "Authentication was cancelled.".to_string(),
                Some(127) if detail.contains("authentication agent") || detail.is_empty() => {
                    "Not authorized. Make sure a polkit authentication agent is running.".to_string()
                }
                _ if !detail.is_empty() => detail.to_string(),
                _ => format!("The administrator scan failed: {e}"),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::ROOT;

    #[test]
    fn tree_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dendro-helper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/file.rs"), b"fn main() {}").unwrap();
        std::fs::write(dir.join(".dot"), b"1234").unwrap();
        let tree = scanner::scan(&dir, ScanOptions::default(), &Progress::default()).unwrap();
        let mut buf = Vec::new();
        write_tree(&mut buf, &tree).unwrap();
        let back = read_tree(&mut &buf[..]).unwrap();
        assert_eq!(back.len(), tree.len());
        assert_eq!(back.root_path, tree.root_path);
        for id in 0..tree.len() as u32 {
            assert_eq!(back.path(id), tree.path(id));
            assert_eq!(back.alloc_size(id), tree.alloc_size(id));
            assert_eq!(back.ext_name(back.ext(id)), tree.ext_name(tree.ext(id)));
        }
        assert_eq!(back.file_count(ROOT), 2);
        // Corrupted input is rejected instead of producing a broken tree.
        let mut broken = buf.clone();
        let n = broken.len();
        broken[n - 24..n - 20].copy_from_slice(&0xffff_fff0u32.to_le_bytes());
        assert!(read_tree(&mut &broken[..]).is_err());
        assert!(read_tree(&mut &buf[..buf.len() - 3]).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
