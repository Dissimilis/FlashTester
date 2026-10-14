//! SMART: the drive's own counters, read with read-only commands only: ATA
//! IDENTIFY, SMART READ DATA and READ THRESHOLDS; NVMe Identify Controller and the
//! health log page. Nothing that changes the drive (SMART ENABLE, self-tests, log
//! writes) can be sent. Routes: direct NVMe and SATA, SAT over USB, and the
//! JMicron, Realtek and ASMedia USB-NVMe bridges, picked by USB id only: a vendor
//! command never goes to a device that isn't known to take it.
//!
//! The values are the firmware's claims. They are shown as reported and never
//! turned into a health or life-left score. Around a job, a snapshot before and
//! after shows what the drive itself counted during the run, and the temperature
//! is sampled for the speed graph.

use crate::ctx::Ctx;
use crate::platform::{self, Ata, DiskInfo, Dir};
use crate::report::{Report, size};
use serde::{Deserialize, Serialize};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The counters worth reading without a manual, in the order they are shown.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counter {
    Pending,
    Uncorrectable,
    OfflineUncorrectable,
    Reallocated,
    MediaErrors,
    LinkErrors,
    Temperature,
    PowerOnHours,
    PowerCycles,
    UnsafeShutdowns,
    DataWritten,
    DataRead,
    HotMinutes,
    ThrottleSeconds,
}

impl Counter {
    pub fn label(self) -> &'static str {
        match self {
            Counter::Pending => "Unreadable sectors waiting for remap",
            Counter::Uncorrectable => "Errors the drive could not correct",
            Counter::OfflineUncorrectable => "Bad sectors found by the drive's own scan",
            Counter::Reallocated => "Remapped sectors",
            Counter::MediaErrors => "Media and data integrity errors",
            Counter::LinkErrors => "Cable / link errors (CRC)",
            Counter::Temperature => "Temperature",
            Counter::PowerOnHours => "Powered on",
            Counter::PowerCycles => "Power cycles",
            Counter::UnsafeShutdowns => "Unsafe shutdowns",
            Counter::DataWritten => "Data written",
            Counter::DataRead => "Data read",
            Counter::HotMinutes => "Time above warning temperature",
            Counter::ThrottleSeconds => "Time throttled to cool down",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            Counter::Pending => "Sectors the drive could not read; it remaps them on the next write. Above 0 means data was lost there.",
            Counter::Uncorrectable => "Reads that failed even after the drive's error correction.",
            Counter::OfflineUncorrectable => "Sectors the drive's background scan could not read.",
            Counter::Reallocated => "Sectors retired and replaced by spares. A few can be normal; a count that keeps rising is not.",
            Counter::MediaErrors => "Errors the NVMe controller could not recover from.",
            Counter::LinkErrors => "Transfers garbled between the drive and the computer. Points at the cable, port or adapter, not the flash.",
            Counter::Temperature => "The temperature the drive reports now.",
            Counter::PowerOnHours => "Hours the drive says it has been powered on.",
            Counter::PowerCycles => "How often the drive says it was powered up.",
            Counter::UnsafeShutdowns => "Times power was cut without the drive being told first.",
            Counter::DataWritten => "Total the drive says it has received from computers.",
            Counter::DataRead => "Total the drive says it has sent to computers.",
            Counter::HotMinutes => "Minutes spent above the drive's own warning temperature.",
            Counter::ThrottleSeconds => "Time the drive slowed itself down to cool off.",
        }
    }

    /// A rise means the drive found trouble.
    pub fn bad(self) -> bool {
        matches!(self, Counter::Pending | Counter::Uncorrectable | Counter::OfflineUncorrectable | Counter::Reallocated | Counter::MediaErrors | Counter::LinkErrors)
    }

    pub fn show(self, v: u64) -> String {
        match self {
            Counter::Temperature => format!("{v} °C"),
            Counter::PowerOnHours => format!("{} h", group(v)),
            Counter::DataWritten | Counter::DataRead => size(v),
            Counter::HotMinutes => format!("{} min", group(v)),
            Counter::ThrottleSeconds => format!("{} s", group(v)),
            _ => group(v),
        }
    }
}

/// 1234567 -> "1 234 567".
fn group(v: u64) -> String {
    let s = v.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Key {
    pub what: Counter,
    pub value: u64,
}

/// One line of the full table: an ATA attribute or an NVMe health-log field.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Raw {
    pub id: String,
    pub name: String,
    pub value: String,
    pub worst: String,
    pub thresh: String,
    pub raw: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Smart {
    /// How it was read: "NVMe", "ATA", "SAT over USB", "JMicron USB-NVMe bridge".
    pub via: String,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    /// The drive's own alarms: an ATA attribute at its failure threshold, NVMe critical warnings.
    pub alarms: Vec<String>,
    pub key: Vec<Key>,
    pub raw: Vec<Raw>,
    /// Temperature at which the drive itself starts warning (NVMe), in °C.
    pub warn_temp: Option<u64>,
}

impl Smart {
    pub fn get(&self, c: Counter) -> Option<u64> {
        self.key.iter().find(|k| k.what == c).map(|k| k.value)
    }
}

/// What a job changed: a counter's value before and after.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Change {
    pub what: Counter,
    pub before: u64,
    pub after: u64,
}

