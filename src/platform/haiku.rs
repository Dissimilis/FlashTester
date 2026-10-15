//! Haiku (command line only): disks are the `raw` devices under /dev/disk, with
//! their partitions published next to them; volumes from fs_stat_dev; identity
//! from SCSI INQUIRY and the USB device list. Haiku has no file-block map (no
//! free-space probe) and no command-line partitioner (no `fix`).

use super::{DiskInfo, UsbInfo, VolumeInfo, sniff_fs, statvfs};
use crate::dev::{ALIGN, Dev, direct, pread, pwrite};
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Command;

// Drivers.h and device/scsi.h.
const B_GET_DEVICE_SIZE: u32 = 1;
const B_GET_GEOMETRY: u32 = 7;
const B_GET_PARTITION_INFO: u32 = 9;
const B_GET_DEVICE_NAME: u32 = 24;
const B_RAW_DEVICE_COMMAND: u32 = 10102;
const B_CD: u8 = 5;

unsafe extern "C" {
    /// What the C header's ioctl macro calls: the argument and its size.
    fn __ioctl(fd: libc::c_int, op: libc::c_ulong, arg: *mut c_void, size: usize) -> libc::c_int;
    /// The path of a node (here a volume's root directory); what BPath uses.
    fn _kern_entry_ref_to_path(dev: libc::dev_t, ino: libc::ino_t, leaf: *const libc::c_char, path: *mut libc::c_char, len: usize) -> i32;
}

#[repr(C)]
#[derive(Default)]
struct Geometry {
    bytes_per_sector: u32,
    sectors_per_track: u32,
    cylinder_count: u32,
    head_count: u32,
    device_type: u8,
    removable: u8,
    read_only: u8,
    write_once: u8,
    bytes_per_physical_sector: u32,
}

#[repr(C)]
struct PartitionInfo {
    offset: i64,
    size: i64,
    logical_block_size: i32,
    session: i32,
    partition: i32,
    device: [u8; 256],
}

#[repr(C)]
struct RawCommand {
    command: [u8; 16],
    command_length: u8,
    flags: u8,
    scsi_status: u8,
    cam_status: u8,
    data: *mut c_void,
    data_length: usize,
    sense_data: *mut c_void,
    sense_data_length: usize,
    timeout: i64,
}

