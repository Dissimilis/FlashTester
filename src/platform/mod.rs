//! Everything OS-specific lives below this module: disk and volume enumeration,
//! drive identity, USB link speed, the system-disk guard, raw disk access,
//! elevation, free-space mapping and repartitioning. Each OS file implements the
//! same functions over the shared types here.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[cfg(unix)]
use std::fs::File;
use std::io;

/// Validate before any discard ioctl, including on a handle opened read-only.
fn discard_range(writable: bool, size: u64, sector: u32, off: u64, len: u64) -> io::Result<()> {
    if !writable { return Err(io::Error::new(io::ErrorKind::PermissionDenied, "disk was opened read-only")); }
    let sector = u64::from(sector.max(512));
    if len == 0 || !off.is_multiple_of(sector) || !len.is_multiple_of(sector) || off > size || len > size - off || size > i64::MAX as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "discard range is unaligned or outside the disk"));
    }
    Ok(())
}
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use std::process::Command;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
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
    /// OS reports both non-rotating media and discard support; unknown on failure.
    #[serde(default)]
    pub ssd_hint: Option<bool>,
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

/// Disks above this size get GPT when repartitioned: MBR cannot address them.
const MBR_LIMIT: u64 = 2 << 40;

/// Sets each disk's protection; when the system disk can't be found, every disk is refused.
fn mark_protected(disks: &mut [DiskInfo], prot: &io::Result<BTreeMap<String, String>>) {
    for d in disks {
        d.protected = match prot {
            Ok(m) => m.get(&d.id).cloned(),
            Err(e) => Some(format!("cannot identify the system disk ({e}); refusing all disks")),
        };
    }
}

/// The listed disk `id` names; `n` is `id` without its device-path prefix.
fn find_disk(id: &str, n: &str) -> io::Result<DiskInfo> {
    list_disks().into_iter().find(|d| d.id == n).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("disk {id} not found")))
}

/// A disk that may be tested, or a refusal message.
pub fn check_disk(id: &str) -> Result<DiskInfo, String> {
    let d = disk(id).map_err(|e| e.to_string())?;
    if let Some(why) = &d.protected {
        return Err(format!("Disk {} ({}) is protected: {why}. This operation is not allowed.", d.id, d.name()));
    }
    Ok(d)
}

/// Refuses a folder whose volume reaches a protected disk.
fn refuse_protected(p: &Path, v: &VolumeInfo) -> Result<(), String> {
    let prot = protected_disks().map_err(|e| format!("cannot identify the system disk ({e}); refusing"))?;
    for e in &v.extents {
        if let Some(why) = prot.get(&e.0) {
            return Err(format!("{} is on disk {} which is protected: {why}. This operation is not allowed.", p.display(), e.0));
        }
    }
    Ok(())
}

#[cfg(unix)]
pub fn free_space(path: &Path) -> io::Result<u64> {
    statvfs(&path.to_string_lossy()).map(|s| s.1).ok_or_else(io::Error::last_os_error)
}

#[cfg(unix)]
pub fn alloc_unit(root: &Path) -> io::Result<u64> {
    statvfs(&root.to_string_lossy()).map(|s| s.2).ok_or_else(io::Error::last_os_error)
}

/// Identity of the file system an open file lives on.
#[cfg(unix)]
pub fn fs_id(f: &File) -> io::Result<u64> {
    Ok(f.metadata()?.dev())
}

/// Identity of the file system holding `p`.
#[cfg(unix)]
pub fn fs_id_path(p: &Path) -> io::Result<u64> {
    Ok(std::fs::metadata(p)?.dev())
}

/// Where full tests keep a copy of their manifest, in case the drive loses it: this
/// user's own folder in /tmp. Another user may have planted a link or folder under
/// that name, so it must be a real folder this user owns and nobody else can write
/// to; /tmp's sticky bit then keeps others from swapping it. Not $TMPDIR: a root
/// shell may have inherited a folder its user controls.
#[cfg(unix)]
pub fn manifest_dir() -> io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let me = unsafe { libc::geteuid() };
    let d = Path::new("/tmp").join(format!("flashtester-{me}"));
    if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(&d)
        && e.kind() != io::ErrorKind::AlreadyExists
    {
        return Err(e);
    }
    let m = std::fs::symlink_metadata(&d)?;
    if !m.is_dir() || m.uid() != me || m.mode() & 0o022 != 0 {
        return Err(io::Error::other(format!("{} is not a private folder", d.display())));
    }
    Ok(d)
}

/// No-op outside Windows: servers don't suspend, and inhibiting it on Linux needs a session bus.
#[cfg(unix)]
pub struct KeepAwake;

#[cfg(unix)]
impl KeepAwake {
    pub fn new() -> KeepAwake {
        KeepAwake
    }
}

