//! FreeBSD: disks, partitions and labels from GEOM's own
//! map (kern.geom.confxml); the system-disk guard; raw disk access; identity from
//! CAM and usbconfig; repartitioning with gpart and newfs. FreeBSD has no FIEMAP,
//! so there is no free-space probe.

use super::{check_disk, DiskInfo, UsbInfo, VolumeInfo, run, sniff_fs, statvfs};
use crate::dev::{ALIGN, Dev, direct, pread_full, pwrite_all};
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Command;

/// DIOCGMEDIASIZE: _IOR('d', 129, off_t).
const DIOCGMEDIASIZE: libc::c_ulong = 0x4008_6481;

pub fn is_admin() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// stdout of a base-system tool, None if it fails.
fn output(cmd: &str, args: &[&str]) -> Option<String> {
    let o = Command::new(cmd).args(args).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

#[derive(Default)]
struct Provider {
    name: String,
    geom: String,
    mode: String,
    size: u64,
    sector: u32,
    cfg: BTreeMap<String, String>,
}

#[derive(Default)]
struct Geom {
    class: String,
    /// Providers this geom's consumers attach to (what it sits on).
    consumes: Vec<String>,
}

/// GEOM's device graph: every layer (disk, partition table, label, encryption,
/// mirror, ...) is a geom that consumes providers below it and offers providers
/// above it. Keys are GEOM's ids.
#[derive(Default)]
struct Mesh {
    provs: HashMap<String, Provider>,
    geoms: HashMap<String, Geom>,
}

fn xml_text(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

impl Mesh {
    fn load() -> io::Result<Mesh> {
        let x = output("sysctl", &["-n", "kern.geom.confxml"]).ok_or_else(|| other("cannot read the GEOM configuration (sysctl kern.geom.confxml)"))?;
        Ok(Mesh::parse(&x))
    }

    /// The parts of the XML we use: each geom's class and what it consumes, each
    /// provider's name, size, access mode and config.
    fn parse(x: &str) -> Mesh {
        let mut m = Mesh::default();
        let (mut class, mut geom, mut prov) = (String::new(), String::new(), String::new());
        let mut stack: Vec<String> = vec![];
        let mut rest = x;
        while let Some(lt) = rest.find('<') {
            let text = &rest[..lt];
            let Some(gt) = rest[lt..].find('>') else { break };
            let tag = &rest[lt + 1..lt + gt];
            rest = &rest[lt + gt + 1..];
            if tag.starts_with('?') || tag.starts_with('!') {
                continue;
            }
            if let Some(name) = tag.strip_prefix('/') {
                stack.pop();
                let parent = stack.last().map_or("", String::as_str);
                let grand = if stack.len() >= 2 { stack[stack.len() - 2].as_str() } else { "" };
                // A vnode backing filename can contain leading/trailing whitespace.
                let t = xml_text(if (grand, parent, name) == ("provider", "config", "file") { text } else { text.trim() });
                match (grand, parent, name) {
                    (_, "class", "name") => class = t,
                    (_, "provider", f) if !prov.is_empty() => {
                        let p = m.provs.get_mut(&prov).unwrap();
                        match f {
                            "name" => p.name = t,
                            "mode" => p.mode = t,
                            "mediasize" => p.size = t.parse().unwrap_or(0),
                            "sectorsize" => p.sector = t.parse().unwrap_or(0),
                            _ => {}
                        }
                    }
                    ("provider", "config", f) if !prov.is_empty() => {
                        m.provs.get_mut(&prov).unwrap().cfg.insert(f.to_string(), t);
                    }
                    _ => {}
                }
                match name {
                    "provider" => prov.clear(),
                    "geom" => geom.clear(),
                    _ => {}
                }
                continue;
            }
            let closed = tag.ends_with('/');
            let tag = tag.trim_end_matches('/');
            let (tname, attrs) = tag.split_once(' ').unwrap_or((tag, ""));
            let attr = |k: &str| attrs.split_once(&format!("{k}=\"")).and_then(|(_, v)| v.split('"').next()).map(str::to_string);
            match (tname, attr("id"), attr("ref")) {
                ("geom", Some(id), _) => {
                    m.geoms.insert(id.clone(), Geom { class: class.clone(), consumes: vec![] });
                    geom = id;
                }
                ("provider", Some(id), _) => {
                    m.provs.insert(id.clone(), Provider { geom: geom.clone(), ..Default::default() });
                    prov = id;
                }
                ("provider", None, Some(r)) if stack.last().map(String::as_str) == Some("consumer") => {
                    if let Some(g) = m.geoms.get_mut(&geom) {
                        g.consumes.push(r);
                    }
                }
                _ => {}
            }
            if !closed {
                stack.push(tname.to_string());
            }
        }
        m
    }

    fn id(&self, name: &str) -> Option<&str> {
        self.provs.iter().find(|(_, p)| p.name == name).map(|(id, _)| id.as_str())
    }

    fn class(&self, id: &str) -> &str {
        self.provs.get(id).and_then(|p| self.geoms.get(&p.geom)).map_or("", |g| g.class.as_str())
    }

    fn consumes(&self, id: &str) -> &[String] {
        self.provs.get(id).and_then(|p| self.geoms.get(&p.geom)).map_or(&[], |g| g.consumes.as_slice())
    }

    /// Providers on top of `id`: the geoms consuming it and what they offer.
    fn above(&self, id: &str) -> Vec<&str> {
        self.provs.iter().filter(|(_, p)| self.geoms.get(&p.geom).is_some_and(|g| g.consumes.iter().any(|c| c == id))).map(|(k, _)| k.as_str()).collect()
    }

    /// The physical disks under provider `id`, through every layer. Fails closed on
    /// a layer whose backing can't be seen.
    fn disks(&self, id: &str, depth: u32) -> io::Result<Vec<String>> {
        let p = self.provs.get(id).ok_or_else(|| other("unknown GEOM provider"))?;
        if depth > 16 {
            return Err(other(format!("{}: GEOM layers nested too deep", p.name)));
        }
        match self.class(id) {
            "DISK" => Ok(vec![p.name.clone()]),
            "MD" => md_disks(self, &p.name, depth),
            "ZFS::ZVOL" => zpool_disks(self, p.name.trim_start_matches("zvol/").split('/').next().unwrap_or(""), depth),
            c => {
                if self.consumes(id).is_empty() {
                    return Err(other(format!("{} ({c}) sits on nothing that can be identified", p.name)));
                }
                let mut v = vec![];
                for c in self.consumes(id) {
                    v.extend(self.disks(c, depth + 1)?);
                }
                Ok(v)
            }
        }
    }

    /// Every provider under `id`, itself included.
    fn chain(&self, id: &str, out: &mut Vec<String>) {
        if out.iter().any(|x| x == id) || out.len() > 64 {
            return;
        }
        out.push(id.to_string());
        for c in self.consumes(id) {
            self.chain(c, out);
        }
    }

    /// Something holds the provider: an exclusive or write open (mounted, swap,
    /// ZFS, a mirror or another layer).
    fn busy(&self, id: &str) -> bool {
        let m = self.provs.get(id).map_or("", |p| p.mode.as_str());
        let count = |c: char| m.split(c).nth(1).and_then(|s| s.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<u32>().ok()).unwrap_or(0);
        count('w') > 0 || count('e') > 0
    }
}

/// What a memory disk sits on: a file (vnode) on some disk, or memory.
fn md_disks(mesh: &Mesh, name: &str, depth: u32) -> io::Result<Vec<String>> {
    let (kind, file) = md_backing(mesh, name)?;
    match kind {
        "vnode" => {
            let m = mount_of(Path::new(file)).map_err(|e| other(format!("{name}: its file {file}: {e}")))?;
            disks_of_mount(mesh, &m, depth + 1)
        }
        "malloc" | "swap" | "null" | "preload" => Ok(vec![]),
        _ => Err(other(format!("{name}: unknown memory disk"))),
    }
}

/// mdconfig itself gets these fields from GEOM. Use them directly so filename
/// spaces, quotes and tabs cannot be mistaken for command-output separators.
fn md_backing<'a>(mesh: &'a Mesh, name: &str) -> io::Result<(&'a str, &'a str)> {
    let id = mesh.id(name).ok_or_else(|| other(format!("{name}: unknown memory disk")))?;
    let p = &mesh.provs[id];
    let kind = p.cfg.get("type").map(String::as_str).ok_or_else(|| other(format!("{name}: unknown memory disk type")))?;
    let file = p.cfg.get("file").map_or("", String::as_str);
    if kind == "vnode" && !Path::new(file).is_absolute() {
        return Err(other(format!("{name}: missing or invalid backing file")));
    }
    Ok((kind, file))
}

/// The disks of a ZFS pool, from `zpool list -vHP` (device paths of its vdevs).
fn zpool_disks(mesh: &Mesh, pool: &str, depth: u32) -> io::Result<Vec<String>> {
    let list = output("zpool", &["list", "-vHP", pool]).ok_or_else(|| other(format!("cannot list the devices of ZFS pool {pool} (zpool)")))?;
    let devs: Vec<&str> = list.split_whitespace().filter_map(|w| w.strip_prefix("/dev/")).collect();
    if devs.is_empty() {
        return Err(other(format!("ZFS pool {pool} shows no devices")));
    }
    let mut out = vec![];
    for d in devs {
        let id = mesh.id(d).ok_or_else(|| other(format!("ZFS pool {pool}: device {d} is not in GEOM")))?;
        out.extend(mesh.disks(id, depth + 1)?);
    }
    Ok(out)
}

struct Mount {
    from: String,
    point: String,
    fstype: String,
}

fn cstr(b: &[libc::c_char]) -> String {
    let b: Vec<u8> = b.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&b).into_owned()
}