fn ctl<T>(f: &File, op: u32, v: &mut T) -> io::Result<()> {
    if unsafe { __ioctl(f.as_raw_fd(), op as libc::c_ulong, v as *mut T as *mut c_void, std::mem::size_of::<T>()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn is_admin() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

fn cstr(b: &[libc::c_char]) -> String {
    let b: Vec<u8> = b.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&b).into_owned()
}

/// A mounted volume.
struct Mount {
    dev: libc::dev_t,
    /// The device it is on (/dev/disk/usb/0/0/0); empty for packagefs, ramfs and the like.
    device: String,
    point: String,
    fs: String,
    label: String,
    size: u64,
    free: u64,
}

fn mounts() -> Vec<Mount> {
    let mut out = vec![];
    let mut pos = 0i32;
    loop {
        let dev = unsafe { libc::next_dev(&mut pos) };
        if dev < 0 {
            break;
        }
        let mut fi: libc::fs_info = unsafe { std::mem::zeroed() };
        if unsafe { libc::fs_stat_dev(dev, &mut fi) } != 0 {
            continue;
        }
        let mut p = [0 as libc::c_char; 1024];
        let point = if unsafe { _kern_entry_ref_to_path(dev, fi.root, std::ptr::null(), p.as_mut_ptr(), p.len()) } == 0 { cstr(&p) } else { String::new() };
        let bs = fi.block_size.max(0) as u64;
        out.push(Mount {
            dev,
            device: cstr(&fi.device_name),
            point,
            fs: cstr(&fi.fsh_name),
            label: cstr(&fi.volume_name),
            size: fi.total_blocks.max(0) as u64 * bs,
            free: fi.free_blocks.max(0) as u64 * bs,
        });
    }
    out
}

/// The disk id (usb/0/0) a device path belongs to: the disk's own raw device or
/// one of the partitions published next to it.
fn disk_of_device(dev: &str) -> io::Result<String> {
    let rel = dev.strip_prefix("/dev/disk/").ok_or_else(|| other(format!("{dev} is not a disk device")))?;
    if rel.starts_with("virtual/files/") {
        return Err(other(format!("{dev} is a file-backed disk image, whose backing file cannot be checked")));
    }
    let id = match rel.strip_suffix("/raw") {
        Some(id) => id,
        None => rel.rsplit_once('/').map(|x| x.0).unwrap_or(""),
    };
    if id.is_empty() || !Path::new(&format!("/dev/disk/{id}/raw")).exists() {
        return Err(other(format!("cannot find the disk holding {dev}")));
    }
    Ok(id.to_string())
}

const VM_SETTINGS: &str = "/boot/home/config/settings/kernel/drivers/virtual_memory";

/// Disks that must never be tested, with the reason. Fails closed: if the
/// running system's disks cannot be identified, this is an error.
pub fn protected_disks() -> io::Result<BTreeMap<String, String>> {
    protected(&mounts())
}

fn protected(ms: &[Mount]) -> io::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let c = std::ffi::CString::new("/boot").unwrap();
    let boot = unsafe { libc::dev_for_path(c.as_ptr()) };
    let m = ms.iter().find(|m| m.dev == boot).ok_or_else(|| other("cannot find the boot volume"))?;
    let d = disk_of_device(&m.device).map_err(|e| other(format!("cannot map /boot to a disk: {e}")))?;
    out.insert(d, "system disk (holds /boot)".to_string());
    // The swap file is on the boot volume unless VirtualMemory moved it. The kernel
    // finds that volume by device and, if the device changed, by name: protect both.
    if let Some(s) = swap_settings(std::fs::read_to_string(VM_SETTINGS))? {
        let get = |k: &str| s.lines().find_map(|l| l.trim().strip_prefix(k).map(|v| v.trim().trim_matches('"').to_string())).filter(|v| !v.is_empty());
        let mut devs: Vec<String> = get("swap_volume_device").into_iter().collect();
        if let Some(name) = get("swap_volume_name") {
            devs.extend(ms.iter().filter(|m| m.label == name && !m.device.is_empty()).map(|m| m.device.clone()));
        }
        for dev in devs {
            let d = disk_of_device(&dev).map_err(|e| other(format!("cannot map swap to a disk: {e}")))?;
            out.entry(d).or_insert_with(|| "holds swap".to_string());
        }
    }
    Ok(out)
}

fn swap_settings(result: io::Result<String>) -> io::Result<Option<String>> {
    match result {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(other(format!("cannot read swap settings: {e}"))),
    }
}

/// One USB device from `listusb -v`.
#[derive(Default)]
struct Usb {
    vid: String,
    pid: String,
    version: String,
    serial: String,
    path: String,
    storage: bool,
}

/// "[Device /dev/bus/usb/0/1]" blocks of "Key ...... value" lines.
fn parse_listusb(s: &str) -> Vec<Usb> {
    let mut out: Vec<Usb> = vec![];
    for l in s.lines() {
        if let Some(p) = l.trim().strip_prefix("[Device ") {
            out.push(Usb { path: p.trim_end_matches(']').to_string(), ..Default::default() });
            continue;
        }
        let (Some(u), t) = (out.last_mut(), l.trim()) else { continue };
        let Some(i) = t.find(" .") else { continue };
        let v = t[i..].trim_start_matches([' ', '.']);
        let first = v.split_whitespace().next().unwrap_or("");
        let hex = |x: &str| x.trim_start_matches("0x").to_ascii_uppercase();
        match &t[..i] {
            "Vendor ID" => u.vid = hex(first),
            "Product ID" => u.pid = hex(first),
            "USB Version" => u.version = first.to_string(),
            "Serial Number" => u.serial = v.trim_matches('"').to_string(),
            // Interface class 8: mass storage.
            "Class" if first == "0x08" => u.storage = true,
            _ => {}
        }
    }
    out
}

/// USB identity, when it is unambiguous: Haiku does not tell which USB device a
/// disk is on, so this only works with one USB storage device attached.
fn usb_info() -> Option<UsbInfo> {
    let o = Command::new("listusb").arg("-v").output().ok()?;
    let devs: Vec<Usb> = parse_listusb(&String::from_utf8_lossy(&o.stdout)).into_iter().filter(|u| u.storage).collect();
    let units = std::fs::read_dir("/dev/disk/usb").ok()?.count();
    let [u] = devs.as_slice() else { return None };
    if units != 1 {
        return None;
    }
    Some(UsbInfo {
        vid: u.vid.clone(),
        pid: u.pid.clone(),
        usb_version: u.version.clone(),
        serial: Some(u.serial.clone()).filter(|s| !s.is_empty()),
        hub: u.path.clone(),
        ..Default::default()
    })
}

/// Every disk id: the directories under /dev/disk holding a `raw` device.
fn disk_ids(dir: &Path, depth: u32, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if e.file_name() == "raw" {
            if let Some(id) = dir.strip_prefix("/dev/disk").ok().and_then(|r| r.to_str()) {
                out.push(id.to_string());
            }
        } else if depth < 8 && e.file_type().is_ok_and(|t| t.is_dir()) {
            disk_ids(&p, depth + 1, out);
        }
    }
}

