//! Fix a fake drive: repartition and format it to the capacity that really
//! stores data (diskpart on Windows, sfdisk + mkfs on Linux).

use crate::dev::MIB;
use crate::platform::{self, DiskInfo};

/// File systems `fix` can format with here; the first is the default
/// (FreeBSD's base system has no exFAT).
pub const FILESYSTEMS: &[&str] = if cfg!(target_os = "freebsd") {
    &["fat32", "ufs"]
} else if cfg!(target_os = "linux") {
    &["exfat", "fat32", "ntfs", "ext4"]
} else {
    &["exfat", "fat32", "ntfs"]
};

/// Partition size in MiB for a drive whose first `usable` bytes are real:
/// diskpart starts the partition at 1 MiB, and we keep 1 MiB of margin.
pub fn partition_mib(usable: u64) -> u64 {
    (usable / MIB).saturating_sub(2)
}

/// Wipe the partition table of `disk` and create one partition of `size_mib`
/// (None = whole disk), formatted with `fs`. Returns the tools' output.
pub fn fix(disk: &DiskInfo, size_mib: Option<u64>, fs: &str, label: &str) -> Result<String, String> {
    if !platform::is_admin() {
        return Err("fix needs administrator rights.".into());
    }
    // Re-check right before acting: same device, still not protected.
    let now = platform::check_disk(&disk.id)?;
    disk.check_identity(&now)?;
    let disk = &now;
    let fs = fs.to_ascii_lowercase();
    if !FILESYSTEMS.contains(&fs.as_str()) {
        return Err(format!("Unsupported file system {fs}; use {}.", FILESYSTEMS.join(", ")));
    }
    let mib = size_mib.unwrap_or(disk.size / MIB);
    if size_mib.is_some() && mib + 1 > disk.size / MIB {
        return Err(format!("A {mib} MiB partition does not fit on this {} MiB disk.", disk.size / MIB));
    }
    if cfg!(windows) && fs == "fat32" && mib > 32 * 1024 {
        return Err("Windows cannot format FAT32 above 32 GB; use exfat.".into());
    }
    if mib < 8 {
        return Err("The usable size is too small to partition.".into());
    }
    let label: String = label.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ' ').take(11).collect();
    platform::repartition(disk, size_mib, &fs, &label)
}