/// One temperature reading during a job.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Temp {
    pub secs: f64,
    /// Stream position of the job at that moment.
    pub pos: u64,
    pub c: u64,
}

/// What the drive's counters said about one job.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SmartRun {
    pub via: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub changes: Vec<Change>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub temps: Vec<Temp>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub warn_temp: Option<u64>,
}

const IDENTIFY: Ata = Ata { command: 0xEC, features: 0, lba_mid: 0, lba_high: 0 };
const READ_DATA: Ata = Ata { command: 0xB0, features: 0xD0, lba_mid: 0x4F, lba_high: 0xC2 };
const READ_THRESHOLDS: Ata = Ata { command: 0xB0, features: 0xD1, lba_mid: 0x4F, lba_high: 0xC2 };

#[derive(Clone, Copy, PartialEq, Debug)]
enum Bridge {
    JMicron,
    Realtek,
    ASMedia,
}

impl Bridge {
    fn via(self) -> &'static str {
        match self {
            Bridge::JMicron => "JMicron USB-NVMe bridge",
            Bridge::Realtek => "Realtek USB-NVMe bridge",
            Bridge::ASMedia => "ASMedia USB-NVMe bridge",
        }
    }
}

/// USB-NVMe bridges by VID:PID, from smartmontools' drive database.
const BRIDGES: &[(&str, &str, Bridge)] = &[
    ("152D", "0583", Bridge::JMicron),
    ("152D", "A583", Bridge::JMicron),
    ("152D", "B581", Bridge::JMicron),
    ("30DE", "1000", Bridge::JMicron),
    ("0BDA", "9210", Bridge::Realtek),
    ("0BDA", "9211", Bridge::Realtek),
    ("2EB9", "9211", Bridge::Realtek),
    ("0BDA", "9220", Bridge::Realtek),
    ("1DE1", "E101", Bridge::Realtek),
    ("174C", "2362", Bridge::ASMedia),
    ("174C", "2364", Bridge::ASMedia),
    ("0411", "039D", Bridge::ASMedia),
    ("04E8", "4001", Bridge::ASMedia),
    ("04E8", "61FB", Bridge::ASMedia),
    ("04E8", "61FD", Bridge::ASMedia),
    ("0781", "55AE", Bridge::ASMedia),
    ("0B05", "1932", Bridge::ASMedia),
    ("0BC2", "AA1A", Bridge::ASMedia),
    ("1058", "266E", Bridge::ASMedia),
    ("13FE", "6500", Bridge::ASMedia),
];

fn bridge(d: &DiskInfo) -> Option<Bridge> {
    let u = d.usb.as_ref()?;
    BRIDGES.iter().find(|(v, p, _)| u.vid.eq_ignore_ascii_case(v) && u.pid.eq_ignore_ascii_case(p)).map(|b| b.2)
}

/// Disk types that never carry SMART: virtual disks, SD and MMC slots.
const NO_SMART: &[&str] = &["Virtual", "File-backed virtual", "Storage Spaces", "MMC", "SD", "iSCSI"];

pub fn possible(d: &DiskInfo) -> bool {
    !NO_SMART.contains(&d.bus.as_str())
}

/// Read the drive's SMART data. Needs administrator / root rights.
pub fn read(d: &DiskInfo) -> Result<Smart, String> {
    if !possible(d) {
        return Err(format!("{} disks don't report SMART data.", d.bus));
    }
    let sat = |via: &str| ata(|a| platform::scsi(d, &sat_cdb(a), Dir::In(512)), via);
    let usb = |e: String| format!("Most USB flash drives and card readers have no SMART, and many USB adapters don't pass it through. {e}");
    match d.bus.as_str() {
        "NVMe" => nvme(|op, nsid, cdw10, len| platform::nvme(d, op, nsid, cdw10, len), "NVMe"),
        "SATA" | "ATA" => ata(|a| platform::ata(d, a), "ATA"),
        // Some of these bridges also take SATA drives, which answer SAT instead.
        "USB" => match bridge(d) {
            Some(b) => nvme(|op, nsid, cdw10, len| bridged(d, b, op, nsid, cdw10, len), b.via()).or_else(|e| sat("SAT over USB").map_err(|_| e)),
            None => sat("SAT over USB").map_err(usb),
        },
        _ => sat("SAT"),
    }
}

/// ATA PASS-THROUGH (16): PIO data-in of one sector, length in the sector count.
pub fn sat_cdb(a: Ata) -> [u8; 16] {
    [0x85, 4 << 1, 0x0E, 0, a.features, 0, 1, 0, 0, 0, a.lba_mid, 0, a.lba_high, 0, a.command, 0]
}