fn bus(id: &str) -> &'static str {
    match id.split('/').next().unwrap_or("") {
        "usb" => "USB",
        "nvme" => "NVMe",
        "mmc" => "MMC",
        // AHCI and IDE disks sit on Haiku's SCSI layer too.
        "scsi" | "atapi" | "ata" => "SCSI",
        _ if id.starts_with("virtual/files/") => "File-backed virtual",
        _ => "Virtual",
    }
}

fn trimmed(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string()
}

/// Device size, from B_GET_DEVICE_SIZE (a size_t) or else the geometry.
fn size_of(f: &File, g: &Geometry) -> u64 {
    let mut n: usize = 0;
    if ctl(f, B_GET_DEVICE_SIZE, &mut n).is_ok() && n > 0 {
        return n as u64;
    }
    g.bytes_per_sector as u64 * g.sectors_per_track as u64 * g.cylinder_count as u64 * g.head_count as u64
}

fn volume(disk: &str, dev: &str, offset: u64, size: u64, ms: &[Mount]) -> VolumeInfo {
    let mine: Vec<&Mount> = ms.iter().filter(|m| m.device == dev).collect();
    let mut v = VolumeInfo { guid: dev.to_string(), mount_points: mine.iter().map(|m| if m.point.is_empty() { format!("(volume {})", m.label) } else { m.point.clone() }).collect(), size, extents: vec![(disk.to_string(), offset, size)], ..Default::default() };
    match mine.first() {
        Some(m) => {
            v.fs = m.fs.clone();
            v.label = m.label.clone();
            v.size = m.size.max(1).min(size.max(m.size));
            v.free = m.free;
        }
        None => v.fs = sniff_fs(dev).unwrap_or_default(),
    }
    v
}

/// The partitions published next to a disk's raw device, with offset and size.
fn partitions(id: &str) -> Vec<(String, u64, u64)> {
    let mut v = vec![];
    let Ok(rd) = std::fs::read_dir(format!("/dev/disk/{id}")) else { return v };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name == "raw" || e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let dev = format!("/dev/disk/{id}/{name}");
        let Ok(f) = File::open(&dev) else { continue };
        let mut p = PartitionInfo { offset: 0, size: 0, logical_block_size: 0, session: 0, partition: 0, device: [0; 256] };
        if ctl(&f, B_GET_PARTITION_INFO, &mut p).is_ok() {
            v.push((dev, p.offset.max(0) as u64, p.size.max(0) as u64));
        }
    }
    v.sort_by_key(|x| x.1);
    v
}

