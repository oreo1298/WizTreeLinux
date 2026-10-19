//! Mounted filesystems, as seen in `/proc/self/mountinfo`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Mount {
    pub id: u64,
    /// `major:minor` of the device.
    pub dev: String,
    /// Root of the mount within its filesystem (e.g. `/@home` for btrfs subvolumes).
    pub root: String,
    pub mount_point: PathBuf,
    pub fstype: String,
    pub source: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FsUsage {
    pub total: u64,
    pub used: u64,
    pub available: u64,
}

/// Filesystems that only expose kernel state and must never be scanned when
/// crossing filesystem boundaries.
const VIRTUAL_FS: &[&str] = &[
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "securityfs",
    "cgroup",
    "cgroup2",
    "pstore",
    "efivarfs",
    "bpf",
    "debugfs",
    "tracefs",
    "configfs",
    "fusectl",
    "mqueue",
    "hugetlbfs",
    "autofs",
    "binfmt_misc",
    "rpc_pipefs",
    "nsfs",
    "selinuxfs",
    "nfsd",
    "devfs",
    "usbfs",
    "sockfs",
    "pipefs",
    "anon_inodefs",
    "fuse.portal",
    "fuse.gvfsd-fuse",
    "fuse.lxcfs",
    "fuse.xdg-document-portal",
    "fuse.snapfuse",
    "binder",
    "rootfs",
];

/// Network filesystems; they are shown with a different icon.
const NETWORK_FS: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "fuse.sshfs",
    "sshfs",
    "9p",
    "ceph",
    "glusterfs",
    "fuse.glusterfs",
    "davfs",
    "fuse.davfs2",
    "fuse.rclone",
    "afs",
    "ncpfs",
    "fuse.s3fs",
];

pub fn is_virtual_fs(fstype: &str) -> bool {
    VIRTUAL_FS.contains(&fstype)
}

pub fn is_network_fs(fstype: &str) -> bool {
    NETWORK_FS.contains(&fstype)
}

/// Decodes the octal escapes (`\040` etc.) used in mountinfo.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            let v = (b[i + 1] - b'0') as u32 * 64 + (b[i + 2] - b'0') as u32 * 8 + (b[i + 3] - b'0') as u32;
            out.push(v as u8);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_mountinfo(text: &str) -> Vec<Mount> {
    let mut mounts = Vec::new();
    for line in text.lines() {
        let Some((pre, post)) = line.split_once(" - ") else { continue };
        let pre: Vec<&str> = pre.split(' ').collect();
        let post: Vec<&str> = post.split(' ').collect();
        if pre.len() < 5 || post.len() < 2 {
            continue;
        }
        let Ok(id) = pre[0].parse() else { continue };
        mounts.push(Mount {
            id,
            dev: pre[2].to_string(),
            root: unescape(pre[3]),
            mount_point: PathBuf::from(unescape(pre[4])),
            fstype: post[0].to_string(),
            source: unescape(post[1]),
        });
    }
    mounts
}

pub fn read_mounts() -> Vec<Mount> {
    std::fs::read_to_string("/proc/self/mountinfo").map(|t| parse_mountinfo(&t)).unwrap_or_default()
}

/// A filesystem mounted at `path` or anywhere below it.
pub fn mount_at_or_below<'a>(mounts: &'a [Mount], path: &Path) -> Option<&'a Mount> {
    mounts.iter().find(|m| m.mount_point.starts_with(path))
}

/// The mount that contains `path` (longest matching mount point).
pub fn mount_for_path<'a>(mounts: &'a [Mount], path: &Path) -> Option<&'a Mount> {
    mounts.iter().filter(|m| path.starts_with(&m.mount_point)).max_by_key(|m| m.mount_point.as_os_str().len())
}