fn to_mount(s: &libc::statfs) -> Mount {
    Mount { from: cstr(&s.f_mntfromname), point: cstr(&s.f_mntonname), fstype: cstr(&s.f_fstypename) }
}

fn mounts() -> Vec<Mount> {
    let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if n <= 0 {
        return vec![];
    }
    let cap = n as usize + 16;
    let mut buf: Vec<libc::statfs> = Vec::with_capacity(cap);
    let n = unsafe { libc::getfsstat(buf.as_mut_ptr(), (cap * std::mem::size_of::<libc::statfs>()) as libc::c_long, libc::MNT_NOWAIT) };
    unsafe { buf.set_len(n.clamp(0, cap as i32) as usize) };
    buf.iter().map(to_mount).collect()
}

/// The mount holding `path`.
fn mount_of(path: &Path) -> io::Result<Mount> {
    let c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).map_err(|e| other(e.to_string()))?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(to_mount(&s))
}

const NO_DISK_FS: &[&str] = &["devfs", "tmpfs", "fdescfs", "procfs", "linprocfs", "linsysfs", "mqueuefs", "autofs"];

/// The physical disks behind a mount.
fn disks_of_mount(mesh: &Mesh, m: &Mount, depth: u32) -> io::Result<Vec<String>> {
    if depth > 8 {
        return Err(other(format!("{}: mounts nested too deep", m.point)));
    }
    if NO_DISK_FS.contains(&m.fstype.as_str()) {
        return Ok(vec![]);
    }
    if m.fstype == "zfs" {
        return zpool_disks(mesh, m.from.split('/').next().unwrap_or(""), depth);
    }
    if m.fstype == "nullfs" {
        let inner = mount_of(Path::new(&m.from))?;
        if inner.point == m.point {
            return Err(other(format!("{}: nullfs mount of itself", m.point)));
        }
        return disks_of_mount(mesh, &inner, depth + 1);
    }
    let dev = m.from.strip_prefix("/dev/").ok_or_else(|| other(format!("{} ({}) is not on a local disk", m.point, m.fstype)))?;
    let id = mesh.id(dev).ok_or_else(|| other(format!("{}: {} is not in GEOM", m.point, m.from)))?;
    mesh.disks(id, depth)
}

/// Every provider a mount uses (to tell which partition it is on).
fn mount_chain(mesh: &Mesh, m: &Mount) -> Vec<String> {
    let mut out = vec![];
    let devs: Vec<String> = if m.fstype == "zfs" {
        output("zpool", &["list", "-vHP", m.from.split('/').next().unwrap_or("")]).map(|l| l.split_whitespace().filter_map(|w| w.strip_prefix("/dev/")).map(str::to_string).collect()).unwrap_or_default()
    } else {
        m.from.strip_prefix("/dev/").map(|d| vec![d.to_string()]).unwrap_or_default()
    };
    for d in devs {
        if let Some(id) = mesh.id(&d) {
            mesh.chain(id, &mut out);
        }
    }
    out
}