/// An NVMe admin command (data in) through a USB bridge's vendor SCSI commands.
fn bridged(d: &DiskInfo, b: Bridge, op: u8, nsid: u32, cdw10: u32, len: usize) -> io::Result<Vec<u8>> {
    let mut c = [0u8; 16];
    match b {
        Bridge::JMicron => return jmicron(d, op, nsid, cdw10, len),
        Bridge::Realtek => {
            c[0] = 0xE4;
            c[1..3].copy_from_slice(&(len as u16).to_le_bytes());
            c[3] = op;
            c[4] = cdw10 as u8;
        }
        Bridge::ASMedia => {
            c[0] = 0xE6;
            c[1] = op;
            c[3] = cdw10 as u8;
            c[7] = (cdw10 >> 16) as u8;
        }
    }
    platform::scsi(d, &c, Dir::In(len))
}

/// JMicron: send the NVMe command in a 512-byte block, read the data, then the
/// completion. All three go through ATA PASS-THROUGH (12) with a protocol byte.
fn jmicron(d: &DiskInfo, op: u8, nsid: u32, cdw10: u32, len: usize) -> io::Result<Vec<u8>> {
    const SIG: u32 = 0x454D_564E; // "NVME"
    let cdb = |proto: u8, n: usize| [0xA1, proto, 0, (n >> 16) as u8, (n >> 8) as u8, n as u8, 0, 0, 0, 0, 0, 0];
    let mut cmd = [0u8; 512];
    cmd[0..4].copy_from_slice(&SIG.to_le_bytes());
    cmd[8..12].copy_from_slice(&(op as u32).to_le_bytes());
    cmd[12..16].copy_from_slice(&nsid.to_le_bytes());
    cmd[48..52].copy_from_slice(&cdw10.to_le_bytes());
    platform::scsi(d, &cdb(0x80, 512), Dir::Out(&cmd))?;
    let data = platform::scsi(d, &cdb(0x82, len), Dir::In(len))?;
    let reply = platform::scsi(d, &cdb(0x8F, 512), Dir::In(512))?;
    if reply.len() < 24 || le32(&reply, 0) != SIG {
        return Err(io::Error::other("bridge gave no NVMe completion"));
    }
    if le32(&reply, 20) >> 17 != 0 {
        return Err(io::Error::other(format!("NVMe status {:#x}", le32(&reply, 20) >> 17)));
    }
    Ok(data)
}