fn disk_info(id: &str, ms: &[Mount]) -> Option<DiskInfo> {
    let path = format!("/dev/disk/{id}/raw");
    let f = File::open(&path).ok()?;
    let mut g = Geometry::default();
    ctl(&f, B_GET_GEOMETRY, &mut g).ok()?;
    if g.device_type == B_CD {
        return None;
    }
    let mut d = DiskInfo { id: id.to_string(), path: path.clone(), bus: bus(id).to_string(), size: size_of(&f, &g), sector_size: g.bytes_per_sector.max(512), removable: g.removable != 0, ..Default::default() };
    if d.size == 0 {
        return None;
    }
    // Whether Haiku let us ask for a serial at all: its NVMe and AHCI drivers don't.
    let mut serial_known = false;
    if id.starts_with("scsi/") {
        let inq = raw(&f, &[0x12, 0, 0, 0, 96, 0], Some(96)).ok()?;
        if inq.len() < 36 || inq[0] & 0x1F == B_CD {
            return None;
        }
        d.vendor = trimmed(&inq[8..16]);
        d.product = trimmed(&inq[16..32]);
        d.revision = trimmed(&inq[32..36]);
        // Unit serial number page; not every controller answers it.
        if let Ok(s) = raw(&f, &[0x12, 1, 0x80, 0, 252, 0], Some(252)) {
            if s.len() >= 4 && s[1] == 0x80 {
                d.serial = trimmed(&s[4..(4 + s[3] as usize).min(s.len())]);
                serial_known = true;
            }
        }
    } else {
        let mut name = [0u8; 256];
        if ctl(&f, B_GET_DEVICE_NAME, &mut name).is_ok() {
            d.product = trimmed(&name);
        }
        if d.product.is_empty() && d.bus == "NVMe" {
            d.product = "NVMe disk".into();
        }
    }
    if d.bus == "USB" {
        d.usb = usb_info();
        if let Some(u) = &d.usb {
            if d.serial.is_empty() {
                d.serial = u.serial.clone().unwrap_or_default();
            }
            serial_known = true;
        }
    }
    d.instance_id = format!("{id} {}", d.serial).trim().to_string();
    d.placeholder_serial = if serial_known { super::suspicious_serial(&d) } else { None };
    let parts = partitions(id);
    d.volumes = parts.iter().map(|(dev, off, len)| volume(id, dev, *off, *len, ms)).collect();
    // A file system straight on the disk (no partition table).
    if parts.is_empty() && (ms.iter().any(|m| m.device == path) || sniff_fs(&path).is_some()) {
        d.volumes.push(volume(id, &path, 0, d.size, ms));
    }
    Some(d)
}

pub fn list_disks() -> Vec<DiskInfo> {
    let ms = mounts();
    let prot = protected(&ms);
    let mut ids = vec![];
    disk_ids(Path::new("/dev/disk"), 0, &mut ids);
    ids.sort();
    let mut disks: Vec<DiskInfo> = ids.iter().filter_map(|id| disk_info(id, &ms)).collect();
    for d in &mut disks {
        d.protected = match &prot {
            Ok(m) => m.get(&d.id).cloned(),
            Err(e) => Some(format!("cannot identify the system disk ({e}); refusing all disks")),
        };
        if d.protected.is_none() && d.bus == "File-backed virtual" {
            d.protected = Some("file-backed disk image, whose backing file cannot be checked".into());
        }
    }
    disks
}

pub fn disk(id: &str) -> io::Result<DiskInfo> {
    let n = id.trim().trim_start_matches("/dev/disk/").trim_end_matches("/raw");
    list_disks().into_iter().find(|d| d.id == n).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("disk {id} not found")))
}