/// Disks that must never be tested, with the reason. Fails closed: if the
/// running system's disks cannot be identified, this is an error.
pub fn protected_disks() -> io::Result<BTreeMap<String, String>> {
    let mesh = Mesh::load()?;
    protected(&mesh, &mounts())
}

fn protected(mesh: &Mesh, ms: &[Mount]) -> io::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    if !ms.iter().any(|m| m.point == "/") {
        return Err(other("no / mount"));
    }
    for point in ["/", "/boot", "/boot/efi", "/efi", "/usr", "/var"] {
        if let Some(m) = ms.iter().rev().find(|m| m.point == point) {
            for d in disks_of_mount(mesh, m, 0).map_err(|e| other(format!("cannot map {point} to a disk: {e}")))? {
                out.entry(d).or_insert_with(|| if point == "/" { "system disk (holds /)".to_string() } else { format!("holds {point}") });
            }
        }
    }
    // Swap devices are what the SWAP class consumes (files show up as memory disks).
    for g in mesh.geoms.values().filter(|g| g.class == "SWAP") {
        for c in &g.consumes {
            for d in mesh.disks(c, 0).map_err(|e| other(format!("cannot map swap to a disk: {e}")))? {
                out.entry(d).or_insert_with(|| "holds swap".to_string());
            }
        }
    }
    let dump = output("sysctl", &["-n", "kern.shutdown.dumpdevname"]).ok_or_else(|| other("cannot query crash dump devices (sysctl kern.shutdown.dumpdevname)"))?;
    for d in dump_disks(mesh, &dump)? {
        out.entry(d).or_insert_with(|| "crash dump device".to_string());
    }
    Ok(out)
}

fn dump_disks(mesh: &Mesh, dump: &str) -> io::Result<Vec<String>> {
    let dump = dump.trim();
    let mut out = vec![];
    if dump.is_empty() {
        return Ok(out);
    }
    // The kernel joins configured device names with commas; no devices is empty.
    for name in dump.split(',') {
        if name.is_empty() {
            return Err(other("empty name in crash dump device list"));
        }
        let id = mesh.id(name).ok_or_else(|| other(format!("crash dump device {name} is not in GEOM")))?;
        out.extend(mesh.disks(id, 0).map_err(|e| other(format!("cannot map crash dump device {name} to a disk: {e}")))?);
    }
    Ok(out)
}

/// CAM's view of a disk: controller (SIM), inquiry string, bus location.
struct Cam {
    sim: String,
    inquiry: String,
    location: String,
}

/// `camcontrol devlist -v`: "scbus2 on umass-sim0 bus 0:" headers, then
/// "<QEMU QEMU HARDDISK 2.5+>  at scbus2 target 0 lun 0 (da0,pass2)".
fn cam() -> HashMap<String, Cam> {
    let mut out = HashMap::new();
    let mut sim = String::new();
    for l in output("camcontrol", &["devlist", "-v"]).unwrap_or_default().lines() {
        if let Some(rest) = l.strip_prefix("scbus") {
            sim = rest.split_whitespace().nth(2).unwrap_or("").to_string();
            continue;
        }
        let (Some(a), Some(b)) = (l.find('<'), l.find('>')) else { continue };
        let inquiry = l[a + 1..b].trim().to_string();
        let location = l[b + 1..].split('(').next().unwrap_or("").trim().trim_start_matches("at ").to_string();
        let Some(names) = l.rsplit_once('(').map(|(_, n)| n.trim_end_matches(')')) else { continue };
        for n in names.split(',') {
            out.insert(n.trim().to_string(), Cam { sim: sim.clone(), inquiry: inquiry.clone(), location: format!("{sim} {location}") });
        }
    }
    out
}

/// "key=value key2="quoted value"" pairs of a newbus %pnpinfo / %location.
fn pairs(s: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut rest = s.trim();
    while let Some((k, v)) = rest.split_once('=') {
        let (val, next) = if let Some(q) = v.strip_prefix('"') { q.split_once('"').unwrap_or((q, "")) } else { v.split_once(' ').unwrap_or((v, "")) };
        out.insert(k.trim().to_string(), val.to_string());
        rest = next.trim();
    }
    out
}

/// USB identity of a umass disk: ids and serial from the umass device, link
/// speed and USB version from usbconfig.
fn usb_info(umass: &str) -> Option<UsbInfo> {
    let unit = umass.strip_prefix("umass-sim")?;
    let pnp = pairs(&output("sysctl", &["-n", &format!("dev.umass.{unit}.%pnpinfo")])?);
    let loc = pairs(&output("sysctl", &["-n", &format!("dev.umass.{unit}.%location")])?);
    let ugen = loc.get("ugen").cloned().unwrap_or_default();
    let line = output("usbconfig", &["-d", &ugen]).unwrap_or_default();
    // "ugen0.2: <...> at usbus0, cfg=0 md=HOST spd=SUPER (5.0Gbps) pwr=ON (0mA)"
    let spd = line.split("spd=").nth(1).and_then(|s| s.split_whitespace().next()).unwrap_or("");
    let link = match spd {
        "LOW" => "USB 1.x Low Speed (1.5 Mbit/s)",
        "FULL" => "USB 1.x Full Speed (12 Mbit/s)",
        "HIGH" => "USB 2.0 High Speed (480 Mbit/s)",
        "SUPER" => "USB 3.x SuperSpeed (5 Gbit/s)",
        "SUPER_PLUS" => "USB 3.x SuperSpeed+ (10 Gbit/s)",
        s => s,
    };
    let desc = output("usbconfig", &["-d", &ugen, "dump_device_desc"]).unwrap_or_default();
    let field = |k: &str| desc.lines().find_map(|l| l.trim().strip_prefix(k)).map(|h| h.split_whitespace().next().unwrap_or("").to_string()).unwrap_or_default();
    let bcd = field("bcdUSB = 0x");
    // "iManufacturer = 0x0001  <SMI Corporation>"
    let string = |k: &str| desc.lines().find_map(|l| l.trim().strip_prefix(k)).and_then(|r| Some(r.split_once('<')?.1.trim_end().trim_end_matches('>').trim().to_string())).filter(|s| s != "no string").unwrap_or_default();
    // "bMaxPower = 0x00fa": units of 2 mA, 8 mA at SuperSpeed.
    let cfg = output("usbconfig", &["-d", &ugen, "dump_curr_config_desc"]).unwrap_or_default();
    let power = cfg.lines().find_map(|l| l.trim().strip_prefix("bMaxPower = 0x")).and_then(|h| u32::from_str_radix(h.split_whitespace().next()?, 16).ok()).unwrap_or(0);
    let unit = if spd.starts_with("SUPER") { 8 } else { 2 };
    let hex = |k: &str| pnp.get(k).map(|v| v.trim_start_matches("0x").to_ascii_uppercase()).unwrap_or_default();
    Some(UsbInfo {
        vid: hex("vendor"),
        pid: hex("product"),
        usb_version: super::bcd(&bcd),
        release: super::bcd(&field("bcdDevice = 0x")),
        manufacturer: string("iManufacturer = 0x"),
        product: string("iProduct = 0x"),
        max_power_ma: power * unit,
        link: link.to_string(),
        superspeed_capable: None,
        superspeed_plus_capable: None,
        serial: pnp.get("sernum").cloned().filter(|s| !s.is_empty()),
        slow_link: matches!(spd, "LOW" | "FULL" | "HIGH"),
        hub: ugen,
        port: loc.get("port").and_then(|p| p.parse().ok()).unwrap_or(0),
        ..Default::default()
    })
}

