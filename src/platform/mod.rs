//! Everything OS-specific lives below this module: disk and volume enumeration,
//! drive identity, USB link speed, the system-disk guard, raw disk access,
//! elevation, free-space mapping and repartitioning. Each OS file implements the
//! same functions over the shared types here.

use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::fs::File;

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

#[cfg(target_os = "haiku")]
mod haiku;
#[cfg(target_os = "haiku")]
pub use haiku::*;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct UsbInfo {
    pub vid: String,
    pub pid: String,
    /// USB version the device reports (bcdUSB), e.g. "3.20".
    pub usb_version: String,
    /// Device release (bcdDevice), e.g. "11.00": usually the controller's firmware revision.
    #[serde(default)]
    pub release: String,
    /// Vendor and product names from the system's USB ID database (Linux), if it has them.
    #[serde(default)]
    pub db_vendor: String,
    #[serde(default)]
    pub db_product: String,
    /// The device's own manufacturer and product strings (USB string descriptors).
    #[serde(default)]
    pub manufacturer: String,
    #[serde(default)]
    pub product: String,
    /// Maximum current the device declares (bMaxPower), in mA; 0 if unknown.
    #[serde(default)]
    pub max_power_ma: u32,
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

impl UsbInfo {
    /// "VID 090C (Silicon Motion, chip maker) PID 1000, release 11.00"
    pub fn id_text(&self) -> String {
        let paren = |s: &str| if s.is_empty() { String::new() } else { format!(" ({s})") };
        let maker = crate::ids::usb_vendor(&self.vid).unwrap_or_else(|| self.db_vendor.clone());
        let release = if self.release.is_empty() { String::new() } else { format!(", release {}", self.release) };
        format!("VID {}{} PID {}{}{release}", self.vid, paren(&maker), self.pid, paren(&self.db_product))
    }

    /// "\"SMI Corporation\" / \"USB DISK\", max 500 mA": the device's own strings and declared current.
    pub fn device_text(&self) -> String {
        let q = |s: &str| if s.is_empty() { "(none)".to_string() } else { format!("\"{s}\"") };
        let power = if self.max_power_ma == 0 { String::new() } else { format!(", max {} mA", self.max_power_ma) };
        format!("{} / {}{power}", q(&self.manufacturer), q(&self.product))
    }
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
    /// Why writes and file-mode tests are blocked (system disk etc.).
    pub protected: Option<String>,
    /// Stable device identity: Windows PnP instance id, Linux sysfs device path.
    pub instance_id: String,
    /// Reason the serial looks like a placeholder.
    pub placeholder_serial: Option<String>,
    /// NVMe controller by PCI ID, or a labelled USB chip/family hint. A USB ID does
    /// not prove the physical controller model or identify the NAND inside it.
    #[serde(default)]
    pub controller: String,
    /// The CID register of an SD or MMC card in a directly attached reader.
    #[serde(default)]
    pub card: Option<CardInfo>,
}

/// An SD or (e)MMC card's identity register (CID). USB card readers don't pass it on.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct CardInfo {
    /// "SD" or "MMC".
    pub kind: String,
    /// Manufacturer ID and the maker it belongs to (empty if unknown).
    pub mid: u32,
    pub maker: String,
    /// OEM / application ID: two letters on SD cards.
    pub oem: String,
    pub name: String,
    /// Manufacture date, "11/2018".
    pub date: String,
    pub serial: String,
}

impl CardInfo {
    /// "SD card by Samsung or Transcend (maker 0x1B, OEM SM), ED4QT, made 11/2018"
    pub fn text(&self) -> String {
        let by = if self.maker.is_empty() { "unknown maker".to_string() } else { self.maker.clone() };
        format!("{} card by {by} (maker 0x{:02X}, OEM {}), {}, made {}", self.kind, self.mid, self.oem, self.name, self.date)
    }
}