/// A disk that may be tested, or a refusal message.
pub fn check_disk(id: &str) -> Result<DiskInfo, String> {
    let d = disk(id).map_err(|e| e.to_string())?;
    if let Some(why) = &d.protected {
        return Err(format!("Disk {} ({}) cannot be tested: {why}.", d.id, d.name()));
    }
    Ok(d)
}

/// The volume (partition) holding `path`.
pub fn volume_of_path(path: &Path) -> io::Result<VolumeInfo> {
    let real = std::fs::canonicalize(path)?;
    let dev = std::fs::metadata(&real)?.dev() as libc::dev_t;
    let ms = mounts();
    let m = ms.iter().find(|m| m.dev == dev).ok_or_else(|| other(format!("cannot find the volume holding {}", real.display())))?;
    if m.device.is_empty() {
        return Err(other(format!("{} ({}) is not on a disk", if m.point.is_empty() { &m.label } else { &m.point }, m.fs)));
    }
    let disk = disk_of_device(&m.device)?;
    let (off, len) = partitions(&disk).into_iter().find(|p| p.0 == m.device).map_or((0, m.size), |p| (p.1, p.2));
    let mut v = volume(&disk, &m.device, off, len, &ms);
    if !m.point.is_empty() && !v.mount_points.contains(&m.point) {
        v.mount_points.insert(0, m.point.clone());
    }
    Ok(v)
}

/// The volume and disk behind a target folder, or a refusal message.
pub fn check_path(p: &Path) -> Result<(VolumeInfo, Option<DiskInfo>), String> {
    let v = volume_of_path(p).map_err(|e| format!("{}: cannot map to a disk ({e}); refusing", p.display()))?;
    let prot = protected_disks().map_err(|e| format!("cannot identify the system disk ({e}); refusing"))?;
    for e in &v.extents {
        if let Some(why) = prot.get(&e.0) {
            return Err(format!("{} is on disk {} which cannot be tested: {why}.", p.display(), e.0));
        }
    }
    // Haiku's FAT (and other non-BFS) drivers refuse O_DIRECT: reads would come
    // from the file cache, not the drive.
    if v.fs != "bfs" {
        return Err(format!("{} is {}: Haiku bypasses the file cache only on BFS volumes, so reads would come from memory instead of the drive. Test the whole disk with --disk, or a BFS volume.", p.display(), v.fs));
    }
    let d = v.extents.first().and_then(|e| disk(&e.0).ok());
    Ok((v, d))
}

/// Raw access to a whole disk. Haiku's raw devices bypass the file cache, but
/// nothing stops a mount while we write: the caller checks before opening.
pub struct RawDisk {
    pub file: File,
    pub size: u64,
    pub sector: u32,
    pub id: String,
}

impl RawDisk {
    pub fn open(d: &DiskInfo, write: bool) -> io::Result<RawDisk> {
        if write {
            same_disk(d).map_err(other)?;
        }
        let mut o = OpenOptions::new();
        o.read(true).write(write);
        direct(&mut o, write);
        let f = o.open(&d.path)?;
        let mut g = Geometry::default();
        let _ = ctl(&f, B_GET_GEOMETRY, &mut g);
        let size = Some(size_of(&f, &g)).filter(|&s| s > 0).unwrap_or(d.size);
        Ok(RawDisk { file: f, size, sector: d.sector_size.max(512), id: d.id.clone() })
    }
}

impl Dev for RawDisk {
    fn len(&self) -> u64 {
        self.size
    }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        let mut done = 0;
        while done < buf.len() {
            done += pwrite(&self.file, &buf[done..], off + done as u64)?;
        }
        Ok(())
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        let n = (buf.len() as u64).min(self.size.saturating_sub(off)) as usize;
        let mut done = 0;
        while done < n {
            let r = pread(&self.file, &mut buf[done..n], off + done as u64)?;
            if r == 0 {
                break;
            }
            done += r;
        }
        Ok(done)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }
    fn align(&self) -> usize {
        (self.sector as usize).max(ALIGN)
    }
}