fn bus(name: &str, cam: Option<&Cam>) -> String {
    let sim = cam.map_or("", |c| c.sim.as_str());
    let b = if sim.starts_with("umass") {
        "USB"
    } else if sim.starts_with("ahci") || sim.starts_with("ata") || name.starts_with("ada") {
        "SATA"
    } else if sim.starts_with("nvme") || name.starts_with("nvd") || name.starts_with("nda") {
        "NVMe"
    } else if name.starts_with("mmcsd") || name.starts_with("sdda") {
        "MMC"
    } else if name.starts_with("vtbd") {
        // virtio-blk. Disks on virtio-scsi (vtscsi) stay SCSI: passed-through drives answer SMART.
        "Virtual"
    } else if ["mpr", "mps", "mpt", "isp", "ahc", "ahd", "mrsas", "ciss"].iter().any(|p| sim.starts_with(p)) {
        "SAS"
    } else {
        "SCSI"
    };
    b.to_string()
}

/// Is the provider (or a fs label on it) a file system: (fs, label).
fn fs_label(mesh: &Mesh, id: &str) -> (String, String) {
    for a in mesh.above(id) {
        let n = &mesh.provs[a].name;
        if mesh.class(a) != "LABEL" {
            continue;
        }
        for (prefix, fs) in [("msdosfs/", "msdosfs"), ("ufs/", "ufs"), ("ufsid/", "ufs"), ("ext2fs/", "ext2fs"), ("ntfs/", "ntfs"), ("iso9660/", "cd9660")] {
            if let Some(l) = n.strip_prefix(prefix) {
                return (fs.to_string(), if prefix == "ufsid/" { String::new() } else { l.to_string() });
            }
        }
    }
    (String::new(), String::new())
}

/// A volume: a partition, or the whole disk when it carries a file system directly.
fn volume(mesh: &Mesh, disk: &str, id: &str, ms: &[Mount], chains: &[Vec<String>]) -> VolumeInfo {
    let p = &mesh.provs[id];
    let mine: Vec<&Mount> = ms.iter().zip(chains).filter(|(_, c)| c.iter().any(|x| x == id)).map(|(m, _)| m).collect();
    let (lfs, label) = fs_label(mesh, id);
    let part_fs = match p.cfg.get("type").map(String::as_str) {
        Some("freebsd-zfs") => "zfs",
        Some("freebsd-swap") => "swap",
        _ => "",
    };
    let dev = format!("/dev/{}", p.name);
    let fs = mine.first().map(|m| m.fstype.clone()).filter(|f| !f.is_empty()).or_else(|| Some(lfs).filter(|f| !f.is_empty())).or_else(|| Some(part_fs.to_string()).filter(|f| !f.is_empty())).or_else(|| sniff_fs(&dev)).unwrap_or_default();
    let offset = p.cfg.get("offset").and_then(|o| o.parse().ok()).unwrap_or(0);
    let mut v = VolumeInfo { guid: dev, mount_points: mine.iter().map(|m| m.point.clone()).collect(), label, fs, size: p.size, free: 0, extents: vec![(disk.to_string(), offset, p.size)] };
    if let Some((total, free, _)) = v.mount_points.first().and_then(|m| statvfs(m)) {
        v.size = total.max(1).min(p.size.max(total));
        v.free = free;
    }
    v
}

/// The partition providers of a disk (first level), in table order.
fn partitions(mesh: &Mesh, disk_id: &str) -> Vec<String> {
    let mut v: Vec<(u64, String)> = mesh.above(disk_id).into_iter().filter(|a| mesh.class(a) == "PART").map(|a| (mesh.provs[a].cfg.get("index").and_then(|i| i.parse().ok()).unwrap_or(0), a.to_string())).collect();
    v.sort();
    v.into_iter().map(|x| x.1).collect()
}

fn can_discard(path: &str) -> Option<bool> {
    // DIOCGATTR, GEOM::candelete. Read metadata only, independent of ATA passthrough.
    #[repr(C)]
    struct Attr { name: [u8; 64], len: i32, value: [u64; 32] }
    let mut a = Attr { name: [0; 64], len: 4, value: [0; 32] };
    a.name[..15].copy_from_slice(b"GEOM::candelete\0");
    let f = File::open(path).ok()?;
    let code = 0xc000_648eu64 | ((std::mem::size_of::<Attr>() as u64) << 16);
    if unsafe { libc::ioctl(f.as_raw_fd(), code as _, &mut a) } != 0 { return None; }
    match a.value[0] as u32 { 0 => Some(false), 1 => Some(true), _ => None }
}