#[cfg(unix)]
impl Default for KeepAwake {
    fn default() -> Self {
        Self::new()
    }
}

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

/// The desktop user behind a root GUI started through pkexec, sudo or doas: uid, gid, home.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn invoking_user() -> Option<(u32, u32, PathBuf)> {
    if !is_admin() {
        return None;
    }
    // pkexec and sudo pass the caller's uid, doas only its name.
    let (field, id) = match std::env::var("PKEXEC_UID").or_else(|_| std::env::var("SUDO_UID")) {
        Ok(uid) => (2, uid),
        Err(_) => (0, std::env::var("DOAS_USER").ok()?),
    };
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().map(|l| l.split(':').collect::<Vec<_>>()).find(|f| f.len() >= 6 && f[field] == id).and_then(|f| Some((f[2].parse().ok()?, f[3].parse().ok()?, PathBuf::from(f[5]))))
}

/// Where the GUI saves reports: ~/Documents/FlashTester (or ~/FlashTester), in the
/// desktop user's home even when the GUI runs as root.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn reports_dir() -> PathBuf {
    let home = invoking_user().map(|u| u.2).or_else(|| std::env::var_os("HOME").map(PathBuf::from)).unwrap_or_default();
    let docs = home.join("Documents");
    if docs.is_dir() { docs } else { home }.join("FlashTester")
}

/// Creates `dir` and new files in it (never overwriting). A root GUI writes these
/// into the desktop user's home, which that user controls (links, swapped folders),
/// so a copy of this exe running as that user does the writing: it can reach
/// nothing the user could not write anyway.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn write_user_files(dir: &Path, files: &[(&str, &[u8])]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::process::CommandExt;
    let Some((uid, gid, _)) = invoking_user() else {
        for (name, data) in files {
            write_new_file(&dir.join(name), data)?;
        }
        return Ok(());
    };
    for (name, data) in files {
        let mut c = Command::new(std::env::current_exe()?);
        c.arg("write-new-file").arg(dir.join(name)).uid(uid).gid(gid).current_dir("/").env_clear();
        // Only stdin and stderr: no descriptor of root's may reach the user's process.
        let mut child = c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn()?;
        let sent = child.stdin.take().unwrap().write_all(data);
        let out = child.wait_with_output()?;
        sent?;
        if !out.status.success() {
            return Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()));
        }
    }
    Ok(())
}

/// Creates `path` and its folder, refusing an existing file or link.
#[cfg(unix)]
pub fn write_new_file(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new().write(true).create_new(true).open(path)?.write_all(data)
}

/// Open with the desktop's default app; not as root (browsers refuse or misbehave),
/// the GUI lists the saved files anyway.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn open_file(p: &Path) -> io::Result<()> {
    std::fs::metadata(p)?;
    if is_admin() {
        return Err(io::Error::other("Cannot open desktop apps as root. Open the saved path from your desktop session."));
    }
    Command::new("xdg-open").arg(std::path::absolute(p)?).spawn().map(|_| ())
}

