//! Windows: disk and volume enumeration, drive identity, USB link speed, the
//! system-disk guard, raw disk access, elevation, free-space mapping, diskpart.

use super::{check_disk, DiskInfo, UsbInfo, VolumeInfo};
use crate::dev::{ALIGN, Dev, FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH, pread_full, pwrite_all};
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation as di;
use windows_sys::Win32::Storage::FileSystem as fs;
use windows_sys::core::GUID;

const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
const IOCTL_STORAGE_GET_DEVICE_NUMBER: u32 = 0x002D_1080;
const IOCTL_DISK_GET_DRIVE_GEOMETRY_EX: u32 = 0x0007_00A0;
const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007_405C;
const IOCTL_DISK_IS_WRITABLE: u32 = 0x0007_0024;
const IOCTL_DISK_UPDATE_PROPERTIES: u32 = 0x0007_0140;
const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;
const FSCTL_LOCK_VOLUME: u32 = 0x0009_0018;
const FSCTL_DISMOUNT_VOLUME: u32 = 0x0009_0020;
pub const FSCTL_GET_RETRIEVAL_POINTERS: u32 = 0x0009_0073;
pub const FSCTL_GET_RETRIEVAL_POINTER_BASE: u32 = 0x0009_0234;
const IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX: u32 = 0x0022_0448;
const IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX_V2: u32 = 0x0022_045C;
const IOCTL_USB_GET_DESCRIPTOR_FROM_NODE_CONNECTION: u32 = 0x0022_0410;

const GUID_DEVINTERFACE_DISK: GUID = GUID::from_u128(0x53f56307_b6bf_11d0_94f2_00a0c91efb8b);
const GUID_DEVINTERFACE_USB_HUB: GUID = GUID::from_u128(0xf18a0e88_c30c_11d0_8815_00a0c906bed8);

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn from_wide(b: &[u16]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf16_lossy(&b[..end])
}

fn multi_sz(b: &[u16]) -> Vec<String> {
    b.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
}

pub fn is_admin() -> bool {
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

fn open_dev(path: &str, access: u32) -> io::Result<File> {
    OpenOptions::new().access_mode(access).share_mode(3).open(path)
}

pub fn ioctl(f: &File, code: u32, inp: &[u8], out: &mut [u8]) -> io::Result<usize> {
    let mut ret = 0u32;
    let ok = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            f.as_raw_handle() as _,
            code,
            if inp.is_empty() { std::ptr::null() } else { inp.as_ptr() as *const c_void },
            inp.len() as u32,
            if out.is_empty() { std::ptr::null_mut() } else { out.as_mut_ptr() as *mut c_void },
            out.len() as u32,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(ret as usize) }
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn cstr_at(b: &[u8], o: usize) -> String {
    if o == 0 || o >= b.len() {
        return String::new();
    }
    let end = b[o..].iter().position(|&c| c == 0).map(|p| o + p).unwrap_or(b.len());
    // Some devices pad with control bytes; keep printable ASCII only.
    b[o..end].iter().filter(|c| (0x20..0x7f).contains(*c)).map(|&c| c as char).collect::<String>().trim().to_string()
}

fn bus_name(t: u32) -> &'static str {
    match t {
        1 => "SCSI",
        2 => "ATAPI",
        3 => "ATA",
        4 => "1394",
        6 => "Fibre",
        7 => "USB",
        8 => "RAID",
        9 => "iSCSI",
        10 => "SAS",
        11 => "SATA",
        12 => "SD",
        13 => "MMC",
        14 => "Virtual",
        15 => "File-backed virtual",
        16 => "Storage Spaces",
        17 => "NVMe",
        18 => "SCM",
        19 => "UFS",
        _ => "Unknown",
    }
}

/// Disk byte extents of a volume (`\\?\Volume{..}` without trailing slash, or a GLOBALROOT path).
fn extents(vol_path: &str) -> io::Result<Vec<(String, u64, u64)>> {
    let f = open_dev(vol_path, 0)?;
    let mut out = vec![0u8; 8 + 24 * 32];
    ioctl(&f, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], &mut out)?;
    let n = u32_at(&out, 0) as usize;
    Ok((0..n.min(32)).map(|i| (u32_at(&out, 8 + i * 24).to_string(), u64_at(&out, 16 + i * 24), u64_at(&out, 24 + i * 24))).collect())
}

pub fn list_volumes() -> Vec<VolumeInfo> {
    let mut out = vec![];
    let mut buf = [0u16; 512];
    let h = unsafe { fs::FindFirstVolumeW(buf.as_mut_ptr(), buf.len() as u32) };
    if h as isize == -1 {
        return out;
    }
    loop {
        out.push(volume_info(&from_wide(&buf)));
        if unsafe { fs::FindNextVolumeW(h, buf.as_mut_ptr(), buf.len() as u32) } == 0 {
            break;
        }
    }
    unsafe { fs::FindVolumeClose(h) };
    out
}

/// Details of a volume given its `\\?\Volume{...}\` name.
fn volume_info(guid: &str) -> VolumeInfo {
    let mut v = VolumeInfo { guid: guid.to_string(), ..Default::default() };
    let wg = wide(guid);
    let mut names = vec![0u16; 1024];
    let mut len = 0u32;
    if unsafe { fs::GetVolumePathNamesForVolumeNameW(wg.as_ptr(), names.as_mut_ptr(), names.len() as u32, &mut len) } != 0 {
        v.mount_points = multi_sz(&names[..len as usize]);
    }
    let mut label = [0u16; 261];
    let mut fsname = [0u16; 64];
    let (mut serial, mut maxc, mut flags) = (0u32, 0u32, 0u32);
    if unsafe { fs::GetVolumeInformationW(wg.as_ptr(), label.as_mut_ptr(), 261, &mut serial, &mut maxc, &mut flags, fsname.as_mut_ptr(), 64) } != 0 {
        v.label = from_wide(&label);
        v.fs = from_wide(&fsname);
    }
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    if unsafe { fs::GetDiskFreeSpaceExW(wg.as_ptr(), &mut avail, &mut total, &mut free) } != 0 {
        v.size = total;
        v.free = avail;
    }
    v.extents = extents(guid.trim_end_matches('\\')).unwrap_or_default();
    if v.size == 0 {
        v.size = v.extents.iter().map(|e| e.2).sum();
    }
    v
}