fn le16(b: &[u8], o: usize) -> u64 {
    u16::from_le_bytes([b[o], b[o + 1]]) as u64
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
/// NVMe's 128-bit counters, saturated to u64.
fn le128(b: &[u8], o: usize) -> u64 {
    u128::from_le_bytes(b[o..o + 16].try_into().unwrap()).min(u64::MAX as u128) as u64
}

fn text(b: &[u8]) -> String {
    b.iter().map(|&c| if (0x20..0x7f).contains(&c) { c as char } else { ' ' }).collect::<String>().trim().to_string()
}

fn nvme(f: impl Fn(u8, u32, u32, usize) -> io::Result<Vec<u8>>, via: &str) -> Result<Smart, String> {
    let id = f(0x06, 0, 1, 4096).map_err(|e| format!("The drive did not answer NVMe Identify ({e})."))?;
    // Get Log Page 02h (SMART / health), 512 bytes: NUMDL = 128 dwords - 1.
    let log = f(0x02, 0xFFFF_FFFF, (127 << 16) | 0x02, 512).map_err(|e| format!("The drive did not return its health log ({e})."))?;
    parse_nvme(&id, &log, via)
}

fn parse_nvme(id: &[u8], log: &[u8], via: &str) -> Result<Smart, String> {
    if id.len() < 4096 || log.len() < 512 {
        return Err("The drive returned too little data.".into());
    }
    let model = text(&id[24..64]);
    if model.is_empty() {
        return Err("The drive returned empty identify data.".into());
    }
    let mut s = Smart { via: via.into(), model, serial: text(&id[4..24]), firmware: text(&id[64..72]), ..Default::default() };
    let kelvin = |k: u64| (k > 273).then(|| k - 273);
    s.warn_temp = kelvin(le16(id, 266));
    let cw = log[0];
    for (bit, msg) in [
        (0, "available spare space is below the drive's threshold"),
        (1, "temperature is outside the drive's limits"),
        (2, "the drive reports degraded reliability (media or internal errors)"),
        (3, "the drive has put itself into read-only mode"),
        (4, "the drive's volatile memory backup has failed"),
        (5, "persistent memory region has become read-only"),
    ] {
        if cw & (1 << bit) != 0 {
            s.alarms.push(format!("Critical warning: {msg}"));
        }
    }
    let temp = kelvin(le16(log, 1));
    let unit = 512_000u64;
    let key = [
        (Counter::MediaErrors, Some(le128(log, 160))),
        (Counter::Temperature, temp),
        (Counter::PowerOnHours, Some(le128(log, 128))),
        (Counter::PowerCycles, Some(le128(log, 112))),
        (Counter::UnsafeShutdowns, Some(le128(log, 144))),
        (Counter::DataWritten, Some(le128(log, 48).saturating_mul(unit))),
        (Counter::DataRead, Some(le128(log, 32).saturating_mul(unit))),
        (Counter::HotMinutes, Some(le32(log, 192) as u64)),
        (Counter::ThrottleSeconds, Some(le32(log, 224) as u64 + le32(log, 228) as u64)),
    ];
    s.key = key.into_iter().filter_map(|(what, v)| v.map(|value| Key { what, value })).collect();
    let row = |id: String, name: &str, value: String| Raw { id, name: name.into(), value, ..Default::default() };
    let c = |v: Option<u64>| v.map(|t| format!("{t} °C")).unwrap_or_else(|| "-".into());
    let mut raw = vec![
        row("0".into(), "Critical warning", format!("{cw:#04x}")),
        row("1".into(), "Composite temperature", c(temp)),
        row("3".into(), "Available spare (vendor's estimate)", format!("{} %", log[3])),
        row("4".into(), "Available spare threshold", format!("{} %", log[4])),
        row("5".into(), "Percentage used (vendor's estimate)", format!("{} %", log[5])),
        row("32".into(), "Data units read (512,000 bytes)", group(le128(log, 32))),
        row("48".into(), "Data units written (512,000 bytes)", group(le128(log, 48))),
        row("64".into(), "Host read commands", group(le128(log, 64))),
        row("80".into(), "Host write commands", group(le128(log, 80))),
        row("96".into(), "Controller busy time (min)", group(le128(log, 96))),
        row("112".into(), "Power cycles", group(le128(log, 112))),
        row("128".into(), "Power-on hours", group(le128(log, 128))),
        row("144".into(), "Unsafe shutdowns", group(le128(log, 144))),
        row("160".into(), "Media and data integrity errors", group(le128(log, 160))),
        row("176".into(), "Error log entries", group(le128(log, 176))),
        row("192".into(), "Warning temperature time (min)", group(le32(log, 192) as u64)),
        row("196".into(), "Critical temperature time (min)", group(le32(log, 196) as u64)),
    ];
    for i in 0..8 {
        if let Some(t) = kelvin(le16(log, 200 + 2 * i)) {
            raw.push(row((200 + 2 * i).to_string(), &format!("Temperature sensor {}", i + 1), format!("{t} °C")));
        }
    }
    raw.extend([
        row("216".into(), "Thermal throttle 1 transitions", group(le32(log, 216) as u64)),
        row("220".into(), "Thermal throttle 2 transitions", group(le32(log, 220) as u64)),
        row("224".into(), "Thermal throttle 1 time (s)", group(le32(log, 224) as u64)),
        row("228".into(), "Thermal throttle 2 time (s)", group(le32(log, 228) as u64)),
    ]);
    if let Some(w) = s.warn_temp {
        raw.push(row("id 266".into(), "Warning temperature threshold", format!("{w} °C")));
    }
    if let Some(t) = kelvin(le16(id, 268)) {
        raw.push(row("id 268".into(), "Critical temperature threshold", format!("{t} °C")));
    }
    s.raw = raw;
    Ok(s)
}

/// ATA strings are byte-swapped within each 16-bit word.
fn ata_str(id: &[u8], word: usize, words: usize) -> String {
    let b: Vec<u8> = (word..word + words).flat_map(|w| [id[2 * w + 1], id[2 * w]]).collect();
    text(&b)
}

fn ata(f: impl Fn(Ata) -> io::Result<Vec<u8>>, via: &str) -> Result<Smart, String> {
    let id = f(IDENTIFY).map_err(|e| format!("The drive did not answer ATA IDENTIFY ({e})."))?;
    if id.len() < 512 || ata_str(&id, 27, 20).is_empty() {
        return Err("The drive returned no valid identify data.".into());
    }
    let w = |i: usize| u16::from_le_bytes([id[2 * i], id[2 * i + 1]]);
    if w(82) & 1 == 0 || w(82) == 0xFFFF {
        return Err("The drive does not support SMART.".into());
    }
    if w(85) & 1 == 0 {
        return Err("SMART is turned off on this drive. FlashTester does not change drive settings.".into());
    }
    let data = f(READ_DATA).map_err(|e| format!("The drive did not return its SMART data ({e})."))?;
    if data.len() < 512 || data.iter().all(|&b| b == 0) {
        return Err("The drive returned empty SMART data.".into());
    }
    let th = f(READ_THRESHOLDS).ok().filter(|t| t.len() >= 512);
    Ok(parse_ata(&id, &data, th.as_deref(), via))
}

fn attr_name(id: u8) -> &'static str {
    match id {
        1 => "Raw read error rate",
        2 => "Throughput performance",
        3 => "Spin-up time",
        4 => "Start/stop count",
        5 => "Reallocated sectors",
        7 => "Seek error rate",
        8 => "Seek time performance",
        9 => "Power-on hours",
        10 => "Spin retry count",
        11 => "Calibration retry count",
        12 => "Power cycle count",
        13 => "Soft read error rate",
        22 => "Helium level",
        170 => "Available reserved space",
        171 => "Program fail count",
        172 => "Erase fail count",
        173 => "Wear leveling count",
        174 => "Unexpected power loss count",
        175 => "Program fail count (chip)",
        176 => "Erase fail count (chip)",
        177 => "Wear leveling count",
        178 => "Used reserved blocks (chip)",
        179 => "Used reserved blocks (total)",
        180 => "Unused reserved blocks",
        181 => "Program fail count (total)",
        182 => "Erase fail count (total)",
        183 => "Runtime bad blocks / SATA downshifts",
        184 => "End-to-end errors",
        187 => "Reported uncorrectable errors",
        188 => "Command timeouts",
        189 => "High fly writes",
        190 => "Airflow temperature",
        191 => "G-sense error rate",
        192 => "Power-off retract / unsafe shutdowns",
        193 => "Load cycle count",
        194 => "Temperature",
        195 => "Hardware ECC recovered",
        196 => "Reallocation events",
        197 => "Current pending sectors",
        198 => "Offline uncorrectable sectors",
        199 => "UDMA CRC errors",
        200 => "Multi-zone / write error rate",
        201 => "Soft read error rate",
        202 => "Data address mark errors / lifetime used",
        206 => "Flying height",
        220 => "Disk shift",
        222 => "Loaded hours",
        223 => "Load retry count",
        224 => "Load friction",
        225 => "Load cycles / host writes",
        226 => "Load-in time",
        230 => "GMR head amplitude / life protection",
        231 => "Life left / temperature (vendor's estimate)",
        232 => "Endurance remaining (vendor's estimate)",
        233 => "Media wearout indicator (vendor's estimate)",
        234 => "Average / max erase count",
        235 => "Good blocks / power-fail backup",
        240 => "Head flying hours",
        241 => "Total LBAs written",
        242 => "Total LBAs read",
        244 => "Average erase count",
        245 => "Max erase count",
        246 => "Total erase count",
        247 => "Host program pages",
        248 => "FTL program pages",
        249 => "NAND writes (GiB)",
        250 => "Read error retry rate",
        251 => "Minimum spares remaining",
        252 => "Newly added bad blocks",
        254 => "Free fall protection",
        _ => "Vendor specific",
    }
}