pub fn free_space(path: &Path) -> io::Result<u64> {
    statvfs(&path.to_string_lossy()).map(|s| s.1).ok_or_else(io::Error::last_os_error)
}

pub fn alloc_unit(root: &Path) -> io::Result<u64> {
    statvfs(&root.to_string_lossy()).map(|s| s.2).ok_or_else(io::Error::last_os_error)
}

const NO_FREE_SPACE_PROBE: &str = "the free-space probe is not available on Haiku (it cannot tell where a file's blocks are on the disk); use the raw probe on an empty disk, or a full test";

pub fn free_space_prepare() -> io::Result<()> {
    Err(other(NO_FREE_SPACE_PROBE))
}

pub fn allocate_and_map(_f: &File, _len: u64, _root: &Path, _vol: &VolumeInfo) -> io::Result<Vec<(u64, u64, u64)>> {
    Err(other(NO_FREE_SPACE_PROBE))
}

pub const NO_FIX: &str = "Haiku has no command-line partitioner; repartition and format the disk with DriveSetup.";

pub fn repartition(_disk: &DiskInfo, _size_mib: Option<u64>, _fs: &str, _label: &str) -> Result<String, String> {
    Err(NO_FIX.into())
}

/// No-op on Haiku.
pub struct KeepAwake;

impl KeepAwake {
    pub fn new() -> KeepAwake {
        KeepAwake
    }
}

impl Default for KeepAwake {
    fn default() -> Self {
        Self::new()
    }
}

/// Identity of the file system an open file lives on.
pub fn fs_id(f: &File) -> io::Result<u64> {
    Ok(f.metadata()?.dev())
}

/// Identity of the file system holding `p`.
pub fn fs_id_path(p: &Path) -> io::Result<u64> {
    Ok(std::fs::metadata(p)?.dev())
}

/// Still the same device (path, size, serial), not protected, and nothing on it mounted.
fn same_disk(d: &DiskInfo) -> Result<(), String> {
    let now = check_disk(&d.id)?;
    if now.instance_id != d.instance_id || now.size != d.size || now.serial != d.serial || now.product != d.product {
        return Err(format!("disk {} is no longer the drive that was selected; aborting", d.id));
    }
    let mounted: Vec<String> = mounts().into_iter().filter(|m| disk_of_device(&m.device).is_ok_and(|x| x == d.id)).map(|m| format!("{} on {}", m.device, if m.point.is_empty() { &m.label } else { &m.point })).collect();
    if mounted.is_empty() { Ok(()) } else { Err(format!("disk {} has mounted volumes ({}); unmount them first", d.id, mounted.join(", "))) }
}

/// B_RAW_DEVICE_COMMAND: `Some(n)` reads n bytes, `None` sends `out`.
fn raw_cmd(f: &File, cdb: &[u8], read: Option<usize>, out: &[u8]) -> io::Result<Vec<u8>> {
    let mut buf = match read {
        Some(n) => vec![0u8; n],
        None => out.to_vec(),
    };
    let mut sense = [0u8; 32];
    let mut c = RawCommand {
        command: [0; 16],
        command_length: cdb.len() as u8,
        // Data in, and give back the bytes actually moved.
        flags: if read.is_some() { 0x01 | 0x02 } else { 0 },
        scsi_status: 0,
        cam_status: 0,
        data: buf.as_mut_ptr() as *mut c_void,
        data_length: buf.len(),
        sense_data: sense.as_mut_ptr() as *mut c_void,
        sense_data_length: sense.len(),
        timeout: 10_000_000,
    };
    c.command[..cdb.len()].copy_from_slice(cdb);
    ctl(f, B_RAW_DEVICE_COMMAND, &mut c).map_err(|e| if e.raw_os_error() == Some(libc::ENOTTY) { other("the driver has no SCSI pass-through on Haiku") } else { e })?;
    // CAM status 1: request completed (Haiku's AHCI also reports a stray SCSI status then).
    if c.cam_status & 0x3F == 2 {
        return Err(other("the controller driver rejected the command (CAM status 0x02, as Haiku's AHCI driver does for ATA pass-through)"));
    }
    if c.cam_status & 0x3F != 1 {
        return Err(other(format!("SCSI status {:#04x}, CAM status {:#04x}, sense {:x}/{:02x}/{:02x}", c.scsi_status, c.cam_status, sense[2] & 0xF, sense[12], sense[13])));
    }
    if read.is_none() {
        return Ok(vec![]);
    }
    buf.truncate(c.data_length.min(buf.len()));
    Ok(buf)
}