/// The volume containing `path`.
pub fn volume_of_path(path: &Path) -> io::Result<VolumeInfo> {
    let p = wide(&path.to_string_lossy());
    let mut mp = [0u16; 512];
    if unsafe { fs::GetVolumePathNameW(p.as_ptr(), mp.as_mut_ptr(), 512) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut guid = [0u16; 128];
    if unsafe { fs::GetVolumeNameForVolumeMountPointW(mp.as_ptr(), guid.as_mut_ptr(), 128) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(volume_info(&from_wide(&guid)))
}

/// Ok(None) if the value doesn't exist; Err if it exists but can't be read.
fn reg_value(key: &str, name: &str, multi: bool) -> io::Result<Option<Vec<String>>> {
    use windows_sys::Win32::System::Registry::*;
    let (k, n) = (wide(key), wide(name));
    let mut buf = vec![0u16; 4096];
    let mut size = (buf.len() * 2) as u32;
    let flags = if multi { RRF_RT_REG_MULTI_SZ } else { RRF_RT_REG_SZ };
    let r = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, k.as_ptr(), n.as_ptr(), flags, std::ptr::null_mut(), buf.as_mut_ptr() as *mut c_void, &mut size) };
    match r {
        0 => Ok(Some(multi_sz(&buf[..size as usize / 2]))),
        2 => Ok(None),
        e => Err(io::Error::from_raw_os_error(e as i32)),
    }
}

/// Disks that must never be tested, with the reason. Fails closed: an error
/// means the system disk could not be identified.
pub fn protected_disks() -> io::Result<BTreeMap<String, String>> {
    let mut m = BTreeMap::new();
    let mut add = |ext: &[(String, u64, u64)], why: &str| {
        for e in ext {
            m.entry(e.0.clone()).or_insert_with(|| why.to_string());
        }
    };
    let mut wd = [0u16; 260];
    let n = unsafe { windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW(wd.as_mut_ptr(), 260) };
    if n == 0 {
        return Err(io::Error::last_os_error());
    }
    let windir = from_wide(&wd);
    let v = volume_of_path(Path::new(&windir))?;
    if v.extents.is_empty() {
        return Err(io::Error::other("cannot map the Windows volume to a disk"));
    }
    add(&v.extents, &format!("system disk (holds {windir})"));
    // Boot partition and page files: failing to find or map them is an error, or the
    // boot (EFI) partition's disk would go unprotected.
    let sp = reg_value("SYSTEM\\Setup", "SystemPartition", false)?
        .and_then(|v| v.into_iter().next())
        .ok_or_else(|| io::Error::other("cannot find the boot partition (HKLM\\SYSTEM\\Setup\\SystemPartition)"))?;
    let e = extents(&format!("\\\\?\\GLOBALROOT{sp}")).map_err(|e| io::Error::other(format!("cannot map the boot partition {sp}: {e}")))?;
    add(&e, "holds the boot (system) partition");
    let mm = "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Memory Management";
    for pf in reg_value(mm, "ExistingPageFiles", true)?.unwrap_or_default() {
        // "\??\C:\pagefile.sys"
        let p = pf.trim_start_matches("\\??\\");
        let v = volume_of_path(Path::new(p))?;
        if v.extents.is_empty() {
            return Err(io::Error::other(format!("cannot map page file {p} to a disk")));
        }
        add(&v.extents, "holds a page file");
    }
    // A VHD-booted or Storage Spaces system sits on physical disks this list can't see.
    for n in m.keys() {
        let query_error = |e| io::Error::other(format!("cannot identify the system disk's bus (disk {n}): {e}"));
        let f = open_dev(&format!("\\\\.\\PhysicalDrive{n}"), 0).map_err(query_error)?;
        let mut desc = vec![0u8; 1024];
        let got = ioctl(&f, IOCTL_STORAGE_QUERY_PROPERTY, &[0u8; 12], &mut desc).map_err(query_error)?;
        let bus = protected_disk_bus(&desc[..got.min(desc.len())]).map_err(query_error)?;
        if matches!(bus, 14..=16) {
            return Err(io::Error::other(format!("the system disk (disk {n}) is a virtual disk or Storage Space; its physical disks can't be identified")));
        }
    }
    Ok(m)
}

fn protected_disk_bus(desc: &[u8]) -> io::Result<u32> {
    // STORAGE_DEVICE_DESCRIPTOR through RawPropertiesLength. A zero-filled or
    // truncated reply cannot establish that a protected disk has physical backing.
    if desc.len() < 36 || u32_at(desc, 0) < 36 || u32_at(desc, 4) < 36 || u32_at(desc, 4) as usize > desc.len() {
        return Err(io::Error::other("missing or truncated storage descriptor"));
    }
    let bus = u32_at(desc, 28);
    if !(1..=19).contains(&bus) {
        return Err(io::Error::other(format!("unknown storage bus type {bus}")));
    }
    Ok(bus)
}

fn cm_id(devinst: u32) -> String {
    let mut buf = [0u16; 512];
    if unsafe { di::CM_Get_Device_IDW(devinst, buf.as_mut_ptr(), 512, 0) } != 0 {
        return String::new();
    }
    from_wide(&buf)
}

