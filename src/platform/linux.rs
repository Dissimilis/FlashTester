//! Linux: disks and volumes from sysfs, udev and mountinfo; the system-disk
//! guard; raw disk access (O_DIRECT, O_EXCL); free-space mapping (fallocate +
//! FIEMAP); repartitioning with sfdisk and mkfs.

use super::{check_disk, CardInfo, DiskInfo, UsbInfo, VolumeInfo, run, sniff_fs, statvfs};
use crate::dev::{ALIGN, Dev, direct, pread_full, pwrite_all};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

const BLKRRPART: libc::c_ulong = 0x125F;
const FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B;
const LOOP_GET_STATUS64: libc::c_ulong = 0x4C05;
const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;
const SG_IO: libc::c_ulong = 0x2285;
const NVME_IOCTL_ADMIN_CMD: libc::c_ulong = 0xC048_4E41;

fn read(p: impl AsRef<Path>) -> String {
    std::fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn num(p: impl AsRef<Path>) -> u64 {
    read(p).parse().unwrap_or(0)
}

pub fn is_admin() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// One line of /proc/self/mountinfo.
struct Mount {
    majmin: (u64, u64),
    point: String,
    fstype: String,
    source: String,
    /// Super options (an overlay's lowerdir/upperdir).
    opts: String,
}

fn unescape(s: &str) -> String {
    // mountinfo escapes space, tab, newline and backslash as \ooo
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn mounts() -> Vec<Mount> {
    read("/proc/self/mountinfo")
        .lines()
        .filter_map(|l| {
            let (a, b) = l.split_once(" - ")?;
            let f: Vec<&str> = a.split(' ').collect();
            let g: Vec<&str> = b.split(' ').collect();
            let (ma, mi) = f.get(2)?.split_once(':')?;
            Some(Mount { majmin: (ma.parse().ok()?, mi.parse().ok()?), point: unescape(f.get(4)?), fstype: g.first()?.to_string(), source: unescape(g.get(1).unwrap_or(&"")), opts: g.get(2).unwrap_or(&"").to_string() })
        })
        .collect()
}

fn majmin_of_dev(dev: &str) -> Option<(u64, u64)> {
    let m = std::fs::metadata(dev).ok()?;
    let r = m.rdev();
    Some((libc::major(r) as u64, libc::minor(r) as u64))
}

/// What a loop device sits on (LOOP_GET_STATUS64): the backing block device, or
/// the device of the file system holding the backing file. Its backing_file path
/// can't be trusted: OpenWrt's overlay loop shows "/sda2" for /dev/sda2.
fn loop_backing(name: &str) -> io::Result<(u64, u64)> {
    let f = File::open(format!("/dev/{name}"))?;
    let mut info = [0u64; 29]; // struct loop_info64: lo_device, lo_inode, lo_rdevice, ...
    if unsafe { libc::ioctl(f.as_raw_fd(), LOOP_GET_STATUS64 as _, info.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let d = if info[2] != 0 { info[2] } else { info[0] };
    Ok((libc::major(d as _) as u64, libc::minor(d as _) as u64))
}

/// The physical disks (kernel names) behind a block device, through partitions,
/// device-mapper, md and similar stacks.
fn disks_of_block(majmin: (u64, u64)) -> io::Result<Vec<String>> {
    let p = std::fs::canonicalize(format!("/sys/dev/block/{}:{}", majmin.0, majmin.1))?;
    let dir = if p.join("partition").exists() { p.parent().unwrap().to_path_buf() } else { p };
    let name = dir.file_name().unwrap().to_string_lossy().to_string();
    // A loop device sits on a file: protect the disk holding that file. Fail closed.
    if name.starts_with("loop") {
        let mm = loop_backing(&name).map_err(|e| io::Error::other(format!("{name}: cannot read its backing device: {e}")))?;
        let mut out = vec![name.clone()];
        if mm.0 != 0 {
            out.extend(disks_of_block(mm)?);
        } else {
            // A file on a file system without a device number: RAM (a live system's
            // copytoram) needs no disk; btrfs and the like are followed to theirs.
            let ms = mounts();
            let m = ms.iter().rev().find(|m| m.majmin == mm).ok_or_else(|| io::Error::other(format!("{name}: its backing file system ({}:{}) is not mounted", mm.0, mm.1)))?;
            if !RAM_FS.contains(&m.fstype.as_str()) {
                out.extend(disks_of_mount(m)?);
            }
        }
        return Ok(out);
    }
    let slaves: Vec<_> = std::fs::read_dir(dir.join("slaves"))?.collect::<io::Result<_>>()?;
    if slaves.is_empty() {
        // A real disk has a device link; zram/ram are memory. Anything else without
        // slaves (bcache, drbd, ...) may hide a disk we can't see: fail closed.
        if dir.join("device").exists() || name.starts_with("zram") || name.starts_with("ram") {
            return Ok(vec![name]);
        }
        return Err(io::Error::other(format!("{name} is a virtual block device whose backing disks can't be identified")));
    }
    let mut out = vec![];
    for s in slaves {
        let dev = std::fs::read_to_string(s.path().join("dev"))?;
        let (a, b) = dev.trim().split_once(':').ok_or_else(|| io::Error::other("bad slave device"))?;
        let a = a.parse().map_err(|_| io::Error::other("bad slave device major"))?;
        let b = b.parse().map_err(|_| io::Error::other("bad slave device minor"))?;
        out.extend(disks_of_block((a, b))?);
    }
    Ok(out)
}

// rootfs: the initramfs itself when the system never pivots to a disk (Tiny Core).
const VIRTUAL_FS: &[&str] = &["tmpfs", "overlay", "ramfs", "rootfs", "devtmpfs", "proc", "sysfs", "cgroup2", "autofs", "fuse.mergerfs"];
const RAM_FS: &[&str] = &["tmpfs", "ramfs", "rootfs"];

/// Disks behind a mount: by its device number, or its source device when the
/// mount has an anonymous device number (btrfs).
fn disks_of_mount(m: &Mount) -> io::Result<Vec<String>> {
    let mut disks = if m.majmin.0 != 0 {
        disks_of_block(m.majmin)?
    } else {
        match majmin_of_dev(&m.source) {
            Some(mm) if mm.0 != 0 => disks_of_block(mm)?,
            _ => return Err(io::Error::other(format!("{} ({}) is not on a block device", m.point, m.fstype))),
        }
    };
    // mountinfo names one device of a multi-device btrfs; sysfs lists all of them.
    if m.fstype == "btrfs" {
        let uuid = majmin_of_dev(&m.source).map(|(a, b)| udev(&format!("{a}:{b}"))).and_then(|u| u.get("ID_FS_UUID").cloned());
        let uuid = uuid.filter(|u| !u.is_empty() && u.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
            .ok_or_else(|| io::Error::other(format!("{}: cannot identify all Btrfs backing devices", m.point)))?;
        let mut members = 0;
        for e in std::fs::read_dir(format!("/sys/fs/btrfs/{uuid}/devices"))? {
            let dev = std::fs::read_to_string(e?.path().join("dev"))?;
            let (a, b) = dev.trim().split_once(':').ok_or_else(|| io::Error::other("bad Btrfs device number"))?;
            let a = a.parse().map_err(|_| io::Error::other("bad Btrfs device major"))?;
            let b = b.parse().map_err(|_| io::Error::other("bad Btrfs device minor"))?;
            disks.extend(disks_of_block((a, b))?);
            members += 1;
        }
        if members == 0 {
            return Err(io::Error::other(format!("{}: Btrfs backing device list is empty", m.point)));
        }
    }
    disks.sort();
    disks.dedup();
    Ok(disks)
}

/// Disks behind a mount the running system needs. An overlay is followed to its
/// layers; RAM, virtual file systems and raw flash hold no testable disk.
fn system_disks(m: &Mount, ms: &[Mount], depth: u32) -> io::Result<Vec<String>> {
    // UBI/JFFS2 sit on raw flash (MTD), which is never listed as a disk.
    if ["ubifs", "jffs2"].contains(&m.fstype.as_str()) || (m.fstype != "overlay" && VIRTUAL_FS.contains(&m.fstype.as_str())) {
        return Ok(vec![]);
    }
    if m.fstype != "overlay" {
        return disks_of_mount(m);
    }
    if depth > 4 {
        return Err(io::Error::other(format!("{}: overlays nested too deep", m.point)));
    }
    // OpenWrt: / is an overlay of /rom (firmware) and /overlay (raw flash, or a
    // disk with extroot); its lowerdir=/ is meaningless after the pivot.
    let openwrt: Vec<&Mount> = ["/rom", "/overlay"].iter().filter_map(|p| ms.iter().rev().find(|x| x.point == *p)).collect();
    let (layers, hidden) = if m.point == "/" && !openwrt.is_empty() { (openwrt, false) } else { overlay_layers(m, ms)? };
    let mut out = if hidden { hidden_layer_disks(ms).map_err(|e| io::Error::other(format!("{}: overlay layers are not visible and {e}", m.point)))? } else { vec![] };
    for l in layers {
        out.extend(system_disks(l, ms, depth + 1)?);
    }
    Ok(out)
}

/// The mounts holding an overlay's lowerdir and upperdir (a live system: squashfs
/// on a loop device on the boot medium, plus tmpfs), and whether some layer paths
/// don't exist here (Ubuntu's casper mounts its layers inside the initramfs).
/// Fails when a layer lies on the overlay itself.
fn overlay_layers<'a>(o: &Mount, ms: &'a [Mount]) -> io::Result<(Vec<&'a Mount>, bool)> {
    let (mut out, mut hidden) = (vec![], false);
    for opt in o.opts.split(',') {
        let dirs: Vec<&str> = match opt.split_once('=') {
            Some(("lowerdir", v)) => v.split(':').collect(),
            Some(("lowerdir+" | "datadir+" | "upperdir", v)) => vec![v],
            _ => continue,
        };
        for dir in dirs {
            let Ok(real) = std::fs::canonicalize(unescape(dir)) else {
                hidden = true;
                continue;
            };
            let m = ms.iter().filter(|m| real.starts_with(&m.point)).max_by_key(|m| m.point.len()).ok_or_else(|| io::Error::other(format!("no mount holds {dir}")))?;
            if m.point == o.point {
                return Err(io::Error::other(format!("{}: overlay layer {dir} is not on a mount of its own", o.point)));
            }
            out.push(m);
        }
    }
    if out.is_empty() && !hidden {
        return Err(io::Error::other(format!("{}: overlay without layers", o.point)));
    }
    Ok((out, hidden))
}

/// What overlay layers mounted out of sight use: the disks behind loop devices
/// (mounted ones too when their file lies on a disk: casper also mounts its
/// squashfs at /rofs), and disks whose partitions are busy (an exclusive open
/// fails) without a visible mount, swap or holder. Needs root; fails if it can't check.
fn hidden_layer_disks(ms: &[Mount]) -> io::Result<Vec<String>> {
    let swaps = read("/proc/swaps");
    let mm = |sys: &Path| read(sys.join("dev")).split_once(':').map(|(a, b)| (a.parse().unwrap_or(0), b.parse().unwrap_or(0))).unwrap_or((0, 0));
    let mut out = vec![];
    for e in std::fs::read_dir("/sys/block")?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with("loop") {
            // Mounted loops on files in RAM or on the overlay itself (snaps) are skipped.
            let on_disk = loop_backing(&name).map(|b| b.0 != 0).unwrap_or(true);
            if e.path().join("loop/backing_file").exists() && (on_disk || !ms.iter().any(|m| m.majmin == mm(&e.path()))) {
                out.extend(disks_of_block(mm(&e.path()))?);
            }
            continue;
        }
        if skip_disk(&name) {
            continue;
        }
        let parts = partitions(&name);
        let devs = if parts.is_empty() { vec![(name.clone(), e.path())] } else { parts.iter().map(|p| (p.clone(), e.path().join(p))).collect() };
        for (dev, sys) in devs {
            let path = format!("/dev/{dev}");
            let seen = ms.iter().any(|m| m.majmin == mm(&sys) || m.source == path)
                || swaps.lines().any(|l| l.split_whitespace().next() == Some(path.as_str()))
                || std::fs::read_dir(sys.join("holders")).map(|mut d| d.next().is_some()).unwrap_or(false);
            if seen {
                continue;
            }
            match OpenOptions::new().read(true).custom_flags(libc::O_EXCL).open(&path) {
                Ok(_) => {}
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) => out.push(name.clone()),
                Err(e) => return Err(io::Error::other(format!("cannot check whether {path} is in use: {e}"))),
            }
        }
    }
    Ok(out)
}

/// Disks that must never be tested, with the reason. Fails closed: if the
/// running system's disks cannot be identified, this is an error.
pub fn protected_disks() -> io::Result<BTreeMap<String, String>> {
    let mut m = BTreeMap::new();
    let ms = mounts();
    let root = ms.iter().rev().find(|x| x.point == "/").ok_or_else(|| io::Error::other("no / mount in mountinfo"))?;
    for d in system_disks(root, &ms, 0).map_err(|e| io::Error::other(format!("cannot map / to a disk: {e}")))? {
        m.entry(d).or_insert_with(|| "system disk (holds /)".to_string());
    }
    // NixOS and Guix keep the system in their store, which may be its own mount
    // (NixOS live: an overlay on a squashfs from the boot medium).
    for point in ["/boot", "/boot/efi", "/efi", "/usr", "/var", "/nix", "/nix/store", "/gnu/store"] {
        if let Some(x) = ms.iter().rev().find(|x| x.point == point) {
            for d in system_disks(x, &ms, 0).map_err(|e| io::Error::other(format!("cannot map {point} to a disk: {e}")))? {
                m.entry(d).or_insert_with(|| format!("holds {point}"));
            }
        }
    }
    // The running kernel's modules: diskless Alpine loop-mounts them (/.modloop) from
    // a file on its boot medium, which none of the mount points above shows.
    if let Ok(dir) = std::fs::canonicalize(format!("/lib/modules/{}", read("/proc/sys/kernel/osrelease"))) {
        if let Some(x) = ms.iter().filter(|x| dir.starts_with(&x.point)).max_by_key(|x| x.point.len()) {
            for d in system_disks(x, &ms, 0).map_err(|e| io::Error::other(format!("cannot map the kernel modules to a disk: {e}")))? {
                m.entry(d).or_insert_with(|| "holds the kernel modules".to_string());
            }
        }
    }
    for l in read("/proc/swaps").lines().skip(1) {
        let Some(dev) = l.split_whitespace().next() else { continue };
        let dev = unescape(dev);
        let md = std::fs::metadata(&dev)?;
        // A swap file: map the mount that holds it. A file on a btrfs subvolume has an
        // anonymous device number (Arch's cloud image: /swap/swapfile), not a disk's.
        let disks = if md.file_type().is_file() {
            let path = std::fs::canonicalize(&dev)?;
            let x = ms.iter().filter(|x| path.starts_with(&x.point)).max_by_key(|x| x.point.len()).ok_or_else(|| io::Error::other(format!("no mount holds swap {dev}")))?;
            system_disks(x, &ms, 0)
        } else {
            disks_of_block((libc::major(md.rdev()) as u64, libc::minor(md.rdev()) as u64))
        };
        for d in disks.map_err(|e| io::Error::other(format!("cannot map swap {dev} to a disk: {e}")))? {
            m.entry(d).or_insert_with(|| "holds swap".to_string());
        }
    }
    Ok(m)
}

/// udev's *_ENC values escape bytes as \xNN.
fn udev_dec(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1] == b'x' {
            if let Ok(v) = u8::from_str_radix(&s[i + 2..i + 4], 16) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).trim().to_string()
}

fn udev(majmin: &str) -> BTreeMap<String, String> {
    read(format!("/run/udev/data/b{majmin}"))
        .lines()
        .filter_map(|l| l.strip_prefix("E:")?.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect()
}

/// Vendor and product names from the system's usb.ids, or else from udev's hwdb
/// copy of it (Debian ships only the hwdb). Empty when neither knows the ID.
fn usb_db_names(vid: &str, pid: &str, majmin: &str) -> (String, String) {
    static IDS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let ids = IDS.get_or_init(|| {
        ["/usr/share/hwdata/usb.ids", "/usr/share/misc/usb.ids", "/usr/share/usb.ids", "/var/lib/usbutils/usb.ids"].iter().find_map(|p| std::fs::read_to_string(p).ok()).unwrap_or_default()
    });
    if let Some(n) = crate::ids::usb_ids_lookup(ids, vid, pid) {
        return n;
    }
    let u: BTreeMap<String, String> = read(format!("/run/udev/data/c{majmin}")).lines().filter_map(|l| l.strip_prefix("E:")?.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))).collect();
    let get = |k: &str| u.get(k).cloned().unwrap_or_default();
    (get("ID_VENDOR_FROM_DATABASE"), get("ID_MODEL_FROM_DATABASE"))
}

