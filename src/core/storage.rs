//! Detects whether the storage backing a path is rotational (HDD) or flash
//! (SSD/NVMe). Copy strategies branch on this: parallel small-file writes pay
//! off on flash but thrash HDD heads, so rotational (and undetectable) media
//! keep the sequential path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use tracing::info;

/// `Some(true)` = rotational (HDD), `Some(false)` = flash (SSD/NVMe), `None` =
/// undetectable (network mounts, unusual device stacking, unsupported
/// platform). Detection runs once per path; the verdict is cached for the
/// process lifetime.
pub fn path_is_rotational(path: &Path) -> Option<bool> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<bool>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cached) = cache
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .get(path)
    {
        return *cached;
    }
    let result = detect_rotational(path);
    info!(
        path = %path.display(),
        rotational = ?result,
        "storage medium detected"
    );
    cache
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .insert(path.to_path_buf(), result);
    result
}

#[cfg(target_os = "linux")]
fn detect_rotational(path: &Path) -> Option<bool> {
    let canonical = path.canonicalize().ok()?;
    let mounts = std::fs::read_to_string("/proc/self/mounts").ok()?;
    let (device, fstype) = mount_for_path(&mounts, &canonical)?;
    if fstype == "zfs" {
        // The mount source is the dataset (pool/some/ds); the pool's vdevs
        // carry the physical medium.
        let pool = device.split('/').next()?;
        let output = Command::new("zpool")
            .args(["list", "-vHP", pool])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let devices = parse_zpool_devices(&String::from_utf8_lossy(&output.stdout));
        return rotational_of_devices(&devices);
    }
    if !device.starts_with("/dev/") {
        return None;
    }
    rotational_of_devices(std::slice::from_ref(&device))
}

/// Longest-prefix mount entry for `path` from `/proc/self/mounts` content.
/// Returns `(source_device, fstype)`. Later entries win ties (overmounts).
#[cfg(any(target_os = "linux", test))]
fn mount_for_path(mounts: &str, path: &Path) -> Option<(String, String)> {
    let mut best_components = 0_usize;
    let mut best: Option<(String, String)> = None;
    for line in mounts.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 {
            continue;
        }
        let mount_point = PathBuf::from(unescape_mount_field(fields[1]));
        if !path.starts_with(&mount_point) {
            continue;
        }
        let components = mount_point.components().count();
        if components >= best_components {
            best_components = components;
            best = Some((unescape_mount_field(fields[0]), fields[2].to_string()));
        }
    }
    best
}

/// `/proc/self/mounts` octal-escapes spaces/tabs/newlines/backslashes
/// (e.g. `\040` for a space).
#[cfg(any(target_os = "linux", test))]
fn unescape_mount_field(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let code: String = chars.by_ref().take(3).collect();
        match u8::from_str_radix(&code, 8) {
            Ok(byte) => out.push(byte as char),
            Err(_) => {
                out.push('\\');
                out.push_str(&code);
            }
        }
    }
    out
}

/// Physical DATA vdev device paths from `zpool list -vHP <pool>` output:
/// tab-separated, vdev rows carry the device path (grouping rows like
/// `mirror-0` are skipped). Auxiliary sections (cache/log/spare/special)
/// terminate the scan — an HDD hot-spare must not mark an all-flash pool
/// rotational.
#[cfg(any(target_os = "linux", test))]
fn parse_zpool_devices(output: &str) -> Vec<String> {
    let mut devices = Vec::new();
    for line in output.lines() {
        let Some(name) = line.split('\t').find(|field| !field.is_empty()) else {
            continue;
        };
        if matches!(name, "cache" | "logs" | "log" | "spares" | "spare" | "special" | "dedup") {
            break;
        }
        if name.starts_with("/dev/") {
            devices.push(name.to_string());
        }
    }
    devices
}

/// Any rotational member makes the whole set rotational (a pool is only as
/// seek-friendly as its slowest vdev); an undetectable member (with no
/// rotational one) makes the verdict undetectable.
#[cfg(target_os = "linux")]
fn rotational_of_devices(devices: &[String]) -> Option<bool> {
    if devices.is_empty() {
        return None;
    }
    let mut unknown = false;
    for device in devices {
        match device_rotational(Path::new(device)) {
            Some(true) => return Some(true),
            Some(false) => {}
            None => unknown = true,
        }
    }
    if unknown { None } else { Some(false) }
}