fn cm_parent(devinst: u32) -> Option<u32> {
    let mut p = 0u32;
    (unsafe { di::CM_Get_Parent(&mut p, devinst, 0) } == 0).then_some(p)
}

/// Device interface paths (and devinst) of a class, optionally limited to one device instance id.
fn interfaces(guid: &GUID, instance: Option<&str>) -> Vec<(String, u32)> {
    let mut out = vec![];
    let inst = instance.map(wide);
    let h = unsafe {
        di::SetupDiGetClassDevsW(guid, inst.as_ref().map_or(std::ptr::null(), |w| w.as_ptr()), std::ptr::null_mut(), di::DIGCF_PRESENT | di::DIGCF_DEVICEINTERFACE)
    };
    if h == -1 {
        return out;
    }
    for i in 0.. {
        let mut ifd: di::SP_DEVICE_INTERFACE_DATA = unsafe { std::mem::zeroed() };
        ifd.cbSize = size_of::<di::SP_DEVICE_INTERFACE_DATA>() as u32;
        if unsafe { di::SetupDiEnumDeviceInterfaces(h, std::ptr::null(), guid, i, &mut ifd) } == 0 {
            break;
        }
        let mut buf = vec![0u32; 512];
        // SP_DEVICE_INTERFACE_DETAIL_DATA_W: cbSize is 8 on 64-bit
        buf[0] = if cfg!(target_pointer_width = "64") { 8 } else { 6 };
        let mut dev: di::SP_DEVINFO_DATA = unsafe { std::mem::zeroed() };
        dev.cbSize = size_of::<di::SP_DEVINFO_DATA>() as u32;
        let ok = unsafe { di::SetupDiGetDeviceInterfaceDetailW(h, &ifd, buf.as_mut_ptr() as *mut _, (buf.len() * 4) as u32, std::ptr::null_mut(), &mut dev) };
        if ok != 0 {
            let words = unsafe { std::slice::from_raw_parts((buf.as_ptr() as *const u16).add(2), buf.len() * 2 - 2) };
            out.push((from_wide(words), dev.DevInst));
        }
    }
    unsafe { di::SetupDiDestroyDeviceInfoList(h) };
    out
}

fn usb_speed_name(s: u8) -> &'static str {
    match s {
        0 => "USB 1.x Low Speed (1.5 Mbit/s)",
        1 => "USB 1.x Full Speed (12 Mbit/s)",
        2 => "USB 2.0 High Speed (480 Mbit/s)",
        3 => "USB 3.x SuperSpeed (5 Gbit/s)",
        _ => "unknown",
    }
}

/// Walk from a disk devnode up to its USB device and ask the hub about the link.
fn usb_info(disk_devinst: u32) -> Option<UsbInfo> {
    let mut d = disk_devinst;
    let mut usb = None;
    for _ in 0..8 {
        d = cm_parent(d)?;
        let id = cm_id(d);
        let up = id.to_ascii_uppercase();
        if up.starts_with("USB\\") && !up.contains("&MI_") {
            usb = Some((d, id));
            break;
        }
    }
    let (dev, id) = usb?;
    let up = id.to_ascii_uppercase();
    let field = |k: &str| up.find(k).map(|i| up[i + k.len()..].chars().take(4).collect::<String>());
    let serial = id.rsplit('\\').next().filter(|s| !s.contains('&')).map(|s| s.to_string());
    let mut info = UsbInfo { vid: field("VID_").unwrap_or_default(), pid: field("PID_").unwrap_or_default(), serial, link: "unknown".into(), ..Default::default() };

    let hub = cm_parent(dev)?;
    let mut port = 0u32;
    let mut sz = 4u32;
    let mut ty = 0u32;
    let r = unsafe { di::CM_Get_DevNode_Registry_PropertyW(dev, di::CM_DRP_ADDRESS, &mut ty, &mut port as *mut u32 as *mut c_void, &mut sz, 0) };
    let hub_path = interfaces(&GUID_DEVINTERFACE_USB_HUB, Some(&cm_id(hub))).into_iter().next();
    if r != 0 || hub_path.is_none() {
        return Some(info);
    }
    let hub_path = hub_path.unwrap().0;
    info.hub = hub_path.clone();
    info.port = port;
    let hf = open_dev(&hub_path, GENERIC_WRITE).or_else(|_| open_dev(&hub_path, 0));
    let Ok(hf) = hf else { return Some(info) };
    let mut b = vec![0u8; 512];
    b[0..4].copy_from_slice(&port.to_le_bytes());
    if ioctl(&hf, IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX, &b.clone(), &mut b).is_ok() {
        // packed struct: descriptor at 4 (bcdUSB at 6, idVendor 12, idProduct 14, bcdDevice 16), Speed at 23
        let bcd = u16::from_le_bytes([b[6], b[7]]);
        info.usb_version = format!("{:x}.{:02x}", bcd >> 8, bcd & 0xff);
        info.release = super::bcd(&format!("{:04x}", u16::from_le_bytes([b[16], b[17]])));
        info.vid = format!("{:04X}", u16::from_le_bytes([b[12], b[13]]));
        info.pid = format!("{:04X}", u16::from_le_bytes([b[14], b[15]]));
        let speed = b[23];
        info.link = usb_speed_name(speed).to_string();
        info.slow_link = speed < 3;
    }
    let mut v2 = [0u8; 16];
    v2[0..4].copy_from_slice(&port.to_le_bytes());
    v2[4..8].copy_from_slice(&16u32.to_le_bytes());
    v2[8..12].copy_from_slice(&7u32.to_le_bytes());
    let inp = v2;
    if ioctl(&hf, IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX_V2, &inp, &mut v2).is_ok() {
        // The legacy Speed field says "high" for SuperSpeed devices; these flags don't.
        let flags = u32_at(&v2, 12);
        info.superspeed_capable = Some(flags & 2 != 0);
        info.superspeed_plus_capable = Some(flags & 8 != 0);
        if flags & 4 != 0 {
            info.link = "USB 3.x SuperSpeed+ (10+ Gbit/s)".into();
            info.slow_link = false;
        } else if flags & 1 != 0 {
            info.link = usb_speed_name(3).to_string();
            info.slow_link = false;
        }
    }
    // String indexes from the device descriptor (iManufacturer at 14, iProduct at 15).
    let lang = usb_descriptor(&hf, port, 3, 0, 0, 255).filter(|s| s.len() >= 4).map_or(0x0409, |s| u16::from_le_bytes([s[2], s[3]]));
    let string = |i: u8| if i == 0 { String::new() } else { usb_descriptor(&hf, port, 3, i, lang, 255).map(|s| usb_string(&s)).unwrap_or_default() };
    info.manufacturer = string(b[18]);
    info.product = string(b[19]);
    // bMaxPower (configuration descriptor byte 8) counts 2 mA, or 8 mA at SuperSpeed.
    if let Some(c) = usb_descriptor(&hf, port, 2, 0, 0, 9).filter(|c| c.len() >= 9) {
        info.max_power_ma = c[8] as u32 * if info.link.contains("SuperSpeed") { 8 } else { 2 };
    }
    Some(info)
}