/// The CID register of an SD or MMC card (sysfs of the mmc card device).
fn card_info(dev: &Path) -> Option<CardInfo> {
    let kind = read(dev.join("type"));
    if kind != "SD" && kind != "MMC" {
        return None;
    }
    let hex = |f: &str| u32::from_str_radix(read(dev.join(f)).trim_start_matches("0x"), 16).ok();
    let mid = hex("manfid")?;
    // SD cards carry two ASCII letters ("SM"); eMMC a one-byte OEM id.
    let oid = hex("oemid").unwrap_or(0);
    let letters = [(oid >> 8) as u8, oid as u8];
    let oem = if kind == "SD" && letters.iter().all(|c| c.is_ascii_graphic()) { String::from_utf8_lossy(&letters).to_string() } else { format!("0x{oid:02X}") };
    Some(CardInfo {
        maker: crate::ids::card_maker(kind == "SD", mid).unwrap_or("").to_string(),
        kind,
        mid,
        oem,
        name: read(dev.join("name")),
        date: read(dev.join("date")),
        serial: read(dev.join("serial")),
    })
}

fn usb_info(devpath: &Path) -> Option<UsbInfo> {
    // The USB device is the nearest ancestor with idVendor (interfaces don't have it).
    let dev = devpath.ancestors().find(|a| a.join("idVendor").exists())?;
    // Mbit/s; low speed is "1.5".
    let speed = read(dev.join("speed")).parse::<f64>().unwrap_or(0.0).ceil() as u64;
    let link = match speed {
        0..=2 => "USB 1.x Low Speed (1.5 Mbit/s)".to_string(),
        3..=12 => "USB 1.x Full Speed (12 Mbit/s)".to_string(),
        13..=480 => "USB 2.0 High Speed (480 Mbit/s)".to_string(),
        481..=5000 => "USB 3.x SuperSpeed (5 Gbit/s)".to_string(),
        s => format!("USB 3.x SuperSpeed+ ({} Gbit/s)", s / 1000),
    };
    let serial = Some(read(dev.join("serial"))).filter(|s| !s.is_empty());
    let port = dev.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let (vid, pid) = (read(dev.join("idVendor")), read(dev.join("idProduct")));
    let (db_vendor, db_product) = usb_db_names(&vid, &pid, &read(dev.join("dev")));
    Some(UsbInfo {
        vid: vid.to_ascii_uppercase(),
        pid: pid.to_ascii_uppercase(),
        usb_version: read(dev.join("version")),
        release: super::bcd(&read(dev.join("bcdDevice"))),
        db_vendor,
        db_product,
        manufacturer: read(dev.join("manufacturer")),
        product: read(dev.join("product")),
        // "500mA"
        max_power_ma: read(dev.join("bMaxPower")).trim_end_matches("mA").trim().parse().unwrap_or(0),
        link,
        superspeed_capable: None,
        superspeed_plus_capable: None,
        serial,
        slow_link: speed > 0 && speed <= 480,
        hub: port,
        port: 0,
    })
}

