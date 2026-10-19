//! Permanent deletion that never leaves the filesystem it started on.
//!
//! `std::fs::remove_dir_all` happily descends into whatever is mounted
//! inside a folder (a USB drive, a network share, a bind mount) and empties
//! it. Here every directory is checked before descending, and the whole
//! operation stops at the first mount point it meets.

use crate::scanner::{open_dir, stat_at};
use crate::tree::Kind;
use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{AtFlags, Mode, OFlags, RawDir};
use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const MAX_DEPTH: u32 = 4096;

fn other_fs(path: &Path) -> io::Error {
    io::Error::other(format!("{} is a mount point of another filesystem; nothing below it was touched", path.display()))
}

/// Deletes a file, symlink or folder (recursively). Symlinks are removed,
/// never followed.
pub fn remove_all(path: &Path) -> io::Result<()> {
    let name = path.file_name().ok_or_else(|| io::Error::other("cannot delete this path"))?;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let cname = CString::new(name.as_bytes())?;
    let pfd = rustix::fs::open(parent, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
    let parent_dev = stat_at(pfd.as_fd(), c".")?.dev;
    let meta = stat_at(pfd.as_fd(), &cname)?;
    if meta.kind != Kind::Dir {
        rustix::fs::unlinkat(pfd.as_fd(), &cname, AtFlags::empty())?;
        return Ok(());
    }
    if meta.is_mount_boundary(parent_dev) {
        return Err(other_fs(path));
    }
    let fd = open_dir(pfd.as_fd(), &cname)?;
    remove_contents(&fd, meta.dev, path, 0)?;
    drop(fd);
    rustix::fs::unlinkat(pfd.as_fd(), &cname, AtFlags::REMOVEDIR)?;
    Ok(())
}

fn list(dir: &OwnedFd) -> io::Result<Vec<CString>> {
    let mut buf = vec![MaybeUninit::<u8>::uninit(); 32 * 1024];
    let mut iter = RawDir::new(dir.as_fd(), &mut buf);
    let mut names = Vec::new();
    while let Some(entry) = iter.next() {
        let entry = entry?;
        let name: &CStr = entry.file_name();
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

fn remove_contents(dir: &OwnedFd, dev: u64, path: &Path, depth: u32) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(io::Error::other("folders are nested too deeply"));
    }
    for name in list(dir)? {
        let meta = stat_at(dir.as_fd(), &name)?;
        if meta.kind == Kind::Dir {
            let child = path.join(OsStr::from_bytes(name.to_bytes()));
            if meta.is_mount_boundary(dev) {
                return Err(other_fs(&child));
            }
            let sub = open_dir(dir.as_fd(), &name)?;
            remove_contents(&sub, dev, &child, depth + 1)?;
            drop(sub);
            rustix::fs::unlinkat(dir.as_fd(), &name, AtFlags::REMOVEDIR)?;
        } else {
            rustix::fs::unlinkat(dir.as_fd(), &name, AtFlags::empty())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dendro-rm-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn removes_trees_without_following_symlinks() {
        let base = temp("tree");
        let outside = base.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        let victim = base.join("victim");
        fs::create_dir_all(victim.join("a/b/.hidden")).unwrap();
        fs::write(victim.join("a/b/.hidden/f"), b"x").unwrap();
        fs::write(victim.join("top.bin"), b"y").unwrap();
        symlink(&outside, victim.join("a/link-to-outside")).unwrap();
        remove_all(&victim).unwrap();
        assert!(!victim.exists());
        assert!(outside.join("keep.txt").exists());

        fs::write(base.join("single"), b"z").unwrap();
        remove_all(&base.join("single")).unwrap();
        assert!(!base.join("single").exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn stops_at_mount_points() {
        if !rustix::process::geteuid().is_root() {
            return;
        }
        let base = temp("mount");
        let victim = base.join("victim");
        let mnt = victim.join("mnt");
        fs::create_dir_all(&mnt).unwrap();
        fs::write(victim.join("a"), b"a").unwrap();
        let mounted = std::process::Command::new("mount")
            .args(["-t", "tmpfs", "dendro-test", mnt.to_str().unwrap()])
            .status()
            .is_ok_and(|s| s.success());
        if !mounted {
            return; // no permission to mount in this environment
        }
        fs::write(mnt.join("precious"), b"data").unwrap();
        let err = remove_all(&victim).unwrap_err();
        assert!(err.to_string().contains("mount point"));
        let survived = mnt.join("precious").exists();
        let _ = std::process::Command::new("umount").arg(&mnt).status();
        assert!(survived, "files on the mounted filesystem were deleted");
        let _ = fs::remove_dir_all(&base);
    }
}