/// GET_DESCRIPTOR sent by the hub to the device on `port` (USB_DESCRIPTOR_REQUEST:
/// connection index, setup packet, then the data).
fn usb_descriptor(hub: &File, port: u32, kind: u8, index: u8, lang: u16, len: u16) -> Option<Vec<u8>> {
    let mut b = vec![0u8; 12 + len as usize];
    b[0..4].copy_from_slice(&port.to_le_bytes());
    b[4] = 0x80; // device to host, standard, device
    b[5] = 6; // GET_DESCRIPTOR
    b[6..8].copy_from_slice(&((kind as u16) << 8 | index as u16).to_le_bytes());
    b[8..10].copy_from_slice(&lang.to_le_bytes());
    b[10..12].copy_from_slice(&len.to_le_bytes());
    let inp = b.clone();
    let n = ioctl(hub, IOCTL_USB_GET_DESCRIPTOR_FROM_NODE_CONNECTION, &inp, &mut b).ok()?;
    (n > 12).then(|| b[12..n.min(b.len())].to_vec())
}

/// A string descriptor: length, type 3, then UTF-16LE text.
fn usb_string(d: &[u8]) -> String {
    let end = (d.first().copied().unwrap_or(0) as usize).min(d.len());
    let units: Vec<u16> = d.get(2..end).unwrap_or(&[]).chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units).trim().to_string()
}

/// The NVMe controller above a disk devnode, named by its PCI ID ("PCI\VEN_1987&DEV_5018&...").
fn nvme_pci(disk_devinst: u32) -> Option<String> {
    let mut d = disk_devinst;
    for _ in 0..4 {
        d = cm_parent(d)?;
        let id = cm_id(d).to_ascii_uppercase();
        if let Some(rest) = id.strip_prefix("PCI\\") {
            let field = |k: &str| rest.find(k).map(|i| rest[i + k.len()..].chars().take(4).collect::<String>());
            return Some(crate::ids::nvme_controller(&field("VEN_")?, &field("DEV_")?));
        }
    }
    None
}

fn disk_details(path: &str, devinst: u32) -> io::Result<DiskInfo> {
    let f = open_dev(path, 0)?;
    let mut num = [0u8; 12];
    ioctl(&f, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut num)?;
    let n = u32_at(&num, 4);
    let mut d = DiskInfo { id: n.to_string(), path: format!("\\\\.\\PhysicalDrive{n}"), instance_id: cm_id(devinst), ..Default::default() };
    let q = [0u8; 12]; // StorageDeviceProperty, PropertyStandardQuery
    let mut desc = vec![0u8; 1024];
    if ioctl(&f, IOCTL_STORAGE_QUERY_PROPERTY, &q, &mut desc).is_ok() {
        d.removable = desc[10] != 0;
        d.vendor = cstr_at(&desc, u32_at(&desc, 12) as usize);
        d.product = cstr_at(&desc, u32_at(&desc, 16) as usize);
        d.revision = cstr_at(&desc, u32_at(&desc, 20) as usize);
        d.serial = cstr_at(&desc, u32_at(&desc, 24) as usize);
        d.bus = bus_name(u32_at(&desc, 28)).to_string();
    }
    let mut g = [0u8; 256];
    if ioctl(&f, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut g).is_ok() {
        d.sector_size = u32_at(&g, 20);
        d.size = u64_at(&g, 24);
    }
    if d.bus == "USB" {
        d.usb = usb_info(devinst);
        d.controller = d.usb.as_ref().map(|u| crate::ids::usb_controller_hint(&u.vid, &u.pid)).unwrap_or_default();
    }
    if d.bus == "NVMe" {
        d.controller = nvme_pci(devinst).unwrap_or_default();
    }
    d.placeholder_serial = super::suspicious_serial(&d);
    if d.usb.as_ref().is_some_and(|u| u.serial.is_none()) && d.serial.is_empty() {
        d.placeholder_serial = Some("the device reports no serial number".into());
    }
    Ok(d)
}