fn parse_ata(id: &[u8], data: &[u8], th: Option<&[u8]>, via: &str) -> Smart {
    let mut s = Smart { via: via.into(), model: ata_str(id, 27, 20), serial: ata_str(id, 10, 10), firmware: ata_str(id, 23, 4), ..Default::default() };
    let mut attrs = vec![];
    for i in 0..30 {
        let o = 2 + i * 12;
        let a = data[o];
        if a == 0 {
            continue;
        }
        let flags = le16(data, o + 1);
        let raw = data[o + 5..o + 11].iter().rev().fold(0u64, |acc, &b| acc << 8 | b as u64);
        let thresh = th.and_then(|t| (0..30).map(|j| 2 + j * 12).find(|&p| t[p] == a).map(|p| t[p + 1]));
        attrs.push((a, flags, data[o + 3], data[o + 4], raw, thresh));
    }
    // Counts take the low 32 bits: some vendors pack other numbers into the high bytes.
    let get = |a: u8| attrs.iter().find(|x| x.0 == a).map(|x| x.4 & 0xFFFF_FFFF);
    let temp = |a: u8| attrs.iter().find(|x| x.0 == a).map(|x| x.4 & 0xFF).filter(|t| (1..128).contains(t));
    let key = [
        (Counter::Pending, get(197)),
        (Counter::Uncorrectable, get(187)),
        (Counter::OfflineUncorrectable, get(198)),
        (Counter::Reallocated, get(5)),
        (Counter::LinkErrors, get(199)),
        (Counter::Temperature, temp(194).or_else(|| temp(190))),
        (Counter::PowerOnHours, get(9)),
        (Counter::PowerCycles, get(12)),
        (Counter::UnsafeShutdowns, get(174).or_else(|| get(192))),
    ];
    s.key = key.into_iter().filter_map(|(what, v)| v.map(|value| Key { what, value })).collect();
    for (a, flags, value, worst, raw, thresh) in attrs {
        let name = attr_name(a);
        if let Some(t) = thresh
            && t > 0
            && t < 0xFE
            && value <= t
        {
            s.alarms.push(format!("{name} ({a}) is at or below the drive's failure threshold ({value}, threshold {t})"));
        }
        s.raw.push(Raw {
            id: a.to_string(),
            name: format!("{name}{}", if flags & 1 != 0 { " (pre-fail)" } else { "" }),
            value: value.to_string(),
            worst: worst.to_string(),
            thresh: thresh.map(|t| t.to_string()).unwrap_or_else(|| "-".into()),
            raw: if raw >> 32 == 0 { raw.to_string() } else { format!("{raw:#014x}") },
        });
    }
    s
}