fn disk_info(mesh: &Mesh, id: &str, cams: &HashMap<String, Cam>, ms: &[Mount], chains: &[Vec<String>]) -> DiskInfo {
    let p = &mesh.provs[id];
    let name = p.name.clone();
    let c = cams.get(&name);
    let mut d = DiskInfo { id: name.clone(), path: format!("/dev/{name}"), size: p.size, sector_size: p.sector.max(512), ..Default::default() };
    d.serial = p.cfg.get("ident").cloned().unwrap_or_default();
    d.product = p.cfg.get("descr").cloned().unwrap_or_default();
    d.bus = bus(&name, c);
    if let Some(c) = c {
        // The inquiry string is "vendor product revision"; GEOM's descr is the first two.
        d.revision = c.inquiry.strip_prefix(d.product.as_str()).map(|r| r.trim().to_string()).unwrap_or_default();
        if d.product.is_empty() {
            d.product = c.inquiry.clone();
        }
        d.instance_id = c.location.clone();
    } else {
        d.instance_id = format!("{name} {}", d.serial);
    }
    if d.bus == "Virtual" && d.product.is_empty() {
        d.product = "Virtio disk".into();
    }
    if d.bus == "USB" {
        let rotation = p.cfg.get("rotationrate").map_or("", String::as_str);
        let discard = can_discard(&d.path);
        d.ssd_hint = super::freebsd_ssd_hint(rotation, discard, "");
        if d.ssd_hint.is_none() {
            let identify = output("camcontrol", &["identify", &name]).unwrap_or_default();
            d.ssd_hint = super::freebsd_ssd_hint(rotation, discard, &identify);
        }
        d.usb = c.and_then(|c| usb_info(&c.sim));
        if let Some(u) = &d.usb {
            // The device address changes on every replug.
            d.instance_id = format!("{} {} port {}", d.instance_id, u.hub, u.port);
            if d.serial.is_empty() {
                d.serial = u.serial.clone().unwrap_or_default();
            }
        }
        d.removable = output("camcontrol", &["inquiry", &name, "-D"]).is_some_and(|s| s.contains("Removable"));
        d.controller = d.usb.as_ref().map(|u| crate::ids::usb_controller_hint(&u.vid, &u.pid)).unwrap_or_default();
    }
    if d.bus == "NVMe" {
        // nda sits on the CAM sim "nvme0"; nvd's newbus parent is the nvme device.
        let ctrl = c.map(|c| c.sim.clone()).filter(|s| s.starts_with("nvme")).or_else(|| {
            let unit = name.strip_prefix("nvd")?;
            output("sysctl", &["-n", &format!("dev.nvd.{unit}.%parent")])
        });
        if let Some(unit) = ctrl.as_deref().and_then(|c| c.trim().strip_prefix("nvme")) {
            let pnp = pairs(&output("sysctl", &["-n", &format!("dev.nvme.{unit}.%pnpinfo")]).unwrap_or_default());
            if let (Some(v), Some(dv)) = (pnp.get("vendor"), pnp.get("device")) {
                d.controller = crate::ids::nvme_controller(v, dv);
            }
        }
    }
    d.placeholder_serial = super::suspicious_serial(&d);
    let parts = partitions(mesh, id);
    d.volumes = parts.iter().map(|pt| volume(mesh, &name, pt, ms, chains)).collect();
    // A file system straight on the disk (no partition table).
    if parts.is_empty() && (chains.iter().any(|c| c.iter().any(|x| x == id)) || !fs_label(mesh, id).0.is_empty() || sniff_fs(&d.path).is_some()) {
        d.volumes.push(volume(mesh, &name, id, ms, chains));
    }
    d
}

fn list(mesh: &Mesh) -> Vec<DiskInfo> {
    let ms = mounts();
    let chains: Vec<Vec<String>> = ms.iter().map(|m| mount_chain(mesh, m)).collect();
    let prot = protected(mesh, &ms);
    let cams = cam();
    let mut ids: Vec<(&str, &str)> = mesh.provs.iter().filter(|(id, p)| mesh.class(id) == "DISK" && p.size > 0 && !p.name.starts_with("cd")).map(|(id, p)| (p.name.as_str(), id.as_str())).collect();
    ids.sort();
    let mut disks: Vec<DiskInfo> = ids.iter().map(|(_, id)| disk_info(mesh, id, &cams, &ms, &chains)).collect();
    super::mark_protected(&mut disks, &prot);
    disks
}

pub fn list_disks() -> Vec<DiskInfo> {
    match Mesh::load() {
        Ok(m) => list(&m),
        Err(_) => vec![],
    }
}

pub fn disk(id: &str) -> io::Result<DiskInfo> {
    super::find_disk(id, id.trim().trim_start_matches("/dev/"))
}