/// All disks with volumes and protection status.
pub fn list_disks() -> Vec<DiskInfo> {
    let vols = list_volumes();
    let prot = protected_disks();
    let mut disks: Vec<DiskInfo> = interfaces(&GUID_DEVINTERFACE_DISK, None).iter().filter_map(|(p, inst)| disk_details(p, *inst).ok()).collect();
    disks.sort_by_key(|d| d.id.parse::<u32>().unwrap_or(u32::MAX));
    for d in &mut disks {
        d.volumes = vols.iter().filter(|v| v.extents.iter().any(|e| e.0 == d.id)).cloned().collect();
    }
    super::mark_protected(&mut disks, &prot);
    disks
}

pub fn disk(id: &str) -> io::Result<DiskInfo> {
    super::find_disk(id, id.trim().trim_start_matches(r"\\.\PhysicalDrive"))
}

/// The volume and disk behind a target folder, or a refusal message.
pub fn check_path(p: &Path) -> Result<(VolumeInfo, Option<DiskInfo>), String> {
    // Resolve junctions and links: the guard must judge where writes really land.
    let real = std::fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let v = volume_of_path(&real).map_err(|e| format!("{}: not a local volume ({e})", p.display()))?;
    let typed = volume_of_path(p).map_err(|e| format!("{}: not a local volume ({e})", p.display()))?;
    if typed.guid != v.guid {
        return Err(format!("{} points to another volume ({}); give that path directly.", p.display(), real.display()));
    }
    if v.extents.is_empty() {
        return Err(format!("{}: cannot map the volume to a disk, refusing (network share or unsupported volume?)", p.display()));
    }
    super::refuse_protected(p, &v)?;
    let d = if v.extents.len() == 1 { disk(&v.extents[0].0).ok() } else { None };
    Ok((v, d))
}

/// Raw access to a whole physical disk. When opened for writing, every volume on
/// the disk is locked and dismounted for as long as this lives.
pub struct RawDisk {
    pub file: File,
    pub size: u64,
    pub sector: u32,
    pub id: String,
    writable: bool,
    _locks: Vec<File>,
    wrote: bool,
}

impl RawDisk {
    /// Lock every source volume, then open only for reading. Unlike the write
    /// path, never force a dismount after a failed lock.
    pub fn open_backup(d: &DiskInfo) -> io::Result<RawDisk> {
        let now = check_disk(&d.id).map_err(io::Error::other)?;
        d.check_identity(&now).map_err(io::Error::other)?;
        let mut locks = vec![];
        for v in &now.volumes {
            let f = open_dev(v.guid.trim_end_matches('\\'), GENERIC_READ | GENERIC_WRITE)?;
            ioctl(&f, FSCTL_LOCK_VOLUME, &[], &mut []).map_err(|_| io::Error::other("Close programs using the source drive before creating an image."))?;
            locks.push(f);
        }
        let mut raw = Self::open_shared(d, false, 1)?;
        raw._locks = locks;
        Ok(raw)
    }

    pub fn open(d: &DiskInfo, write: bool) -> io::Result<RawDisk> {
        Self::open_shared(d, write, 3)
    }

    pub fn open_restore(d: &DiskInfo) -> io::Result<RawDisk> {
        Self::open_shared(d, true, 1)
    }

    fn open_shared(d: &DiskInfo, write: bool, share: u32) -> io::Result<RawDisk> {
        let mut locks = vec![];
        let current = disk(&d.id)?;
        d.check_identity(&current).map_err(io::Error::other)?;
        if write {
            // Re-check right before writing: disk numbers can move when devices come and go.
            let now = check_disk(&d.id).map_err(io::Error::other)?;
            d.check_identity(&now).map_err(io::Error::other)?;
            for v in &current.volumes {
                locks.push(lock_volume(v)?);
            }
        }
        let path = d.path.clone();
        let f = OpenOptions::new()
            .read(true)
            .write(write)
            .share_mode(share)
            .custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH)
            .open(&path)?;
        let mut len = [0u8; 8];
        ioctl(&f, IOCTL_DISK_GET_LENGTH_INFO, &[], &mut len)?;
        if write && ioctl(&f, IOCTL_DISK_IS_WRITABLE, &[], &mut []).is_err() {
            return Err(io::Error::other("the disk is write-protected"));
        }
        Ok(RawDisk { file: f, size: u64::from_le_bytes(len), sector: d.sector_size.max(512), id: d.id.clone(), writable: write, _locks: locks, wrote: false })
    }
}

