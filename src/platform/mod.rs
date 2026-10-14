//! Everything OS-specific lives below this module: disk and volume enumeration,
//! drive identity, USB link speed, the system-disk guard, raw disk access,
//! elevation, free-space mapping and repartitioning. Each OS file implements the
//! same functions over the shared types here.

use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::{fs::File, io, process::Command};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(target_os = "linux")]
pub mod x11gui;

#[cfg(target_os = "freebsd")]
mod freebsd;
#[cfg(target_os = "freebsd")]
pub use freebsd::*;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct UsbInfo {
    pub vid: String,
    pub pid: String,
    /// USB version the device reports (bcdUSB), e.g. "3.20".
    pub usb_version: String,
    pub link: String,
    pub superspeed_capable: Option<bool>,
    /// Supports SuperSpeed+ (10 Gbit/s or more).
    #[serde(default)]
    pub superspeed_plus_capable: Option<bool>,
    /// Device serial (None when the device has none).
    pub serial: Option<String>,
    /// True when the link runs at USB 2 (or slower) speed.
    pub slow_link: bool,
    /// Hub device path and port number (for diagnostics and port power-cycling).
    #[serde(default)]
    pub hub: String,
    #[serde(default)]
    pub port: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct VolumeInfo {
    /// Windows volume GUID path, or the Linux partition device (/dev/sdb1).
    pub guid: String,
    pub mount_points: Vec<String>,
    pub label: String,
    pub fs: String,
    pub size: u64,
    pub free: u64,
    /// (disk id, byte offset on the disk, length)
    pub extents: Vec<(String, u64, u64)>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DiskInfo {
    /// What `--disk` takes: the disk number on Windows, the kernel name (sda) on Linux.
    pub id: String,
    /// Device path: \\.\PhysicalDriveN or /dev/sdX.
    pub path: String,
    pub vendor: String,
    pub product: String,
    pub revision: String,
    pub serial: String,
    pub bus: String,
    pub removable: bool,
    pub size: u64,
    pub sector_size: u32,
    pub usb: Option<UsbInfo>,
    pub volumes: Vec<VolumeInfo>,
    /// Why this disk can never be a test target (system disk etc.).
    pub protected: Option<String>,
    /// Stable device identity: Windows PnP instance id, Linux sysfs device path.
    pub instance_id: String,
    /// Reason the serial looks like a placeholder.
    pub placeholder_serial: Option<String>,
}

impl DiskInfo {
    pub fn name(&self) -> String {
        let n = format!("{} {}", self.vendor, self.product).trim().to_string();
        if n.is_empty() { "(no name)".into() } else { n }
    }
    /// Main mount point (or drive letter) of each volume.
    pub fn letters(&self) -> String {
        let v: Vec<_> = self.volumes.iter().filter_map(|v| v.mount_points.iter().min_by_key(|m| m.len()).cloned()).collect();
        v.join(" ")
    }
}

/// Placeholder check for a disk: the drive's own serial counts (behind a SATA/NVMe
/// bridge it is the real drive's); the USB serial only when the drive has none.
pub fn suspicious_serial(d: &DiskInfo) -> Option<String> {
    let usb = d.usb.as_ref().and_then(|u| u.serial.clone()).unwrap_or_default();
    placeholder_serial(if d.serial.trim().is_empty() { &usb } else { &d.serial })
}

/// Why a serial number looks like a placeholder, if it does.
pub fn placeholder_serial(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return Some("blank serial".into());
    }
    let first = s.chars().next().unwrap();
    if s.chars().all(|c| c == first) {
        return Some(format!("serial is one repeated character ({s})"));
    }
    // 012345678942 is the Realtek USB bridge default.
    let known = ["1234567890", "0123456789", "123456789ABCDEF", "0123456789ABCDEF", "1234567890ABCDEF", "AAAAAAAAAAAA", "012345678942"];
    if known.iter().any(|k| s.eq_ignore_ascii_case(k) || s.to_ascii_uppercase().ends_with(*k)) {
        return Some(format!("well-known placeholder serial ({s})"));
    }
    let zeros = s.chars().filter(|&c| c == '0').count();
    if s.len() >= 6 && zeros + 1 >= s.len() {
        return Some(format!("serial is almost all zeros ({s})"));
    }
    None
}

/// Data direction of a SCSI pass-through command (SMART reads; see `smart.rs`).
pub enum Dir<'a> {
    /// Read this many bytes.
    In(usize),
    /// Send these bytes.
    Out(&'a [u8]),
}

/// An ATA command with 28-bit registers that reads one 512-byte sector (PIO data-in).
#[derive(Clone, Copy)]
pub struct Ata {
    pub command: u8,
    pub features: u8,
    pub lba_mid: u8,
    pub lba_high: u8,
}

/// Partition device for partition 1 of a Linux disk (sda -> sda1, nvme0n1 -> nvme0n1p1).
pub fn first_partition(disk_path: &str) -> String {
    if disk_path.ends_with(|c: char| c.is_ascii_digit()) { format!("{disk_path}p1") } else { format!("{disk_path}1") }
}

// Helpers shared by the Unix platforms.

/// (usable size, free for us, block size). The usable size leaves out space
/// reserved for root (ext4's 5%), so size - free is what files really use.
#[cfg(unix)]
fn statvfs(p: &str) -> Option<(u64, u64, u64)> {
    let c = std::ffi::CString::new(p).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let f = s.f_frsize as u64;
    let reserved = (s.f_bfree as u64).saturating_sub(s.f_bavail as u64);
    Some(((s.f_blocks as u64).saturating_sub(reserved) * f, s.f_bavail as u64 * f, s.f_bsize as u64))
}

/// File system type from its superblock, for systems without udev (OpenWrt,
/// Alpine): (offset, magic, name). Needs root to read the device; None otherwise.
#[cfg(unix)]
fn sniff_fs(dev: &str) -> Option<String> {
    use std::io::Read;
    const MAGIC: &[(usize, &[u8], &str)] = &[
        (3, b"EXFAT   ", "exfat"),
        (3, b"NTFS    ", "ntfs"),
        (82, b"FAT32   ", "vfat"),
        (54, b"FAT1", "vfat"),
        (0, b"XFSB", "xfs"),
        (0, b"LUKS\xba\xbe", "crypto_LUKS"),
        (0, b"hsqs", "squashfs"),
        (1080, &[0x53, 0xef], "ext"),
        (1024, &[0x10, 0x20, 0xf5, 0xf2], "f2fs"),
        (4086, b"SWAPSPACE2", "swap"),
        (32769, b"CD001", "iso9660"),
        (65600, b"_BHRfS_M", "btrfs"),
    ];
    let mut b = vec![0u8; 65608];
    let mut f = File::open(dev).ok()?;
    let mut got = 0;
    while got < b.len() {
        match f.read(&mut b[got..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => got += k,
        }
    }
    let name = MAGIC.iter().find(|(o, m, _)| o + m.len() <= got && &b[*o..o + m.len()] == *m)?.2;
    if name != "ext" {
        return Some(name.into());
    }
    // ext2/3/4 by feature flags: incompat EXTENTS (0x40) is ext4, compat HAS_JOURNAL (0x4) ext3.
    let flag = |o: usize| u32::from_le_bytes(b[1024 + o..1028 + o].try_into().unwrap());
    Some(if flag(0x60) & 0x40 != 0 { "ext4" } else if flag(0x5c) & 4 != 0 { "ext3" } else { "ext2" }.into())
}

#[cfg(unix)]
fn run(cmd: &str, args: &[&str], input: Option<&str>) -> Result<String, String> {
    use std::io::Write;
    let mut c = Command::new(cmd);
    c.args(args).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = c.spawn().map_err(|e| if e.kind() == io::ErrorKind::NotFound { format!("{cmd} is not installed") } else { format!("cannot run {cmd}: {e}") })?;
    if let Some(i) = input {
        child.stdin.take().unwrap().write_all(i.as_bytes()).map_err(|e| e.to_string())?;
    }
    drop(child.stdin.take());
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        return Err(format!("{cmd} failed:\n{text}"));
    }
    Ok(text)
}