impl DiskInfo {
    /// OS-reported identity and labelled hints; no vendor commands are sent.
    pub fn details(&self) -> Vec<(String, String)> {
        let mut rows: Vec<(String, String)> = [
            ("Device", self.name()), ("Serial number", self.serial.clone()),
            ("Firmware", self.revision.clone()), ("Controller / hint", self.controller.clone()),
            ("Reported capacity", format!("{} ({} bytes)", crate::report::size(self.size), self.size)),
            ("Connection", self.bus.clone()), ("Sector size", format!("{} bytes", self.sector_size)),
            ("Device path", self.path.clone()), ("Device ID", self.instance_id.clone()),
        ].into_iter().map(|(k, v)| (k.into(), if v.is_empty() { "Not reported".into() } else { v })).collect();
        if let Some(u) = &self.usb {
            rows.push(("USB ID".into(), u.id_text()));
            rows.push(("USB device".into(), u.device_text()));
            rows.push(("USB connection".into(), u.link.clone()));
        }
        if let Some(c) = &self.card {
            rows.push(("Card".into(), c.text()));
        }
        if let Some(why) = &self.protected {
            rows.push(("Write protection".into(), why.clone()));
        }
        for v in &self.volumes {
            rows.push(("Volume".into(), format!("{} {} | {} | {} | {} free of {}", v.mount_points.join(", "), v.label, v.guid, v.fs, crate::report::size(v.free), crate::report::size(v.size))));
        }
        rows
    }

    pub fn check_identity(&self, now: &Self) -> Result<(), String> {
        if self.id != now.id || self.path != now.path || self.instance_id != now.instance_id
            || self.size != now.size || self.serial != now.serial || self.sector_size != now.sector_size {
            Err(format!("Disk {} changed since it was selected. Refresh the drive list and select it again.", self.id))
        } else {
            Ok(())
        }
    }

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

/// A BCD version as four hex digits ("0210", "1100") -> "2.10", "11.00".
pub fn bcd(s: &str) -> String {
    let s = s.trim();
    if s.len() == 4 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        let major = s[..2].trim_start_matches('0');
        format!("{}.{}", if major.is_empty() { "0" } else { major }, &s[2..])
    } else {
        s.to_string()
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

/// How to make a wiped disk usable again, optionally at its real size in bytes:
/// our `fix` command, or DriveSetup on Haiku, which has no command-line partitioner.
pub fn fix_hint(id: &str, size: Option<u64>) -> String {
    match (cfg!(target_os = "haiku"), size) {
        (true, Some(s)) => format!("DriveSetup, with one partition of at most {s} bytes"),
        (true, None) => "DriveSetup".into(),
        (false, Some(s)) => format!("flashtester fix --disk {id} --size {s} --destroy"),
        (false, None) => format!("flashtester fix --disk {id} --destroy"),
    }
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
        (544, b"1SFB", "bfs"),
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

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn run(cmd: &str, args: &[&str], input: Option<&str>) -> Result<String, String> {
    use std::io::Write;
    let mut c = std::process::Command::new(cmd);
    c.args(args).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = c.spawn().map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { format!("{cmd} is not installed") } else { format!("cannot run {cmd}: {e}") })?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcd_and_id_text() {
        assert_eq!(bcd("1100"), "11.00");
        assert_eq!(bcd("0210"), "2.10");
        assert_eq!(bcd("0000"), "0.00");
        assert_eq!(bcd(" 2.10"), "2.10");
        let u = UsbInfo { vid: "090C".into(), pid: "1000".into(), release: "11.00".into(), ..Default::default() };
        assert_eq!(u.id_text(), "VID 090C (Silicon Motion, chip maker) PID 1000, release 11.00");
        let u = UsbInfo { vid: "ABCD".into(), pid: "1234".into(), db_vendor: "LogiLink".into(), db_product: "UDisk flash drive".into(), ..Default::default() };
        assert_eq!(u.id_text(), "VID ABCD (LogiLink) PID 1234 (UDisk flash drive)");
    }
}