/// Start this exe's GUI as root through pkexec (polkit asks for the password). The
/// root copy connects to the caller's X display with the caller's X cookie; pkexec
/// clears the environment, so both go in as arguments (sudo and doas reset them too,
/// so the fallback hint spells them out). The caller must stay alive until pkexec
/// exits: pkexec refuses to serve a dead parent.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn relaunch_root() -> Result<std::process::Child, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let display = std::env::var("DISPLAY").map_err(|_| "DISPLAY is not set.".to_string())?;
    let mut args = vec![exe.into_os_string(), "gui".into(), "--display".into(), display.into()];
    // Absolute: pkexec runs the root copy from root's home.
    let xauth = std::env::var_os("XAUTHORITY").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".Xauthority")));
    if let Some(x) = xauth.filter(|x| x.is_file()).and_then(|x| std::path::absolute(x).ok()) {
        args.extend(["--xauthority".into(), x.into_os_string()]);
    }
    Command::new("pkexec").args(&args).spawn().map_err(|e| {
        let quote = |a: &std::ffi::OsString| {
            let a = a.to_string_lossy();
            if a.chars().all(|c| c.is_ascii_alphanumeric() || "/._:-".contains(c)) { a.into_owned() } else { format!("'{}'", a.replace('\'', r"'\''")) }
        };
        let cmd = args.iter().map(quote).collect::<Vec<_>>().join(" ");
        let hint = format!("Start FlashTester as root instead: sudo {cmd} (or doas {cmd})");
        if e.kind() == io::ErrorKind::NotFound { format!("pkexec is not installed. {hint}") } else { format!("Cannot run pkexec ({e}). {hint}") }
    })
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

    #[cfg(unix)]
    #[test]
    fn new_files_never_replace_files_or_follow_links() {
        let d = std::env::temp_dir().join(format!("ft-new-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        write_new_file(&d.join("a/r.json"), b"1").unwrap();
        assert!(write_new_file(&d.join("a/r.json"), b"2").is_err());
        std::os::unix::fs::symlink(d.join("a/r.json"), d.join("a/link.json")).unwrap();
        assert!(write_new_file(&d.join("a/link.json"), b"3").is_err());
        assert_eq!(std::fs::read(d.join("a/r.json")).unwrap(), b"1");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn manifest_dir_is_private() {
        let m = std::fs::symlink_metadata(manifest_dir().unwrap()).unwrap();
        assert!(m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0);
    }
}

#[cfg(test)]
mod discard_tests {
    use super::*;
    #[test]
    fn discard_refuses_read_only_and_invalid_ranges() {
        assert_eq!(discard_range(false, 8192, 512, 0, 8192).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        for (off, len) in [(0, 0), (1, 512), (0, 513), (8192, 512), (u64::MAX, 512)] {
            assert!(discard_range(true, 8192, 512, off, len).is_err());
        }
        assert!(discard_range(true, 8192, 512, 0, 8192).is_ok());
        assert!(discard_range(true, 8192, 4096, 512, 4096).is_err());
    }
}

fn discard_error(e: io::Error) -> io::Error {
    #[cfg(windows)]
    let unsupported = matches!(e.raw_os_error(), Some(1 | 50));
    #[cfg(unix)]
    let unsupported = matches!(e.raw_os_error(), Some(libc::EOPNOTSUPP | libc::ENOTTY));
    if unsupported { io::Error::new(io::ErrorKind::Unsupported, e) } else { e }
}

// Neither non-rotating media nor discard alone distinguishes a USB SSD from a stick.
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "freebsd", test))]
fn ssd_hint(nonrotating: Option<bool>, discard: Option<bool>) -> Option<bool> {
    nonrotating.zip(discard).map(|(n, d)| n && d)
}

#[cfg(any(target_os = "linux", test))]
fn linux_ssd_hint(rotational: &str, discard_max_bytes: &str) -> Option<bool> {
    let nonrotating = match rotational.trim() { "0" => Some(true), "1" => Some(false), _ => None };
    ssd_hint(nonrotating, discard_max_bytes.trim().parse::<u64>().ok().map(|n| n > 0))
}

#[cfg(any(target_os = "freebsd", test))]
fn freebsd_ssd_hint(rotationrate: &str, discard: Option<bool>, identify: &str) -> Option<bool> {
    let rotation = identify.lines().find_map(|l| l.trim().strip_prefix("media RPM")).unwrap_or("");
    let rotation = if rotationrate.is_empty() || rotationrate == "unknown" { rotation.trim() } else { rotationrate.trim() };
    let nonrotating = match rotation {
        "0" | "nonrotating" | "non-rotating" => Some(true),
        _ => rotation.parse::<u32>().ok().filter(|n| (1025..=65534).contains(n)).map(|_| false),
    };
    let trim = identify.lines().find_map(|l| l.trim().strip_prefix("Data Set Management (DSM/TRIM)"))
        .and_then(|s| match s.trim() { "yes" => Some(true), "no" => Some(false), _ => None });
    ssd_hint(nonrotating, discard.or(trim))
}

#[cfg(test)]
mod ssd_tests {
    use super::*;
    #[test]
    fn both_signals_are_required_and_missing_data_stays_unknown() {
        assert_eq!(linux_ssd_hint("0\n", "4294966784\n"), Some(true));
        assert_eq!(linux_ssd_hint("0", "0"), Some(false));
        assert_eq!(linux_ssd_hint("1", "512"), Some(false));
        for (r, d) in [("", "512"), ("0", ""), ("bad", "512"), ("0", "-1")] { assert_eq!(linux_ssd_hint(r, d), None); }
        assert_eq!(freebsd_ssd_hint("0", Some(true), ""), Some(true));
        assert_eq!(freebsd_ssd_hint("nonrotating", Some(false), ""), Some(false));
        let trim = "Data Set Management (DSM/TRIM) yes\n";
        for r in ["0", "nonrotating"] { assert_eq!(freebsd_ssd_hint(r, None, trim), Some(true)); }
        assert_eq!(freebsd_ssd_hint("7200", None, trim), Some(false));
        assert_eq!(freebsd_ssd_hint("unknown", None, trim), None);
        assert_eq!(freebsd_ssd_hint("1", None, trim), None); // GEOM prints 0, not ATA's raw 1.
        assert_eq!(freebsd_ssd_hint("0", None, ""), None);
        assert_eq!(freebsd_ssd_hint("0", None, "Data Set Management (DSM/TRIM) no"), Some(false));
        assert_eq!(freebsd_ssd_hint("", None, &format!("media RPM             non-rotating\n{trim}")), Some(true));
    }
}