fn skip_disk(name: &str) -> bool {
    ["loop", "ram", "zram", "dm-", "md", "sr", "fd", "nbd", "zd", "mtdblock", "ubiblock"].iter().any(|p| name.starts_with(p)) || name.contains("boot") || name.contains("rpmb")
}

fn partitions(name: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(format!("/sys/block/{name}"))
        // Not only sda1 / mmcblk0p1: Amlogic kernels name eMMC partitions by label ("super", "data").
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|p| Path::new(&format!("/sys/block/{name}/{p}/partition")).exists()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// A volume: a partition, or the whole disk when it carries a file system directly.
fn volume(disk: &str, part: &str, ms: &[Mount]) -> VolumeInfo {
    let sys = if disk == part { format!("/sys/block/{disk}") } else { format!("/sys/block/{disk}/{part}") };
    let devnum = read(format!("{sys}/dev"));
    let mm = devnum.split_once(':').map(|(a, b)| (a.parse().unwrap_or(0), b.parse().unwrap_or(0))).unwrap_or((0, 0));
    let dev = format!("/dev/{part}");
    let start = if disk == part { 0 } else { num(format!("{sys}/start")) * 512 };
    let size = num(format!("{sys}/size")) * 512;
    let u = udev(&devnum);
    let mine: Vec<&Mount> = ms.iter().filter(|m| m.majmin == mm || m.source == dev).collect();
    let mut v = VolumeInfo {
        guid: dev,
        mount_points: mine.iter().map(|m| m.point.clone()).collect(),
        label: u.get("ID_FS_LABEL").cloned().unwrap_or_default(),
        fs: u.get("ID_FS_TYPE").cloned().or_else(|| mine.first().map(|m| m.fstype.clone())).or_else(|| sniff_fs(&format!("/dev/{part}"))).unwrap_or_default(),
        size,
        free: 0,
        extents: vec![(disk.to_string(), start, size)],
    };
    if let Some((total, free, _)) = v.mount_points.first().and_then(|p| statvfs(p)) {
        v.size = total.max(1).min(size.max(total));
        v.free = free;
    }
    v
}

fn disk_info(name: &str, ms: &[Mount]) -> Option<DiskInfo> {
    let sys = PathBuf::from(format!("/sys/block/{name}"));
    let size = num(sys.join("size")) * 512;
    if size == 0 {
        return None;
    }
    let devpath = std::fs::canonicalize(&sys).ok()?;
    let p = devpath.to_string_lossy().to_string();
    let devnum = read(sys.join("dev"));
    let u = udev(&devnum);
    let mut d = DiskInfo { id: name.to_string(), path: format!("/dev/{name}"), size, instance_id: p.clone(), ..Default::default() };
    d.ssd_hint = super::linux_ssd_hint(&read(sys.join("queue/rotational")), &read(sys.join("queue/discard_max_bytes")));
    d.sector_size = num(sys.join("queue/logical_block_size")).max(512) as u32;
    d.removable = read(sys.join("removable")) == "1";
    d.bus = if p.contains("/usb") {
        "USB".into()
    } else if name.starts_with("nvme") {
        "NVMe".into()
    } else if name.starts_with("mmcblk") {
        let t = read(sys.join("device/type"));
        if t.is_empty() { "MMC".into() } else { t }
    } else if p.contains("/ata") {
        "SATA".into()
    } else if p.contains("virtio") && !p.contains("/host") {
        // virtio-blk. Disks on virtio-scsi stay SCSI: passed-through drives answer SMART.
        "Virtual".into()
    } else {
        "SCSI".into()
    };
    d.vendor = read(sys.join("device/vendor"));
    d.product = [read(sys.join("device/model")), read(sys.join("device/name"))].into_iter().find(|s| !s.is_empty()).unwrap_or_default();
    d.revision = [read(sys.join("device/rev")), read(sys.join("device/firmware_rev")), read(sys.join("device/fwrev"))].into_iter().find(|s| !s.is_empty()).unwrap_or_default();
    d.serial = u.get("ID_SERIAL_SHORT").cloned().or_else(|| [read(sys.join("device/serial")), read(sys.join("serial"))].into_iter().find(|s| !s.is_empty())).unwrap_or_default();
    // udev knows the drive behind a SATA bridge (ata_id); the SCSI strings name the bridge.
    if let Some(m) = u.get("ID_MODEL_ENC").map(|m| udev_dec(m)).filter(|m| !m.is_empty()) {
        d.product = m;
        d.vendor = if u.get("ID_BUS").map(String::as_str) == Some("ata") { String::new() } else { u.get("ID_VENDOR_ENC").map(|v| udev_dec(v)).unwrap_or(d.vendor.clone()) };
    }
    // "0x1af4" on virtio is a PCI vendor id, not a name.
    if d.vendor == "ATA" || d.vendor.starts_with("0x") {
        d.vendor.clear();
    }
    if d.bus == "Virtual" && d.product.is_empty() {
        d.product = "Virtio disk".into();
    }
    if d.bus == "USB" {
        d.usb = usb_info(&devpath);
        // Without udev, take the USB serial as udev would (ID_SERIAL_SHORT).
        if d.serial.is_empty() {
            d.serial = d.usb.as_ref().and_then(|u| u.serial.clone()).unwrap_or_default();
        }
        d.controller = d.usb.as_ref().map(|u| crate::ids::usb_controller_hint(&u.vid, &u.pid)).unwrap_or_default();
    }
    if d.bus == "NVMe" {
        // The NVMe controller is the PCI function above the namespace (class 01:08).
        if let Some(pci) = devpath.ancestors().find(|a| read(a.join("class")).starts_with("0x0108")) {
            d.controller = crate::ids::nvme_controller(&read(pci.join("vendor")), &read(pci.join("device")));
        }
    }
    if name.starts_with("mmcblk") {
        d.card = card_info(&sys.join("device"));
    }
    d.placeholder_serial = super::suspicious_serial(&d);
    let parts = partitions(name);
    d.volumes = parts.iter().map(|pt| volume(name, pt, ms)).collect();
    // A file system straight on the disk: no partition table, or a hybrid ISO
    // mounted whole although it also carries partitions.
    let whole_mounted = ms.iter().any(|m| m.source == d.path);
    if whole_mounted || (parts.is_empty() && (u.contains_key("ID_FS_TYPE") || sniff_fs(&d.path).is_some())) {
        d.volumes.push(volume(name, name, ms));
    }
    Some(d)
}

pub fn list_disks() -> Vec<DiskInfo> {
    let ms = mounts();
    let prot = protected_disks();
    let mut names: Vec<String> = std::fs::read_dir("/sys/block").map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| !skip_disk(n)).collect()).unwrap_or_default();
    names.sort();
    let mut disks: Vec<DiskInfo> = names.iter().filter_map(|n| disk_info(n, &ms)).collect();
    super::mark_protected(&mut disks, &prot);
    disks
}