/// The volume (partition) holding `path`.
pub fn volume_of_path(path: &Path) -> io::Result<VolumeInfo> {
    let real = std::fs::canonicalize(path)?;
    let mut m = mount_of(&real)?;
    // A nullfs mount shows another mount's folder: test what lies underneath.
    for _ in 0..8 {
        if m.fstype != "nullfs" {
            break;
        }
        m = mount_of(Path::new(&m.from))?;
    }
    let mesh = Mesh::load()?;
    let disks = disks_of_mount(&mesh, &m, 0)?;
    let first = disks.first().cloned().ok_or_else(|| other(format!("{} ({}) is not on a disk", m.point, m.fstype)))?;
    let mut v = if m.fstype == "zfs" {
        // A pool writes to all its devices: each must pass the guard.
        let (size, free, _) = statvfs(&m.point).unwrap_or((0, 0, 0));
        VolumeInfo { guid: m.from.clone(), mount_points: vec![], label: String::new(), fs: m.fstype.clone(), size, free, extents: vec![] }
    } else {
        let dev = m.from.strip_prefix("/dev/").unwrap_or("");
        let id = mesh.id(dev).ok_or_else(|| other(format!("{} is not in GEOM", m.from)))?;
        let mut chain = vec![];
        mesh.chain(id, &mut chain);
        if let Some(c) = chain.iter().find(|c| !["PART", "LABEL", "DISK"].contains(&mesh.class(c))) {
            return Err(other(format!("{} is on {} ({}), not a plain disk partition (mirror, RAID, encryption or similar are not supported)", path.display(), mesh.provs[c.as_str()].name, mesh.class(c))));
        }
        // The first-level partition on the disk (or the disk itself).
        let disk_id = mesh.id(&first).unwrap_or("");
        let part = chain.iter().find(|c| mesh.consumes(c).iter().any(|x| x == disk_id) && mesh.class(c) == "PART").cloned().unwrap_or_else(|| disk_id.to_string());
        volume(&mesh, &first, &part, std::slice::from_ref(&m), &[chain])
    };
    for d in disks {
        if !v.extents.iter().any(|e| e.0 == d) {
            v.extents.push((d, 0, 0));
        }
    }
    if !v.mount_points.contains(&m.point) {
        v.mount_points.insert(0, m.point.clone());
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

/// Raw access to a whole disk. FreeBSD disk devices are unbuffered already, and
/// GEOM refuses write access while anything on the disk is mounted or held.
pub struct RawDisk {
    pub file: File,
    pub size: u64,
    pub sector: u32,
    pub id: String,
    writable: bool,
}

impl RawDisk {
    /// A second handle for a worker thread, on the descriptor this one already holds.
    pub fn worker(&self, write: bool) -> io::Result<File> {
        crate::dev::io_handle(&self.file, write)
    }

    pub fn open_restore(d: &DiskInfo) -> io::Result<RawDisk> { Self::open(d, true) }

    pub fn open_backup(d: &DiskInfo) -> io::Result<RawDisk> {
        same_disk(d).map_err(other)?;
        if disk(&d.id)?.volumes.iter().any(|v| !v.mount_points.is_empty()) {
            return Err(other("Unmount all source volumes before creating a disk image."));
        }
        // GEOM does not implement O_EXCL for a read-only device handle.
        // A writer claim prevents filesystem mounts; the returned Dev is made
        // read-only so the backup engine cannot issue writes through it.
        let mut raw = Self::open(d, true)?;
        raw.writable = false;
        Ok(raw)
    }

    pub fn open(d: &DiskInfo, write: bool) -> io::Result<RawDisk> {
        let current = disk(&d.id)?;
        d.check_identity(&current).map_err(io::Error::other)?;
        if write {
            let mounted: Vec<String> = disk(&d.id)?.volumes.iter().flat_map(|v| v.mount_points.iter().map(move |m| format!("{} on {m}", v.guid))).collect();
            if !mounted.is_empty() {
                return Err(other(format!("disk {} has mounted file systems ({}); unmount them first", d.id, mounted.join(", "))));
            }
            same_disk(d).map_err(other)?;
        }
        let mut o = OpenOptions::new();
        o.read(true).write(write);
        direct(&mut o, write);
        let f = o.open(&d.path).map_err(|e| if write && e.raw_os_error() == Some(libc::EPERM) { other(format!("{} is in use (mounted, swap, ZFS or another GEOM layer)", d.path)) } else { e })?;
        let mut size: libc::off_t = 0;
        if unsafe { libc::ioctl(f.as_raw_fd(), DIOCGMEDIASIZE, &mut size) } != 0 || size <= 0 {
            size = d.size as libc::off_t;
        }
        Ok(RawDisk { file: f, size: size as u64, sector: d.sector_size.max(512), id: d.id.clone(), writable: write })
    }
}

impl Dev for RawDisk {
    fn sector_size(&self) -> usize { self.sector as usize }
    fn fork(&mut self) -> io::Result<Box<dyn Dev>> {
        Ok(Box::new(crate::dev::IoHandle { file: self.worker(self.writable)?, size: self.size, alignment: self.align(), writable: self.writable }))
    }
    fn discard(&mut self, off: u64, len: u64) -> io::Result<()> {
        super::discard_range(self.writable, self.size, self.sector, off, len)?;
        let range = [off, len];
        // DIOCGDELETE: _IOW('d', 136, off_t[2])
        if unsafe { libc::ioctl(self.file.as_raw_fd(), 0x8010_6488u64 as _, range.as_ptr()) } != 0 { return Err(super::discard_error(io::Error::last_os_error())); }
        Ok(())
    }
    fn len(&self) -> u64 {
        self.size
    }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "disk was opened read-only"));
        }
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

const NO_FREE_SPACE_PROBE: &str = "the free-space probe is not available on FreeBSD (it cannot tell where a file's blocks are on the disk); use the raw probe on an empty disk, or a full test";

pub fn free_space_prepare() -> io::Result<()> {
    Err(other(NO_FREE_SPACE_PROBE))
}

pub fn allocate_and_map(_f: &File, _len: u64, _root: &Path, _vol: &VolumeInfo) -> io::Result<Vec<(u64, u64, u64)>> {
    Err(other(NO_FREE_SPACE_PROBE))
}

/// One partition of `size_mib` (None = whole disk) formatted with `fs` (fat32 or
/// ufs), via gpart and newfs. The disk must not be mounted.
pub fn repartition(disk: &DiskInfo, size_mib: Option<u64>, fs: &str, label: &str) -> Result<String, String> {
    let now = super::disk(&disk.id).map_err(|e| e.to_string())?;
    if now.volumes.iter().any(|v| !v.mount_points.is_empty()) {
        return Err(format!("disk {} has mounted file systems; unmount them first", disk.id));
    }
    let gpt = fs == "ufs" || disk.size > super::MBR_LIMIT;
    let ty = match (fs, gpt) {
        ("ufs", _) => "freebsd-ufs",
        (_, false) => "fat32lba",
        (_, true) => "ms-basic-data",
    };
    // Re-check right before each destructive step: device names can be reused.
    same_disk(disk)?;
    let mut log = String::new();
    // Drop any old partition table; this fails harmlessly when there is none.
    if let Ok(o) = run("gpart", &["destroy", "-F", &disk.id], None) {
        log += &o;
    }
    same_disk(disk)?;
    log += &run("gpart", &["create", "-s", if gpt { "GPT" } else { "MBR" }, &disk.id], None)?;
    let size = size_mib.map(|m| format!("{m}M"));
    let mut args = vec!["add", "-t", ty, "-a", "1M"];
    if let Some(s) = &size {
        args.extend(["-s", s]);
    }
    args.push(&disk.id);
    let added = run("gpart", &args, None)?;
    log += &added;
    // "da0s1 added"
    let part = added.split_whitespace().next().unwrap_or("").to_string();
    same_disk(disk)?;
    let mesh = Mesh::load().map_err(|e| e.to_string())?;
    let disk_id = mesh.id(&disk.id).unwrap_or("");
    if !mesh.id(&part).is_some_and(|p| mesh.consumes(p).iter().any(|c| c == disk_id)) {
        return Err(format!("{part} does not belong to the selected disk; not formatting it"));
    }
    let dev = format!("/dev/{part}");
    // newfs_msdos's own cluster size can leave too few clusters for FAT32 (fewer than
    // 65525 on a 2 GB stick): the largest of 512 B..32 KB that keeps enough.
    let bytes = mesh.id(&part).map_or(0, |p| mesh.provs[p].size);
    let spc = [64u64, 32, 16, 8, 4, 2, 1].into_iter().find(|s| bytes / (s * 512) >= 70_000).unwrap_or(1).to_string();
    log += &match fs {
        "ufs" => run("newfs", &["-U", "-L", &label.replace(' ', "_"), &dev], None)?,
        _ => run("newfs_msdos", &["-F", "32", "-c", &spc, "-L", label, &dev], None)?,
    };
    Ok(log)
}