fn lock_volume(v: &VolumeInfo) -> io::Result<File> {
    let path = v.guid.trim_end_matches('\\');
    let f = open_dev(path, GENERIC_READ | GENERIC_WRITE)?;
    let mut locked = false;
    for _ in 0..5 {
        if ioctl(&f, FSCTL_LOCK_VOLUME, &[], &mut []).is_ok() {
            locked = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    // Dismount even if the lock failed (forces open handles closed), then lock.
    ioctl(&f, FSCTL_DISMOUNT_VOLUME, &[], &mut []).ok();
    if !locked && ioctl(&f, FSCTL_LOCK_VOLUME, &[], &mut []).is_err() {
        let name = v.mount_points.first().cloned().unwrap_or_else(|| v.guid.clone());
        return Err(io::Error::other(format!("volume {name} is in use and cannot be locked; close programs using it")));
    }
    Ok(f)
}

impl Drop for RawDisk {
    fn drop(&mut self) {
        if self.wrote {
            ioctl(&self.file, IOCTL_DISK_UPDATE_PROPERTIES, &[], &mut []).ok();
        }
    }
}

impl Dev for RawDisk {
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

#[cfg(test)]
mod read_only_tests {
    use super::*;

    #[test]
    fn protected_bus_requires_a_complete_known_descriptor() {
        let mut descriptor = [0u8; 40];
        descriptor[0..4].copy_from_slice(&40u32.to_le_bytes());
        descriptor[4..8].copy_from_slice(&40u32.to_le_bytes());
        descriptor[28..32].copy_from_slice(&17u32.to_le_bytes());
        assert_eq!(protected_disk_bus(&descriptor).unwrap(), 17);
        for len in 0..descriptor.len() {
            assert!(protected_disk_bus(&descriptor[..len]).is_err());
        }
        for bus in [0u32, 20, u32::MAX] {
            descriptor[28..32].copy_from_slice(&bus.to_le_bytes());
            assert!(protected_disk_bus(&descriptor).is_err());
        }
        for bus in 14u32..=16 {
            descriptor[28..32].copy_from_slice(&bus.to_le_bytes());
            assert_eq!(protected_disk_bus(&descriptor).unwrap(), bus);
        }
        assert!(protected_disk_bus(&[0; 40]).is_err());
    }

    #[test]
    fn raw_read_handle_rejects_writes_without_marking_disk_changed() {
        // A scratch file, never a physical drive. Even a writable OS handle must
        // not bypass the RawDisk mode guard or trigger partition refresh on drop.
        let path = std::env::temp_dir().join(format!("flashtester-read-only-{}.bin", std::process::id()));
        let file = OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
        file.set_len(4096).unwrap();
        let mut raw = RawDisk { file, size: 4096, sector: 512, id: "fixture".into(), writable: false, _locks: vec![], wrote: false };
        let error = raw.write_at(0, &[42; 512]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!raw.wrote);
        raw.flush().unwrap();
        let mut data = [1; 512];
        assert_eq!(raw.read_at(0, &mut data).unwrap(), 512);
        assert_eq!(data, [0; 512]);
        drop(raw);
        std::fs::remove_file(path).unwrap();
    }
}

/// Enable a privilege (e.g. "SeManageVolumePrivilege") on the process token.
pub fn enable_privilege(name: &str) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, LUID};
    use windows_sys::Win32::Security::*;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut tok = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut tok) == 0 {
            return false;
        }
        let mut luid = LUID { LowPart: 0, HighPart: 0 };
        let n = wide(name);
        let mut ok = LookupPrivilegeValueW(std::ptr::null(), n.as_ptr(), &mut luid) != 0;
        if ok {
            let tp = TOKEN_PRIVILEGES { PrivilegeCount: 1, Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }] };
            ok = AdjustTokenPrivileges(tok, 0, &tp, 0, std::ptr::null_mut(), std::ptr::null_mut()) != 0
                && windows_sys::Win32::Foundation::GetLastError() == 0;
        }
        CloseHandle(tok);
        ok
    }
}