/// Counters that moved between two snapshots.
pub fn changes(before: &Smart, after: &Smart) -> Vec<Change> {
    before
        .key
        .iter()
        .filter(|k| k.what != Counter::Temperature)
        .filter_map(|k| after.get(k.what).filter(|&v| v != k.value).map(|v| Change { what: k.what, before: k.value, after: v }))
        .collect()
}

/// A snapshot before a job, temperature samples while it runs, a snapshot after.
pub struct Watch {
    disk: DiskInfo,
    before: Smart,
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<Vec<Temp>>,
}

/// Seconds between temperature readings.
const POLL: u64 = 30;

impl Watch {
    /// None when the drive doesn't answer; then the job runs without it.
    pub fn start(d: &DiskInfo, ctx: &Ctx) -> Option<Watch> {
        let before = read(d).ok()?;
        let stop = Arc::new(AtomicBool::new(false));
        let (dd, c, s, first, serial) = (d.clone(), ctx.clone(), stop.clone(), before.get(Counter::Temperature), before.serial.clone());
        let thread = std::thread::spawn(move || {
            let t0 = Instant::now();
            let mut temps = vec![];
            if let Some(t) = first {
                temps.push(Temp { secs: 0.0, pos: 0, c: t });
                c.temp(0, t);
            }
            'poll: loop {
                for _ in 0..POLL * 4 {
                    if s.load(Ordering::SeqCst) {
                        break 'poll;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                let st = c.status();
                let pos = st.base + st.done;
                // Stop at the first failure (a bridge that chokes once may choke again)
                // and when the disk number names another drive after a replug.
                let Some(t) = read(&dd).ok().filter(|m| m.serial == serial).and_then(|m| m.get(Counter::Temperature)) else { break };
                temps.push(Temp { secs: t0.elapsed().as_secs_f64(), pos, c: t });
                c.temp(pos, t);
            }
            temps
        });
        Some(Watch { disk: d.clone(), before, stop, thread })
    }

    /// Read the counters again and put what changed into the report.
    pub fn finish(self, rep: &mut Report) {
        self.stop.store(true, Ordering::SeqCst);
        let temps = self.thread.join().unwrap_or_default();
        // After a replug the disk number may name another drive: compare identity.
        let after = read(&self.disk).ok().filter(|a| a.serial == self.before.serial && a.model == self.before.model);
        judge(&self.before, after.as_ref(), temps, rep);
    }
}

/// Put what the drive's counters did during a run into the report: rising error
/// counters and new alarms are warnings, throttling and heat are notes.
fn judge(before: &Smart, after: Option<&Smart>, temps: Vec<Temp>, rep: &mut Report) {
    let changes = after.map(|a| changes(before, a)).unwrap_or_default();
    let bad: Vec<String> = changes
        .iter()
        .filter(|c| c.what.bad() && c.after > c.before && c.what != Counter::LinkErrors)
        .map(|c| format!("{} {} -> {}", c.what.label().to_lowercase(), group(c.before), group(c.after)))
        .collect();
    if !bad.is_empty() {
        rep.warnings.push(format!("The drive's own counters rose during this run: {}.", bad.join(", ")));
    }
    if let Some(c) = changes.iter().find(|c| c.what == Counter::LinkErrors && c.after > c.before) {
        rep.warnings.push(format!(
            "Cable/link (CRC) errors rose from {} to {} during this run: data was garbled on the way, not on the flash. Try another cable, port or adapter.",
            c.before, c.after
        ));
    }
    if let Some(a) = after {
        for alarm in a.alarms.iter().filter(|x| !before.alarms.contains(x)) {
            rep.warnings.push(format!("New alarm from the drive: {alarm}."));
        }
    }
    if let Some(c) = changes.iter().find(|c| c.what == Counter::ThrottleSeconds && c.after > c.before) {
        rep.notes.push(format!("The drive throttled itself for {} s to cool down during this run; slow stretches in the speed graph may be heat, not flash.", c.after - c.before));
    }
    let max = temps.iter().map(|t| t.c).max();
    if let (Some(m), Some(w)) = (max, before.warn_temp)
        && m >= w
    {
        rep.notes.push(format!("The drive reached {m} C, at or above its own warning temperature of {w} C."));
    }
    rep.smart = Some(SmartRun { via: before.via.clone(), changes, temps, warn_temp: before.warn_temp });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_ata_str(id: &mut [u8], word: usize, s: &str) {
        for (i, pair) in s.as_bytes().chunks(2).enumerate() {
            id[2 * (word + i) + 1] = pair[0];
            id[2 * (word + i)] = *pair.get(1).unwrap_or(&b' ');
        }
    }

    fn attr(data: &mut [u8], slot: usize, id: u8, flags: u16, value: u8, raw: u64) {
        let o = 2 + slot * 12;
        data[o] = id;
        data[o + 1..o + 3].copy_from_slice(&flags.to_le_bytes());
        data[o + 3] = value;
        data[o + 4] = value;
        data[o + 5..o + 11].copy_from_slice(&raw.to_le_bytes()[..6]);
    }

    fn ata_pages() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut id = vec![0u8; 512];
        put_ata_str(&mut id, 27, "Samsung SSD 870 EVO 1TB");
        put_ata_str(&mut id, 10, "S5Y1NX0R123456");
        put_ata_str(&mut id, 23, "SVT02B6Q");
        id[164] = 1; // word 82: SMART supported
        id[170] = 1; // word 85: SMART enabled
        let mut data = vec![0u8; 512];
        attr(&mut data, 0, 5, 0x33, 100, 3);
        attr(&mut data, 1, 9, 0x32, 99, 1234);
        attr(&mut data, 2, 194, 0x22, 60, 0x0037_0014_0029); // 41 °C, min/max packed above
        attr(&mut data, 3, 197, 0x32, 100, 0);
        attr(&mut data, 4, 199, 0x3E, 100, 7);
        attr(&mut data, 5, 177, 0x13, 5, 1500); // wear attribute at its threshold
        let mut th = vec![0u8; 512];
        for (slot, (a, t)) in [(5u8, 10u8), (9, 0), (194, 0), (197, 0), (199, 0), (177, 5)].iter().enumerate() {
            th[2 + slot * 12] = *a;
            th[3 + slot * 12] = *t;
        }
        (id, data, th)
    }