fn raw(f: &File, cdb: &[u8], n: Option<usize>) -> io::Result<Vec<u8>> {
    raw_cmd(f, cdb, n, &[])
}

// SMART pass-through (read-only commands only; see smart.rs). Only disks on
// Haiku's SCSI layer take raw commands; its AHCI translation, NVMe and USB disk
// drivers have no pass-through.

pub fn scsi(d: &DiskInfo, cdb: &[u8], dir: super::Dir) -> io::Result<Vec<u8>> {
    let f = File::open(&d.path)?;
    match dir {
        super::Dir::In(n) => raw_cmd(&f, cdb, Some(n), &[]),
        super::Dir::Out(b) => raw_cmd(&f, cdb, None, b),
    }
}

pub fn ata(_d: &DiskInfo, _a: super::Ata) -> io::Result<Vec<u8>> {
    Err(other("Haiku has no ATA pass-through"))
}

pub fn nvme(_d: &DiskInfo, _op: u8, _nsid: u32, _cdw10: u32, _len: usize) -> io::Result<Vec<u8>> {
    Err(other("Haiku's NVMe driver has no pass-through for SMART"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_missing_swap_settings_use_the_default_volume() {
        assert!(swap_settings(Err(io::ErrorKind::NotFound.into())).unwrap().is_none());
        assert_eq!(swap_settings(Ok("swap_volume_device /dev/disk/ata/1/0/raw".into())).unwrap().as_deref(), Some("swap_volume_device /dev/disk/ata/1/0/raw"));
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::InvalidData, io::ErrorKind::Other] {
            assert!(swap_settings(Err(kind.into())).is_err());
        }
    }

    #[test]
    fn listusb_storage_devices() {
        let s = "[Device /dev/bus/usb/0/1]\n    Class .................. 0x00 (Defined at Interface level) ()\n    USB Version ............ 3.0\n    Vendor ID .............. 0x46f4 (QEMU)\n    Product ID ............. 0x0001\n    Serial Number .......... \"FT 01\"\n    [Configuration 0]\n        [Interface 0]\n            [Alternate 0 active]\n                Class .............. 0x08 Mass Storage ()\n[Device /dev/bus/usb/0/4]\n    Vendor ID .............. 0x0627 (Adomax)\n                Class .............. 0x03 Human Interface Device\n";
        let u = parse_listusb(s);
        assert_eq!(u.len(), 2);
        assert!(u[0].storage && !u[1].storage);
        assert_eq!((u[0].vid.as_str(), u[0].pid.as_str(), u[0].version.as_str(), u[0].serial.as_str()), ("46F4", "0001", "3.0", "FT 01"));
        assert_eq!(u[0].path, "/dev/bus/usb/0/1");
    }

    #[test]
    fn struct_layouts_match_the_headers() {
        assert_eq!(std::mem::size_of::<RawCommand>(), 64);
        assert_eq!(std::mem::size_of::<Geometry>(), 24);
        assert_eq!(std::mem::size_of::<PartitionInfo>(), 288);
    }
}