/// Mounts worth offering on the start page: real storage, no kernel or
/// container internals, one entry per distinct mounted location.
pub fn listable_mounts(mounts: &[Mount]) -> Vec<Mount> {
    const HIDDEN_PREFIXES: &[&str] = &[
        "/proc",
        "/sys",
        "/dev",
        "/run/user",
        "/run/credentials",
        "/run/snapd",
        "/snap",
        "/var/lib/docker",
        "/var/lib/containers",
        "/var/lib/snapd",
        "/var/snap",
        "/run/docker",
        "/run/containerd",
        "/run/netns",
        "/var/lib/flatpak",
        "/var/lib/kubelet",
        "/tmp/.mount_",
    ];
    let mut seen = HashSet::new();
    let mut out: Vec<Mount> = Vec::new();
    for m in mounts {
        let fs = m.fstype.as_str();
        let mp = m.mount_point.to_string_lossy();
        if is_virtual_fs(fs) || matches!(fs, "tmpfs" | "ramfs" | "squashfs" | "erofs" | "zram") {
            continue;
        }
        if fs == "overlay" && mp != "/" {
            continue;
        }
        if HIDDEN_PREFIXES
            .iter()
            .any(|p| mp == *p || mp.starts_with(&format!("{p}/")) || (p.ends_with('_') && mp.starts_with(p)))
        {
            continue;
        }
        // Bind mounts of the same directory show up several times.
        if !seen.insert((m.dev.clone(), m.root.clone())) {
            continue;
        }
        // A later mount on the same mount point hides the earlier one.
        out.retain(|o| o.mount_point != m.mount_point);
        out.push(m.clone());
    }
    out.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    out
}

/// Capacity of the filesystem containing `path`. May block on dead network
/// mounts, so call it off the UI thread.
pub fn fs_usage(path: &Path) -> Option<FsUsage> {
    let s = rustix::fs::statvfs(path).ok()?;
    let frsize = if s.f_frsize > 0 { s.f_frsize } else { s.f_bsize };
    let total = s.f_blocks.saturating_mul(frsize);
    let free = s.f_bfree.saturating_mul(frsize);
    let available = s.f_bavail.saturating_mul(frsize);
    Some(FsUsage { total, used: total.saturating_sub(free), available })
}

/// A short, friendly name for a mount on the start page.
pub fn display_name(m: &Mount) -> String {
    let mp = m.mount_point.to_string_lossy();
    if mp == "/" {
        return "System".to_string();
    }
    for prefix in ["/run/media/", "/media/"] {
        if let Some(rest) = mp.strip_prefix(prefix) {
            // /run/media/<user>/<label> or /media/<user>/<label>
            if let Some(label) = rest.rsplit('/').next() {
                if !label.is_empty() {
                    return label.to_string();
                }
            }
        }
    }
    mp.into_owned()
}

pub fn icon_name(m: &Mount) -> &'static str {
    let mp = m.mount_point.to_string_lossy();
    if is_network_fs(&m.fstype) {
        "folder-remote-symbolic"
    } else if mp.starts_with("/run/media/") || mp.starts_with("/media/") || mp.starts_with("/mnt/") {
        "drive-removable-media-symbolic"
    } else {
        "drive-harddisk-symbolic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
22 1 0:21 / /proc rw,nosuid shared:5 - proc proc rw
25 1 259:2 /@ / rw,noatime shared:1 - btrfs /dev/nvme0n1p2 rw,compress=zstd:3
26 25 259:2 /@home /home rw,noatime shared:2 - btrfs /dev/nvme0n1p2 rw
27 25 259:1 / /boot rw,relatime shared:3 - vfat /dev/nvme0n1p1 rw
28 25 0:30 / /tmp rw shared:4 - tmpfs tmpfs rw
29 25 8:17 / /run/media/me/My\\040Disk rw - exfat /dev/sdb1 rw
30 25 0:55 / /var/lib/docker/overlay2/x/merged rw - overlay overlay rw
31 26 259:2 /@home /home rw,noatime - btrfs /dev/nvme0n1p2 rw
";

    #[test]
    fn parses_and_filters() {
        let mounts = parse_mountinfo(SAMPLE);
        assert_eq!(mounts.len(), 8);
        assert_eq!(mounts[5].mount_point, PathBuf::from("/run/media/me/My Disk"));
        let list = listable_mounts(&mounts);
        let points: Vec<_> = list.iter().map(|m| m.mount_point.to_string_lossy().into_owned()).collect();
        assert_eq!(points, vec!["/", "/boot", "/home", "/run/media/me/My Disk"]);
        assert_eq!(display_name(&list[3]), "My Disk");
        let m = mount_for_path(&mounts, Path::new("/home/me/x")).unwrap();
        assert_eq!(m.fstype, "btrfs");
        assert_eq!(m.root, "/@home");
    }
}