    #[test]
    fn ata_parse() {
        let (id, data, th) = ata_pages();
        let s = parse_ata(&id, &data, Some(&th), "ATA");
        assert_eq!(s.model, "Samsung SSD 870 EVO 1TB");
        assert_eq!(s.serial, "S5Y1NX0R123456");
        assert_eq!(s.firmware, "SVT02B6Q");
        assert_eq!(s.get(Counter::Reallocated), Some(3));
        assert_eq!(s.get(Counter::Temperature), Some(41));
        assert_eq!(s.get(Counter::PowerOnHours), Some(1234));
        assert_eq!(s.get(Counter::LinkErrors), Some(7));
        assert_eq!(s.get(Counter::Uncorrectable), None);
        assert_eq!(s.raw.len(), 6);
        assert_eq!(s.raw[2].raw, "0x003700140029");
        assert_eq!(s.alarms.len(), 1, "{:?}", s.alarms);
        assert!(s.alarms[0].contains("(177)"));
    }

    #[test]
    fn ata_refuses_empty_or_disabled() {
        let (id, data, _) = ata_pages();
        let mut off = id.clone();
        off[170] = 0;
        assert!(ata(|a| Ok(if a.command == 0xEC { off.clone() } else { data.clone() }), "ATA").unwrap_err().contains("turned off"));
        assert!(ata(|_| Ok(vec![0u8; 512]), "ATA").is_err());
        assert!(ata(|a| Ok(if a.command == 0xEC { id.clone() } else { vec![0u8; 512] }), "ATA").unwrap_err().contains("empty"));
        // No thresholds: still read, no alarms.
        let s = ata(|a| if a.features == 0xD1 { Err(io::Error::other("no")) } else { Ok(if a.command == 0xEC { id.clone() } else { data.clone() }) }, "ATA").unwrap();
        assert!(s.alarms.is_empty());
    }

    fn nvme_pages() -> (Vec<u8>, Vec<u8>) {
        let mut id = vec![0u8; 4096];
        id[4..12].copy_from_slice(b"S6B0NL0W");
        id[24..41].copy_from_slice(b"Samsung SSD 980 1");
        id[64..72].copy_from_slice(b"3B4QFXO7");
        id[266..268].copy_from_slice(&355u16.to_le_bytes()); // 82 °C
        let mut log = vec![0u8; 512];
        log[0] = 0b100; // reliability degraded
        log[1..3].copy_from_slice(&314u16.to_le_bytes()); // 41 °C
        log[48] = 200; // 200 units = 102.4 MB
        log[128] = 77;
        log[144] = 4;
        log[160] = 2;
        log[224..228].copy_from_slice(&30u32.to_le_bytes());
        log[228..232].copy_from_slice(&5u32.to_le_bytes());
        (id, log)
    }