pub fn disk(id: &str) -> io::Result<DiskInfo> {
    super::find_disk(id, id.trim().trim_start_matches("/dev/"))
}

/// The volume (partition) holding `path`.
pub fn volume_of_path(path: &Path) -> io::Result<VolumeInfo> {
    let real = std::fs::canonicalize(path)?;
    let ms = mounts();
    let m = ms
        .iter()
        .filter(|m| real.starts_with(&m.point))
        .max_by_key(|m| m.point.len())
        .ok_or_else(|| io::Error::other("no mount found"))?;
    let mm = if m.majmin.0 != 0 { m.majmin } else { majmin_of_dev(&m.source).filter(|x| x.0 != 0).ok_or_else(|| io::Error::other(format!("{} ({}) is not on a block device", m.point, m.fstype)))? };
    let p = std::fs::canonicalize(format!("/sys/dev/block/{}:{}", mm.0, mm.1))?;
    let part = p.file_name().unwrap().to_string_lossy().to_string();
    let disk = if p.join("partition").exists() { p.parent().unwrap().file_name().unwrap().to_string_lossy().to_string() } else { part.clone() };
    if !Path::new(&format!("/sys/block/{disk}")).exists() || skip_disk(&disk) {
        return Err(io::Error::other(format!("{} is on {part}, not a plain disk partition (LVM, RAID or similar are not supported)", path.display())));
    }
    let mut v = volume(&disk, &part, &ms);
    if !v.mount_points.contains(&m.point) {
        v.mount_points.insert(0, m.point.clone());
    }
    // A multi-device btrfs writes to all its members: each must pass the guard.
    if m.fstype == "btrfs" {
        for d in disks_of_mount(m)? {
            if !v.extents.iter().any(|e| e.0 == d) {
                v.extents.push((d, 0, 0));
            }
        }
    }
    Ok(v)
}