#[cfg(target_os = "linux")]
fn device_rotational(device: &Path) -> Option<bool> {
    let canonical = device.canonicalize().unwrap_or_else(|_| device.to_path_buf());
    let name = canonical.file_name()?.to_str()?;
    block_name_rotational(name, 0)
}

#[cfg(target_os = "linux")]
fn block_name_rotational(name: &str, depth: usize) -> Option<bool> {
    if depth > 6 {
        return None;
    }
    let sys = PathBuf::from("/sys/class/block").join(name);
    if !sys.exists() {
        return None;
    }
    // Stacked devices (dm/LVM/md): the verdict comes from the members.
    if let Ok(slaves) = std::fs::read_dir(sys.join("slaves")) {
        let names: Vec<String> = slaves
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .collect();
        if !names.is_empty() {
            let mut unknown = false;
            for slave in &names {
                match block_name_rotational(slave, depth + 1) {
                    Some(true) => return Some(true),
                    Some(false) => {}
                    None => unknown = true,
                }
            }
            return if unknown { None } else { Some(false) };
        }
    }
    if let Ok(raw) = std::fs::read_to_string(sys.join("queue/rotational")) {
        return match raw.trim() {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        };
    }
    // Partitions have no queue/ directory; the canonical /sys path nests them
    // under the whole-disk directory, which does.
    let canonical_sys = sys.canonicalize().ok()?;
    let parent = canonical_sys.parent()?.file_name()?.to_str()?;
    if parent == "block" {
        return None;
    }
    block_name_rotational(parent, depth + 1)
}

#[cfg(windows)]
fn detect_rotational(path: &Path) -> Option<bool> {
    use std::os::windows::process::CommandExt;
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let drive = drive_letter(&canonical)?;
    let script = format!(
        "(Get-PhysicalDisk -DeviceNumber (Get-Partition -DriveLetter {drive}).DiskNumber).MediaType"
    );
    let mut command = Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "SSD" => Some(false),
        "HDD" => Some(true),
        _ => None,
    }
}

#[cfg(windows)]
fn drive_letter(path: &Path) -> Option<char> {
    use std::path::{Component, Prefix};
    match path.components().next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => Some(letter as char),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn detect_rotational(_path: &Path) -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_for_path_prefers_longest_prefix() {
        let mounts = "\
/dev/sda1 / ext4 rw 0 0
zfs_pool/data /zfs_pool zfs rw 0 0
zfs_pool/data/deep /zfs_pool/deep zfs rw 0 0
";
        let (device, fstype) =
            mount_for_path(mounts, Path::new("/zfs_pool/deep/file")).expect("mount");
        assert_eq!(device, "zfs_pool/data/deep");
        assert_eq!(fstype, "zfs");
        let (device, fstype) = mount_for_path(mounts, Path::new("/etc/hosts")).expect("mount");
        assert_eq!(device, "/dev/sda1");
        assert_eq!(fstype, "ext4");
    }

    #[test]
    fn mount_field_octal_escapes_decode() {
        assert_eq!(unescape_mount_field("/mnt/my\\040disk"), "/mnt/my disk");
        assert_eq!(unescape_mount_field("/plain"), "/plain");
    }

    #[test]
    fn zpool_vdev_devices_parse() {
        // The HDD spare and cache device must NOT count: only data vdevs
        // decide whether parallel small-file writes are safe.
        let output = "\
zfs_pool\t10.9T\t5.06T\t5.81T\t-\t-\t5%\t46%\t1.00x\tONLINE\t-
\tmirror-0\t10.9T\t5.06T\t5.81T\t-\t-\t5%\t46.4%\t-\tONLINE
\t\t/dev/sda1\t10.9T\t-\t-\t-\t-\t-\t-\t-\tONLINE
\t\t/dev/sdb1\t10.9T\t-\t-\t-\t-\t-\t-\t-\tONLINE
cache\t-\t-\t-\t-\t-\t-\t-\t-\t-
\t/dev/nvme1n1\t512G\t-\t-\t-\t-\t-\t-\t-\tONLINE
spare\t-\t-\t-\t-\t-\t-\t-\t-\t-
\t/dev/sdz1\t10.9T\t-\t-\t-\t-\t-\t-\t-\tAVAIL
";
        assert_eq!(
            parse_zpool_devices(output),
            vec!["/dev/sda1".to_string(), "/dev/sdb1".to_string()]
        );
    }
}