/// Still the same device (location, size, serial), not protected, and nothing
/// on it is open for writing (mounted, swap, ZFS, another GEOM layer).
fn same_disk(d: &DiskInfo) -> Result<(), String> {
    let now = check_disk(&d.id)?;
    d.check_identity(&now)?;
    let mesh = Mesh::load().map_err(|e| e.to_string())?;
    let id = mesh.id(&d.id).ok_or_else(|| format!("disk {} is gone", d.id))?;
    let mut held = vec![];
    let mut todo = vec![id.to_string()];
    while let Some(p) = todo.pop() {
        if mesh.busy(&p) {
            held.push(mesh.provs[p.as_str()].name.clone());
        }
        todo.extend(mesh.above(&p).into_iter().map(str::to_string));
    }
    if held.is_empty() { Ok(()) } else { Err(format!("disk {} is in use ({} open: mounted, swap, ZFS or another GEOM layer); release it first", d.id, held.join(", "))) }
}

// SMART pass-through (read-only commands only; see smart.rs). ATA and SCSI go
// through `camcontrol cmd`, which moves the data over stdin/stdout ("-"); NVMe
// through the controller's pass-through ioctl.

fn camcmd(d: &DiskInfo, args: &[&str], input: Option<&[u8]>) -> io::Result<Vec<u8>> {
    use std::io::Write;
    use std::process::Stdio;
    let mut c = Command::new("camcontrol");
    c.args(["cmd", &d.id, "-t", "10"]).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn()?;
    if let Some(i) = input {
        child.stdin.take().unwrap().write_all(i)?;
    }
    drop(child.stdin.take());
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(other(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(out.stdout)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

pub fn scsi(d: &DiskInfo, cdb: &[u8], dir: super::Dir) -> io::Result<Vec<u8>> {
    let cdb = hex(cdb);
    match dir {
        super::Dir::In(n) => camcmd(d, &["-c", &cdb, "-i", &n.to_string(), "-"], None),
        super::Dir::Out(b) => camcmd(d, &["-c", &cdb, "-o", &b.len().to_string(), "-"], Some(b)).map(|_| vec![]),
    }
}

/// Native ATA (ada disks). camcontrol's 12 bytes: command, features, LBA low, mid,
/// high, device, LBA low/mid/high (exp), features (exp), sector count, count (exp).
pub fn ata(d: &DiskInfo, a: super::Ata) -> io::Result<Vec<u8>> {
    let regs = hex(&[a.command, a.features, 0, a.lba_mid, a.lba_high, 0, 0, 0, 0, 0, 1, 0]);
    camcmd(d, &["-a", &regs, "-i", "512", "-"], None)
}

/// The NVMe controller device (/dev/nvmeN) behind an nda or nvd disk: nda sits on
/// the controller's CAM bus; for nvd, the controller whose serial matches.
fn nvme_ctrl(d: &DiskInfo) -> io::Result<File> {
    let open = |p: &str| OpenOptions::new().read(true).write(true).open(p);
    if let Some(sim) = d.instance_id.split_whitespace().next().filter(|s| s.starts_with("nvme")) {
        return open(&format!("/dev/{sim}"));
    }
    let ctrls: Vec<String> = std::fs::read_dir("/dev")?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.strip_prefix("nvme").is_some_and(|u| !u.is_empty() && u.chars().all(|c| c.is_ascii_digit())))
        .collect();
    for c in &ctrls {
        let Ok(f) = open(&format!("/dev/{c}")) else { continue };
        let serial = nvme_pt(&f, 0x06, 0, 1, 4096).map(|id| String::from_utf8_lossy(&id[4..24]).trim().to_string()).unwrap_or_default();
        if ctrls.len() == 1 || (!serial.is_empty() && serial == d.serial.trim()) {
            return Ok(f);
        }
    }
    Err(other(format!("no NVMe controller found for {}", d.id)))
}

/// NVME_PASSTHROUGH_CMD with struct nvme_pt_command (104 bytes): the 64-byte
/// command, the 16-byte completion, buffer pointer, length, is_read, lock pointer.
fn nvme_pt(f: &File, op: u8, nsid: u32, cdw10: u32, len: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let mut pt = [0u64; 13];
    let b = unsafe { std::slice::from_raw_parts_mut(pt.as_mut_ptr() as *mut u8, 104) };
    b[0] = op;
    b[4..8].copy_from_slice(&nsid.to_le_bytes());
    b[40..44].copy_from_slice(&cdw10.to_le_bytes());
    b[80..88].copy_from_slice(&(buf.as_mut_ptr() as u64).to_le_bytes());
    b[88..92].copy_from_slice(&(len as u32).to_le_bytes());
    b[92..96].copy_from_slice(&1u32.to_le_bytes());
    if unsafe { libc::ioctl(f.as_raw_fd(), 0xC068_6E00u32 as _, pt.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let b = unsafe { std::slice::from_raw_parts(pt.as_ptr() as *const u8, 104) };
    let status = u16::from_le_bytes([b[78], b[79]]) & 0xFFFE;
    if status != 0 {
        return Err(other(format!("NVMe status {:#x}", status >> 1)));
    }
    Ok(buf)
}

pub fn nvme(d: &DiskInfo, op: u8, nsid: u32, cdw10: u32, len: usize) -> io::Result<Vec<u8>> {
    nvme_pt(&nvme_ctrl(d)?, op, nsid, cdw10, len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_disk_paths_preserve_spaces_and_literal_quotes() {
        let mesh = Mesh::parse("<mesh><class><name>MD</name><geom id=\"g\"><provider id=\"p\"><name>md0</name><config><type>vnode</type><file>/var/images/my &quot;quoted&quot; &amp; disk\twith newline\n.img </file></config></provider></geom></class></mesh>");
        assert_eq!(md_backing(&mesh, "md0").unwrap(), ("vnode", "/var/images/my \"quoted\" & disk\twith newline\n.img "));
        assert!(md_backing(&mesh, "md1").is_err());
    }

    #[test]
    fn incomplete_memory_disk_config_is_refused() {
        for config in [
            "<type>vnode</type>",
            "<type>vnode</type><file></file>",
            "<type>vnode</type><file>relative.img</file>",
            "<file>/var/image</file>",
        ] {
            let mesh = Mesh::parse(&format!("<mesh><class><name>MD</name><geom id=\"g\"><provider id=\"p\"><name>md0</name><config>{config}</config></provider></geom></class></mesh>"));
            assert!(md_backing(&mesh, "md0").is_err(), "accepted {config:?}");
        }
        let mesh = Mesh::parse("<mesh><class><name>MD</name><geom id=\"g\"><provider id=\"p\"><name>md0</name><config><type>swap</type></config></provider></geom></class></mesh>");
        assert_eq!(md_backing(&mesh, "md0").unwrap(), ("swap", ""));
    }

    const XML: &str = r#"<mesh>
  <class id="0x1"><name>DISK</name>
    <geom id="0x10"><class ref="0x1"/><name>da0</name><rank>1</rank><config></config>
      <provider id="0x11"><geom ref="0x10"/><mode>r1w1e2</mode><name>da0</name><mediasize>2147483648</mediasize><sectorsize>512</sectorsize>
        <config><ident>FT0001</ident><descr>QEMU &amp; Co HARDDISK</descr></config></provider>
    </geom>
  </class>
  <class id="0x2"><name>PART</name>
    <geom id="0x20"><class ref="0x2"/><name>da0</name><rank>2</rank><config><scheme>GPT</scheme></config>
      <consumer id="0x21"><geom ref="0x20"/><provider ref="0x11"/><mode>r1w1e2</mode><config></config></consumer>
      <provider id="0x22"><geom ref="0x20"/><mode>r1w1e1</mode><name>da0p1</name><mediasize>1048576</mediasize><sectorsize>512</sectorsize>
        <config><index>1</index><type>freebsd-ufs</type><offset>20480</offset><label>rootfs</label></config></provider>
    </geom>
  </class>
  <class id="0x3"><name>LABEL</name>
    <geom id="0x30"><class ref="0x3"/><name>da0p1</name><rank>3</rank><config></config>
      <consumer id="0x31"><geom ref="0x30"/><provider ref="0x22"/><mode>r1w1e1</mode></consumer>
      <provider id="0x32"><geom ref="0x30"/><mode>r1w1e1</mode><name>gpt/rootfs</name><mediasize>1048576</mediasize><sectorsize>512</sectorsize></provider>
    </geom>
  </class>
  <class id="0x4"><name>SWAP</name>
    <geom id="0x40"><class ref="0x4"/><name>swap</name><rank>4</rank>
      <consumer id="0x41"><geom ref="0x40"/><provider ref="0x32"/><mode>r1w1e0</mode></consumer>
    </geom>
  </class>
  <class id="0x5"><name>ELI</name>
    <geom id="0x50"><class ref="0x5"/><name>lost</name><rank>2</rank>
      <provider id="0x52"><geom ref="0x50"/><mode>r0w0e0</mode><name>lost.eli</name><mediasize>1</mediasize><sectorsize>512</sectorsize></provider>
    </geom>
  </class>
</mesh>"#;

    #[test]
    fn mesh_walks_labels_and_partitions_to_the_disk() {
        let m = Mesh::parse(XML);
        let root = m.id("gpt/rootfs").unwrap();
        assert_eq!(m.class(root), "LABEL");
        assert_eq!(m.disks(root, 0).unwrap(), vec!["da0".to_string()]);
        let disk = m.id("da0").unwrap();
        assert_eq!(m.provs[disk].size, 2147483648);
        assert_eq!(m.provs[disk].cfg["descr"], "QEMU & Co HARDDISK");
        assert_eq!(partitions(&m, disk).len(), 1);
        assert_eq!(m.provs[partitions(&m, disk)[0].as_str()].cfg["offset"], "20480");
        assert!(m.busy(disk));
        let mut chain = vec![];
        m.chain(root, &mut chain);
        assert_eq!(chain.len(), 3);
    }

    #[test]
    fn swap_is_protected_and_unknown_layers_fail_closed() {
        let m = Mesh::parse(XML);
        let swap: Vec<_> = m.geoms.values().filter(|g| g.class == "SWAP").flat_map(|g| g.consumes.clone()).collect();
        assert_eq!(m.disks(&swap[0], 0).unwrap(), vec!["da0".to_string()]);
        assert!(m.disks(m.id("lost.eli").unwrap(), 0).is_err());
    }

    #[test]
    fn dump_devices_cover_every_backing_disk_or_fail_closed() {
        let xml = XML.replace("</mesh>", "<class><name>DISK</name><geom id=\"g1\"><provider id=\"p1\"><name>da1</name></provider></geom></class></mesh>");
        let mesh = Mesh::parse(&xml);
        assert!(dump_disks(&mesh, "\n").unwrap().is_empty());
        assert_eq!(dump_disks(&mesh, "gpt/rootfs\n").unwrap(), vec!["da0"]);
        assert_eq!(dump_disks(&mesh, "gpt/rootfs,da1\n").unwrap(), vec!["da0", "da1"]);
        for dump in ["missing", "da1,missing", "da1,", ",da1", "da1,,gpt/rootfs", "none", "lost.eli"] {
            assert!(dump_disks(&mesh, dump).is_err(), "accepted {dump:?}");
        }
    }

    #[test]
    fn newbus_pairs() {
        let p = pairs(r#"vendor=0x46f4 product=0x0001 sernum="FT 0001" release=0x0000"#);
        assert_eq!(p["vendor"], "0x46f4");
        assert_eq!(p["sernum"], "FT 0001");
        assert_eq!(p["release"], "0x0000");
    }
}