/// The volume and disk behind a target folder, or a refusal message.
pub fn check_path(p: &Path) -> Result<(VolumeInfo, Option<DiskInfo>), String> {
    let v = volume_of_path(p).map_err(|e| format!("{}: cannot map to a disk ({e}); refusing", p.display()))?;
    super::refuse_protected(p, &v)?;
    let d = v.extents.first().and_then(|e| disk(&e.0).ok());
    Ok((v, d))
}

/// Raw access to a whole disk. For writing, the disk must not be mounted and is
/// opened exclusively (O_EXCL), so nothing else can hold it meanwhile.
pub struct RawDisk {
    pub file: File,
    pub size: u64,
    pub sector: u32,
    pub id: String,
    writable: bool,
    wrote: bool,
}

impl RawDisk {
    pub fn open_restore(d: &DiskInfo) -> io::Result<RawDisk> { Self::open(d, true) }

    pub fn open_backup(d: &DiskInfo) -> io::Result<RawDisk> {
        Self::open_mode(d, false, true)
    }

    pub fn open(d: &DiskInfo, write: bool) -> io::Result<RawDisk> {
        Self::open_mode(d, write, write)
    }

    fn open_mode(d: &DiskInfo, write: bool, exclusive: bool) -> io::Result<RawDisk> {
        let current = disk(&d.id)?;
        d.check_identity(&current).map_err(io::Error::other)?;
        if exclusive {
            // Re-check right before writing: device names can move when devices come and go.
            let now = check_disk(&d.id).map_err(io::Error::other)?;
            d.check_identity(&now).map_err(io::Error::other)?;
            let mounted: Vec<String> = now.volumes.iter().flat_map(|v| v.mount_points.iter().map(move |m| format!("{} on {m}", v.guid))).collect();
            if !mounted.is_empty() {
                return Err(io::Error::other(format!("disk {} has mounted file systems ({}); unmount them first", d.id, mounted.join(", "))));
            }
            in_use_check(&d.id)?;
        }
        let mut o = OpenOptions::new();
        o.read(true).write(write);
        direct(&mut o, write);
        if exclusive {
            o.custom_flags(libc::O_DIRECT | libc::O_NOFOLLOW | libc::O_EXCL | if write { libc::O_DSYNC } else { 0 });
        }
        let f = o.open(&d.path).map_err(|e| if e.raw_os_error() == Some(libc::EBUSY) { io::Error::other(format!("{} is in use (mounted, swap, RAID or held by another program)", d.path)) } else { e })?;
        let mut size = 0u64;
        if unsafe { libc::ioctl(f.as_raw_fd(), BLKGETSIZE64 as _, &mut size) } != 0 {
            size = d.size;
        }
        Ok(RawDisk { file: f, size, sector: d.sector_size.max(512), id: d.id.clone(), writable: write, wrote: false })
    }
}