pub fn set_valid_data(f: &File, len: u64) -> io::Result<()> {
    if unsafe { fs::SetFileValidData(f.as_raw_handle() as _, len as i64) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Bytes per cluster and bytes per sector of the volume at `root` (e.g. `E:\`).
pub fn cluster_size(root: &str) -> io::Result<(u64, u64)> {
    let r = wide(root);
    let (mut spc, mut bps, mut fc, mut tc) = (0u32, 0u32, 0u32, 0u32);
    if unsafe { fs::GetDiskFreeSpaceW(r.as_ptr(), &mut spc, &mut bps, &mut fc, &mut tc) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((spc as u64 * bps as u64, bps as u64))
}

pub fn open_volume(v: &VolumeInfo) -> io::Result<File> {
    open_dev(v.guid.trim_end_matches('\\'), GENERIC_READ)
}

/// When run from Explorer with no console of its own, detach so the GUI has no console window.
pub fn hide_own_console() {
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    let mut ids = [0u32; 4];
    let n = unsafe { GetConsoleProcessList(ids.as_mut_ptr(), 4) };
    if n == 1 {
        unsafe { FreeConsole() };
    }
}

/// Where the GUI saves reports.
pub fn reports_dir() -> PathBuf {
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default().join("Documents").join("FlashTester")
}

/// Nothing to do on Windows: an elevated process writes as the same user.
pub fn give_to_user(_p: &Path) {}

/// A modal error message, for when there is no window to show it in.
pub fn error_box(title: &str, msg: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MessageBoxW};
    unsafe { MessageBoxW(std::ptr::null_mut(), wide(msg).as_ptr(), wide(title).as_ptr(), MB_ICONERROR) };
}

pub fn open_file(p: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SHELLEXECUTEINFOW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC};

    std::fs::metadata(p)?;
    let path = std::path::absolute(p)?;
    // Shell handlers can require STA. Keep this separate from the GUI's COM
    // apartment, and wait for the launch before releasing this thread's COM.
    std::thread::spawn(move || {
        let initialized = unsafe { CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) };
        if initialized < 0 {
            return Err(io::Error::other(format!("Cannot initialize the Windows shell (0x{:08X})", initialized as u32)));
        }
        let file: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let verb = wide("open");
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_FLAG_NO_UI | SEE_MASK_NOASYNC,
            lpVerb: verb.as_ptr(),
            lpFile: file.as_ptr(),
            nShow: 1,
            ..Default::default()
        };
        let result = if unsafe { ShellExecuteExW(&mut info) } != 0 { Ok(()) } else { Err(io::Error::last_os_error()) };
        unsafe { CoUninitialize() };
        result
    }).join().unwrap_or_else(|_| Err(io::Error::other("The Windows shell launcher stopped unexpectedly")))
}

/// Relaunch this exe elevated with the same arguments (UAC prompt).
pub fn relaunch_elevated(args: &str) -> bool {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let (verb, file, params) = (wide("runas"), wide(&exe), wide(args));
    let r = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), params.as_ptr(), std::ptr::null(), 1) };
    r as isize > 32
}

/// Cluster runs of a file: (VCN, LCN, cluster count), skipping unallocated runs.
pub fn retrieval_pointers(f: &File) -> io::Result<Vec<(u64, u64, u64)>> {
    let mut out = vec![];
    let mut vcn = 0u64;
    loop {
        let inp = vcn.to_le_bytes();
        let mut buf = vec![0u8; 16 + 16 * 512];
        let mut ret = 0u32;
        let ok = unsafe {
            windows_sys::Win32::System::IO::DeviceIoControl(
                f.as_raw_handle() as _,
                FSCTL_GET_RETRIEVAL_POINTERS,
                inp.as_ptr() as *const c_void,
                8,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        let more = ok == 0 && io::Error::last_os_error().raw_os_error() == Some(234);
        if ok == 0 && !more {
            return Err(io::Error::last_os_error());
        }
        let n = u32_at(&buf, 0) as usize;
        let mut start = u64_at(&buf, 8);
        for i in 0..n {
            let next = u64_at(&buf, 16 + i * 16);
            let lcn = u64_at(&buf, 24 + i * 16);
            if lcn as i64 >= 0 {
                out.push((start, lcn, next - start));
            }
            start = next;
        }
        vcn = start;
        if !more || n == 0 {
            return Ok(out);
        }
    }
}

/// Keeps Windows from sleeping while it lives (a test can run for hours). The
/// request belongs to the calling thread and is released on drop, including on
/// cancel, error or panic.
pub struct KeepAwake;

impl KeepAwake {
    pub fn new() -> KeepAwake {
        use windows_sys::Win32::System::Power::*;
        unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
        KeepAwake
    }
}

impl Default for KeepAwake {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Power::*;
        unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
    }
}

pub fn free_space(path: &Path) -> io::Result<u64> {
    let w = wide(&path.to_string_lossy());
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    if unsafe { fs::GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(avail)
}

/// Allocation unit of the volume holding `root`.
pub fn alloc_unit(root: &Path) -> io::Result<u64> {
    Ok(cluster_size(&root.to_string_lossy())?.0)
}

/// Free-space probe: allow allocating files without zero-filling them.
pub fn free_space_prepare() -> io::Result<()> {
    if enable_privilege("SeManageVolumePrivilege") { Ok(()) } else { Err(io::Error::other("free-space probe needs administrator rights (SeManageVolumePrivilege)")) }
}

/// Allocate `len` bytes to `f` without zero-filling and return where they are on
/// the disk: (file offset, disk offset, length) runs.
pub fn allocate_and_map(f: &File, len: u64, root: &Path, vol: &VolumeInfo) -> io::Result<Vec<(u64, u64, u64)>> {
    let (cluster, sector) = cluster_size(&root.to_string_lossy())?;
    let part_off = vol.extents.first().map(|e| e.1).ok_or_else(|| io::Error::other("volume has no disk extent"))?;
    let vh = open_volume(vol)?;
    let mut base = [0u8; 8];
    let base_bytes = match ioctl(&vh, FSCTL_GET_RETRIEVAL_POINTER_BASE, &[], &mut base) {
        Ok(_) => u64::from_le_bytes(base) * sector,
        Err(_) => 0,
    };
    f.set_len(len)?;
    set_valid_data(f, len).map_err(|e| io::Error::other(format!("cannot skip zero-filling ({e}); the free-space probe needs NTFS or exFAT")))?;
    Ok(retrieval_pointers(f)?.into_iter().map(|(vcn, lcn, n)| (vcn * cluster, part_off + base_bytes + lcn * cluster, n * cluster)).collect())
}

/// One partition of `size_mib` (None = whole disk) formatted with `fs`, via diskpart.
pub fn repartition(disk: &DiskInfo, size_mib: Option<u64>, fs: &str, label: &str) -> Result<String, String> {
    let size_arg = size_mib.map(|m| format!(" size={m}")).unwrap_or_default();
    let style = if disk.size > super::MBR_LIMIT { "gpt" } else { "mbr" };
    let script = format!("select disk {}\nclean\nconvert {style}\ncreate partition primary{size_arg}\nformat fs={fs} label=\"{label}\" quick\nassign\n", disk.id);
    let path = std::env::temp_dir().join(format!("flashtester-fix-{}.txt", std::process::id()));
    std::fs::write(&path, &script).map_err(|e| e.to_string())?;
    let out = std::process::Command::new("diskpart").arg("/s").arg(&path).output().map_err(|e| format!("cannot run diskpart: {e}"));
    let _ = std::fs::remove_file(&path);
    let out = out?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    // diskpart's text is localized; its exit code is not.
    if !out.status.success() {
        return Err(format!("diskpart failed:\n{text}"));
    }
    Ok(text)
}

/// Identity of the file system an open file lives on (volume serial number).
pub fn fs_id(f: &File) -> io::Result<u64> {
    let mut info: fs::BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { fs::GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.dwVolumeSerialNumber as u64)
}

/// Identity of the file system holding `p` (a folder or file).
pub fn fs_id_path(p: &Path) -> io::Result<u64> {
    // FILE_FLAG_BACKUP_SEMANTICS opens folders too.
    let f = OpenOptions::new().access_mode(0).share_mode(7).custom_flags(0x0200_0000).open(p)?;
    fs_id(&f)
}

// SMART pass-through (read-only commands only; see smart.rs). The buffered IOCTLs
// carry header and data in one buffer; the data starts 16-byte aligned. Offsets
// are the 64-bit structure layouts.

const IOCTL_SCSI_PASS_THROUGH: u32 = 0x0004_D004;
const IOCTL_ATA_PASS_THROUGH: u32 = 0x0004_D02C;
const _: () = assert!(std::mem::size_of::<usize>() == 8);

/// A SCSI command with data in or out: SCSI_PASS_THROUGH (56 bytes), 32 bytes of sense, data.
pub fn scsi(d: &DiskInfo, cdb: &[u8], dir: super::Dir) -> io::Result<Vec<u8>> {
    const HDR: usize = 56;
    const DATA: usize = 96;
    let (n, data_in) = match dir {
        super::Dir::In(n) => (n, 1u8),
        super::Dir::Out(b) => (b.len(), 0u8),
    };
    let mut b = vec![0u8; DATA + n];
    b[0..2].copy_from_slice(&(HDR as u16).to_le_bytes());
    b[6] = cdb.len() as u8;
    b[7] = 32;
    b[8] = data_in;
    b[12..16].copy_from_slice(&(n as u32).to_le_bytes());
    b[16..20].copy_from_slice(&10u32.to_le_bytes());
    b[24..32].copy_from_slice(&(DATA as u64).to_le_bytes());
    b[32..36].copy_from_slice(&(HDR as u32).to_le_bytes());
    b[36..36 + cdb.len()].copy_from_slice(cdb);
    if let super::Dir::Out(x) = dir {
        b[DATA..].copy_from_slice(x);
    }
    let f = open_dev(&d.path, GENERIC_READ | GENERIC_WRITE)?;
    let inp = b.clone();
    let back = ioctl(&f, IOCTL_SCSI_PASS_THROUGH, &inp, &mut b)?;
    if back < HDR {
        return Err(io::Error::other("short pass-through reply"));
    }
    if b[2] != 0 {
        let s = &b[HDR..HDR + 32];
        return Err(io::Error::other(format!("SCSI status {:#04x}, sense {:x}/{:02x}/{:02x}", b[2], s[2] & 0xF, s[12], s[13])));
    }
    // DataTransferLength comes back as what moved; smart.rs rejects short data.
    let got = (u32_at(&b, 12) as usize).min(n);
    Ok(if data_in == 1 { b[DATA..DATA + got].to_vec() } else { vec![] })
}

/// A native ATA command reading one sector: ATA_PASS_THROUGH_EX (48 bytes), then the data.
pub fn ata(d: &DiskInfo, a: super::Ata) -> io::Result<Vec<u8>> {
    const HDR: usize = 48;
    let mut b = vec![0u8; HDR + 512];
    b[0..2].copy_from_slice(&(HDR as u16).to_le_bytes());
    b[2..4].copy_from_slice(&3u16.to_le_bytes()); // ATA_FLAGS_DRDY_REQUIRED | ATA_FLAGS_DATA_IN
    b[8..12].copy_from_slice(&512u32.to_le_bytes());
    b[12..16].copy_from_slice(&10u32.to_le_bytes());
    b[24..32].copy_from_slice(&(HDR as u64).to_le_bytes());
    // CurrentTaskFile: features, count, LBA low, mid, high, device, command.
    b[40..47].copy_from_slice(&[a.features, 1, 0, a.lba_mid, a.lba_high, 0, a.command]);
    let f = open_dev(&d.path, GENERIC_READ | GENERIC_WRITE)?;
    let inp = b.clone();
    let back = ioctl(&f, IOCTL_ATA_PASS_THROUGH, &inp, &mut b)?;
    // The status register comes back in place of the command byte; bit 0 is ERR.
    if back < HDR || b[46] & 1 != 0 {
        return Err(io::Error::other(format!("ATA error {:#04x}", b[40])));
    }
    if back < HDR + 512 || u32_at(&b, 8) < 512 {
        return Err(io::Error::other("the drive returned less than a sector"));
    }
    Ok(b[HDR..].to_vec())
}

/// NVMe Identify (op 06h) or Get Log Page (op 02h). The inbox NVMe driver only
/// takes these through IOCTL_STORAGE_QUERY_PROPERTY (protocol-specific data).
pub fn nvme(d: &DiskInfo, op: u8, _nsid: u32, cdw10: u32, len: usize) -> io::Result<Vec<u8>> {
    let data_type = match op {
        0x06 => 1u32, // NVMeDataTypeIdentify, value = CNS
        0x02 => 2,    // NVMeDataTypeLogPage, value = log id
        _ => return Err(io::Error::from(io::ErrorKind::Unsupported)),
    };
    let f = open_dev(&d.path, GENERIC_READ | GENERIC_WRITE)?;
    let mut last = io::Error::from(io::ErrorKind::Unsupported);
    // StorageAdapterProtocolSpecificProperty, then StorageDeviceProtocolSpecificProperty.
    for prop in [49u32, 50] {
        // STORAGE_PROPERTY_QUERY, with STORAGE_PROTOCOL_SPECIFIC_DATA (40 bytes) at 8, data after it.
        let mut b = vec![0u8; 48 + len];
        b[0..4].copy_from_slice(&prop.to_le_bytes());
        b[8..12].copy_from_slice(&3u32.to_le_bytes()); // ProtocolTypeNvme
        b[12..16].copy_from_slice(&data_type.to_le_bytes());
        b[16..20].copy_from_slice(&(cdw10 & 0xFF).to_le_bytes());
        b[24..28].copy_from_slice(&40u32.to_le_bytes());
        b[28..32].copy_from_slice(&(len as u32).to_le_bytes());
        let inp = b.clone();
        match ioctl(&f, IOCTL_STORAGE_QUERY_PROPERTY, &inp, &mut b) {
            // Back comes STORAGE_PROTOCOL_DATA_DESCRIPTOR: the same protocol data at 8.
            Ok(back) => {
                let off = 8 + u32_at(&b, 24) as usize;
                if back < 48 || (u32_at(&b, 28) as usize) < len || off.checked_add(len).is_none_or(|e| e > back.min(b.len())) {
                    return Err(io::Error::other("short NVMe reply"));
                }
                return Ok(b[off..off + len].to_vec());
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}