    #[test]
    fn nvme_parse() {
        let (id, log) = nvme_pages();
        let s = parse_nvme(&id, &log, "NVMe").unwrap();
        assert_eq!(s.model, "Samsung SSD 980 1");
        assert_eq!(s.serial, "S6B0NL0W");
        assert_eq!(s.warn_temp, Some(82));
        assert_eq!(s.get(Counter::Temperature), Some(41));
        assert_eq!(s.get(Counter::DataWritten), Some(102_400_000));
        assert_eq!(s.get(Counter::PowerOnHours), Some(77));
        assert_eq!(s.get(Counter::UnsafeShutdowns), Some(4));
        assert_eq!(s.get(Counter::MediaErrors), Some(2));
        assert_eq!(s.get(Counter::ThrottleSeconds), Some(35));
        assert_eq!(s.alarms.len(), 1);
        assert!(parse_nvme(&vec![0u8; 4096], &log, "NVMe").is_err());
    }

    #[test]
    fn nvme_command_words() {
        let (id, log) = nvme_pages();
        let s = nvme(
            |op, nsid, cdw10, len| match (op, nsid, cdw10, len) {
                (0x06, 0, 1, 4096) => Ok(id.clone()),
                (0x02, 0xFFFF_FFFF, 0x007F_0002, 512) => Ok(log.clone()),
                x => Err(io::Error::other(format!("unexpected {x:?}"))),
            },
            "NVMe",
        );
        assert!(s.is_ok(), "{s:?}");
    }

    #[test]
    fn sat_and_bridge_commands() {
        assert_eq!(sat_cdb(READ_DATA), [0x85, 0x08, 0x0E, 0, 0xD0, 0, 1, 0, 0, 0, 0x4F, 0, 0xC2, 0, 0xB0, 0]);
        let mut d = DiskInfo { bus: "USB".into(), ..Default::default() };
        assert_eq!(bridge(&d), None);
        d.usb = Some(platform::UsbInfo { vid: "174c".into(), pid: "2362".into(), ..Default::default() });
        assert_eq!(bridge(&d), Some(Bridge::ASMedia));
        d.usb = Some(platform::UsbInfo { vid: "0781".into(), pid: "5581".into(), ..Default::default() });
        assert_eq!(bridge(&d), None, "a plain SanDisk stick gets no vendor commands");
    }

    #[test]
    fn deltas() {
        let (id, log) = nvme_pages();
        let a = parse_nvme(&id, &log, "NVMe").unwrap();
        let mut log2 = log.clone();
        log2[1..3].copy_from_slice(&330u16.to_le_bytes());
        log2[160] = 5;
        let b = parse_nvme(&id, &log2, "NVMe").unwrap();
        assert_eq!(changes(&a, &b), vec![Change { what: Counter::MediaErrors, before: 2, after: 5 }]);
        assert_eq!(group(1234567), "1 234 567");
        assert_eq!(Counter::PowerOnHours.show(12345), "12 345 h");
    }

    #[test]
    fn run_verdicts() {
        let (id, data, th) = ata_pages();
        let a = parse_ata(&id, &data, Some(&th), "ATA");
        let mut more = data.clone();
        attr(&mut more, 3, 197, 0x32, 100, 8); // pending 0 -> 8
        attr(&mut more, 4, 199, 0x3E, 100, 9); // CRC 7 -> 9
        let b = parse_ata(&id, &more, Some(&th), "ATA");
        let hot = vec![Temp { secs: 0.0, pos: 0, c: 41 }];
        let mut rep = Report::default();
        judge(&a, Some(&b), hot.clone(), &mut rep);
        assert_eq!(rep.warnings.len(), 2, "{:?}", rep.warnings);
        assert!(rep.warnings[0].contains("waiting for remap 0 -> 8"));
        assert!(rep.warnings[1].contains("from 7 to 9"));
        assert_eq!(rep.smart.as_ref().unwrap().changes.len(), 2);
        // The drive vanished (or another took its number): no changes, no warnings.
        let mut rep = Report::default();
        judge(&a, None, hot, &mut rep);
        assert!(rep.warnings.is_empty() && rep.smart.unwrap().changes.is_empty());
        // NVMe: a new critical warning, throttling and heat.
        let (nid, log) = nvme_pages();
        let mut cool = log.clone();
        cool[0] = 0;
        cool[224..232].fill(0);
        let n0 = parse_nvme(&nid, &cool, "NVMe").unwrap();
        let n1 = parse_nvme(&nid, &log, "NVMe").unwrap();
        let mut rep = Report::default();
        judge(&n0, Some(&n1), vec![Temp { secs: 30.0, pos: 1, c: 85 }], &mut rep);
        assert!(rep.warnings.iter().any(|w| w.contains("degraded reliability")), "{:?}", rep.warnings);
        assert!(rep.notes.iter().any(|n| n.contains("throttled itself for 35 s")), "{:?}", rep.notes);
        assert!(rep.notes.iter().any(|n| n.contains("reached 85 C")), "{:?}", rep.notes);
    }
}