impl Drop for RawDisk {
    fn drop(&mut self) {
        if self.wrote {
            unsafe { libc::ioctl(self.file.as_raw_fd(), BLKRRPART as _) };
        }
    }
}

impl Dev for RawDisk {
    fn sector_size(&self) -> usize { self.sector as usize }
    fn fork(&mut self) -> io::Result<Box<dyn Dev>> {
        self.wrote |= self.writable;
        Ok(Box::new(crate::dev::IoHandle { file: crate::dev::io_handle(&self.file, self.writable)?, size: self.size, alignment: self.align(), writable: self.writable }))
    }
    fn discard(&mut self, off: u64, len: u64) -> io::Result<()> {
        super::discard_range(self.writable, self.size, self.sector, off, len)?;
        self.wrote = true;
        let range = [off, len];
        // BLKDISCARD
        if unsafe { libc::ioctl(self.file.as_raw_fd(), 0x1277 as _, range.as_ptr()) } != 0 { return Err(super::discard_error(io::Error::last_os_error())); }
        Ok(())
    }
    fn len(&self) -> u64 {
        self.size
    }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "disk was opened read-only"));
        }
        self.wrote = true;
        pwrite_all(&self.file, buf, off)
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        let n = (buf.len() as u64).min(self.size.saturating_sub(off)) as usize;
        pread_full(&self.file, &mut buf[..n], off)
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.writable { self.file.sync_all() } else { Ok(()) }
    }
    fn align(&self) -> usize {
        (self.sector as usize).max(ALIGN)
    }
}

pub fn free_space_prepare() -> io::Result<()> {
    if !is_admin() {
        return Err(io::Error::other("the free-space probe needs root (it reads the raw disk to check the mapping)"));
    }
    // A large volume means many probe files open at once: lift the soft limit.
    let mut l = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut l) == 0 {
            l.rlim_cur = l.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &l);
        }
    }
    Ok(())
}

/// Allocate `len` bytes to `f` without writing them (fallocate) and return where
/// they are on the disk: (file offset, disk offset, length) runs from FIEMAP.
pub fn allocate_and_map(f: &File, len: u64, _root: &Path, vol: &VolumeInfo) -> io::Result<Vec<(u64, u64, u64)>> {
    let part_off = vol.extents.first().map(|e| e.1).ok_or_else(|| io::Error::other("volume has no disk extent"))?;
    if unsafe { libc::fallocate(f.as_raw_fd(), 0, 0, len as libc::off_t) } != 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EOPNOTSUPP) {
            return Err(io::Error::other(format!("{} cannot preallocate files; the free-space probe needs ext4, xfs or another file system with fallocate and FIEMAP", vol.fs)));
        }
        return Err(e);
    }
    const N: usize = 256;
    let mut out = vec![];
    let mut start = 0u64;
    loop {
        // struct fiemap (32 bytes) + N * struct fiemap_extent (56 bytes)
        let mut buf = vec![0u64; (32 + 56 * N) / 8];
        buf[0] = start;
        buf[1] = len - start;
        let w = buf.as_mut_ptr() as *mut u32;
        unsafe {
            *w.add(4) = 1; // FIEMAP_FLAG_SYNC
            *w.add(6) = N as u32; // fm_extent_count
        }
        if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_FIEMAP as _, buf.as_mut_ptr()) } != 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EOPNOTSUPP) {
                return Err(io::Error::other(format!("{} cannot report where a file's blocks are on the disk (FIEMAP); the free-space probe needs ext4, xfs or another file system with fallocate and FIEMAP", vol.fs)));
            }
            return Err(io::Error::other(format!("cannot map the file to disk blocks (FIEMAP): {e}")));
        }
        let mapped = unsafe { *w.add(5) } as usize;
        let mut last = false;
        for i in 0..mapped {
            let e = &buf[4 + i * 7..4 + i * 7 + 7];
            let (logical, physical, length) = (e[0], e[1], e[2]);
            let flags = (e[5] & 0xffff_ffff) as u32;
            // LAST=1; skip UNKNOWN 2, DELALLOC 4, ENCODED 8, NOT_ALIGNED 0x100, INLINE 0x200, TAIL 0x400
            if flags & (2 | 4 | 8 | 0x100 | 0x200 | 0x400) == 0 {
                out.push((logical, part_off + physical, length));
            }
            start = logical + length;
            last |= flags & 1 != 0;
        }
        if mapped == 0 || last || start >= len {
            return Ok(out);
        }
    }
}

/// One partition of `size_mib` (None = whole disk) formatted with `fs`, via
/// sfdisk and mkfs. The disk must not be mounted.
pub fn repartition(disk: &DiskInfo, size_mib: Option<u64>, fs: &str, label: &str) -> Result<String, String> {
    let now = super::disk(&disk.id).map_err(|e| e.to_string())?;
    if now.volumes.iter().any(|v| !v.mount_points.is_empty()) {
        return Err(format!("disk {} has mounted file systems; unmount them first", disk.id));
    }
    let gpt = disk.size > super::MBR_LIMIT;
    let ty = match (fs, gpt) {
        ("ext4", false) => "83",
        ("fat32", false) => "c",
        (_, false) => "7",
        ("ext4", true) => "0FC63DAF-8483-4772-8E79-3D69D8477DE4",
        (_, true) => "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7",
    };
    let size = size_mib.map(|m| format!("size={m}M, ")).unwrap_or_default();
    let script = format!("label: {}\nstart=1MiB, {size}type={ty}\n", if gpt { "gpt" } else { "dos" });
    let mkfs = match fs {
        "exfat" => "mkfs.exfat",
        "fat32" => "mkfs.vfat",
        "ntfs" => "mkfs.ntfs",
        _ => "mkfs.ext4",
    };
    // Check the tools first: a missing mkfs must not leave an empty partition behind.
    for c in ["sfdisk", mkfs] {
        if !std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(c).is_file())) {
            return Err(format!("{c} is not installed; the disk was not changed"));
        }
    }
    // exfat-utils' mkfs.exfat (mkexfatfs, FUSE exFAT) takes -n for the label; exfatprogs' takes -L.
    let exfat_label = if fs == "exfat" && run(mkfs, &["-V"], None).unwrap_or_else(|e| e).contains("mkexfatfs") { "-n" } else { "-L" };
    // Re-check right before each destructive step: device names can be reused.
    same_disk(disk)?;
    let mut log = run("sfdisk", &["--wipe", "always", "--wipe-partitions", "always", &disk.path], Some(&script))?;
    let _ = run("udevadm", &["settle"], None);
    let part = super::first_partition(&disk.path);
    for _ in 0..50 {
        if Path::new(&part).exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    same_disk(disk)?;
    let pname = part.trim_start_matches("/dev/");
    let parent = std::fs::canonicalize(format!("/sys/class/block/{pname}")).ok().and_then(|p| p.parent().map(|q| q.to_string_lossy().to_string()));
    if parent.as_deref() != Some(disk.instance_id.as_str()) {
        return Err(format!("{part} does not belong to the selected disk; not formatting it"));
    }
    let args: Vec<&str> = match fs {
        "exfat" => vec![exfat_label, label, &part],
        "fat32" => vec!["-F", "32", "-n", label, &part],
        "ntfs" => vec!["-f", "-L", label, &part],
        _ => vec!["-F", "-L", label, &part],
    };
    // Without udev (mdev on diskless Alpine) the partition node can vanish after it
    // first appeared: mdev handles the old "remove" event after devtmpfs made the
    // new node. Recreate it from sysfs and retry.
    for tries in 1.. {
        ensure_node(&part);
        match run(mkfs, &args, None) {
            Err(e) if (e.contains("No such file or directory") || e.contains("does not exist")) && tries < 10 => {
                std::thread::sleep(std::time::Duration::from_millis(500));
                same_disk(disk)?;
            }
            r => {
                log += &r?;
                break;
            }
        }
    }
    // mdev can delete the node again once mkfs is done: leave one to mount.
    std::thread::sleep(std::time::Duration::from_secs(1));
    ensure_node(&part);
    Ok(log)
}

/// Create a missing partition node from its sysfs device number.
fn ensure_node(part: &str) {
    if Path::new(part).exists() {
        return;
    }
    let dev = read(format!("/sys/class/block/{}/dev", part.trim_start_matches("/dev/")));
    let Some((Ok(a), Ok(b))) = dev.split_once(':').map(|(a, b)| (a.parse::<u32>(), b.parse::<u32>())) else { return };
    if let Ok(p) = std::ffi::CString::new(part) {
        unsafe { libc::mknod(p.as_ptr(), libc::S_IFBLK | 0o660, libc::makedev(a, b)) };
    }
}

/// Still the same device (sysfs path, size, serial) and not protected.
fn same_disk(d: &DiskInfo) -> Result<(), String> {
    let now = check_disk(&d.id)?;
    d.check_identity(&now)?;
    in_use_check(&d.id).map_err(|e| e.to_string())
}

/// Refuse a disk whose partitions (or itself) are held by device-mapper, md, bcache
/// or similar: a mounted LV or RAID can live on it without any mount of its own.
fn in_use_check(id: &str) -> io::Result<()> {
    let mut held = vec![];
    for dir in std::iter::once(format!("/sys/block/{id}")).chain(partitions(id).into_iter().map(|p| format!("/sys/block/{id}/{p}"))) {
        if let Ok(rd) = std::fs::read_dir(format!("{dir}/holders")) {
            held.extend(rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()));
        }
    }
    if held.is_empty() { Ok(()) } else { Err(io::Error::other(format!("disk {id} is in use by {} (LVM, RAID, crypt or cache); release it first", held.join(", ")))) }
}

// SMART pass-through (read-only commands only; see smart.rs). The device is opened
// read-only and not exclusively, so it works next to a running raw test.

/// struct sg_io_hdr from <scsi/sg.h>.
#[repr(C)]
struct SgIoHdr {
    interface_id: i32,
    dxfer_direction: i32,
    cmd_len: u8,
    mx_sb_len: u8,
    iovec_count: u16,
    dxfer_len: u32,
    dxferp: *mut libc::c_void,
    cmdp: *const u8,
    sbp: *mut u8,
    timeout: u32,
    flags: u32,
    pack_id: i32,
    usr_ptr: *mut libc::c_void,
    status: u8,
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8,
    host_status: u16,
    driver_status: u16,
    resid: i32,
    duration: u32,
    info: u32,
}

fn smart_open(d: &DiskInfo) -> io::Result<File> {
    OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(&d.path)
}

/// A SCSI command through SG_IO. SATA disks go this way too: libata translates SAT.
pub fn scsi(d: &DiskInfo, cdb: &[u8], dir: super::Dir) -> io::Result<Vec<u8>> {
    let (mut buf, direction) = match dir {
        super::Dir::In(n) => (vec![0u8; n], -3), // SG_DXFER_FROM_DEV
        super::Dir::Out(b) => (b.to_vec(), -2),  // SG_DXFER_TO_DEV
    };
    let mut sense = [0u8; 32];
    let mut h = SgIoHdr {
        interface_id: b'S' as i32,
        dxfer_direction: direction,
        cmd_len: cdb.len() as u8,
        mx_sb_len: sense.len() as u8,
        iovec_count: 0,
        dxfer_len: buf.len() as u32,
        dxferp: buf.as_mut_ptr() as *mut libc::c_void,
        cmdp: cdb.as_ptr(),
        sbp: sense.as_mut_ptr(),
        timeout: 10_000,
        flags: 0,
        pack_id: 0,
        usr_ptr: std::ptr::null_mut(),
        status: 0,
        masked_status: 0,
        msg_status: 0,
        sb_len_wr: 0,
        host_status: 0,
        driver_status: 0,
        resid: 0,
        duration: 0,
        info: 0,
    };
    let f = smart_open(d)?;
    if unsafe { libc::ioctl(f.as_raw_fd(), SG_IO as _, &mut h) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if h.status != 0 || h.host_status != 0 || h.driver_status & 0xF != 0 {
        return Err(io::Error::other(format!("SCSI status {:#04x}, sense {:x}/{:02x}/{:02x}", h.status, sense[2] & 0xF, sense[12], sense[13])));
    }
    if direction == -2 {
        return Ok(vec![]);
    }
    buf.truncate(buf.len().saturating_sub(h.resid.max(0) as usize));
    Ok(buf)
}

/// Native ATA: libata takes SAT, so it is the same as over USB.
pub fn ata(d: &DiskInfo, a: super::Ata) -> io::Result<Vec<u8>> {
    scsi(d, &crate::smart::sat_cdb(a), super::Dir::In(512))
}

/// An NVMe admin command with data in: NVME_IOCTL_ADMIN_CMD on the namespace device.
pub fn nvme(d: &DiskInfo, op: u8, nsid: u32, cdw10: u32, len: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    // struct nvme_passthru_cmd (72 bytes): opcode, flags, rsvd, nsid, cdw2, cdw3,
    // metadata, addr, metadata_len, data_len, cdw10..15, timeout_ms, result.
    let mut c = [0u8; 72];
    c[0] = op;
    c[4..8].copy_from_slice(&nsid.to_le_bytes());
    c[24..32].copy_from_slice(&(buf.as_mut_ptr() as u64).to_le_bytes());
    c[36..40].copy_from_slice(&(len as u32).to_le_bytes());
    c[40..44].copy_from_slice(&cdw10.to_le_bytes());
    c[64..68].copy_from_slice(&10_000u32.to_le_bytes());
    let f = smart_open(d)?;
    let r = unsafe { libc::ioctl(f.as_raw_fd(), NVME_IOCTL_ADMIN_CMD as _, c.as_mut_ptr()) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    if r > 0 {
        return Err(io::Error::other(format!("NVMe status {r:#x}")));
    }
    Ok(buf)
}
