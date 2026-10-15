//! NAND candidates and bounded controller reply parsers. Device reports are evidence,
//! never proof of capacity or authenticity. No vendor commands run during enumeration.
use crate::{
    platform::DiskInfo,
    report::{Report, Status, Target},
};
use serde::{Deserialize, Serialize};

pub const PROVENANCE: &str = include_str!("flash-data/provenance.json");
pub const PARSER_VERSION: u32 = 3;

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ")
}

pub fn parse_id(text: &str) -> Result<Vec<u8>, String> {
    let s: String = text.chars().filter(|c| !c.is_ascii_whitespace() && *c != ':' && *c != '-').collect();
    if !matches!(s.len(), 8 | 12) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Supply exactly four or six NAND ID bytes in device order, e.g. AD D3 10 A6 34 00.".into());
    }
    Ok((0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub source_index: usize,
    pub vendor: String,
    pub part: String,
    pub cell_type: String,
    /// Unmodified source token, not a measured topology.
    pub die_count_code: String,
    /// Unmodified source value; no capacity unit or usable capacity is asserted.
    pub capacity_value: u64,
    pub extended_match: String,
    pub raw_record: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Nand {
    pub id: String,
    pub candidates: Vec<Candidate>,
    /// Observed repeated byte patterns, not a die/channel count.
    pub repeated_offsets: Vec<usize>,
}

pub fn lookup(id: &[u8]) -> Result<Nand, String> {
    if !matches!(id.len(), 4 | 6) {
        return Err("NAND ID must contain four or six bytes.".into());
    }
    let primary = u32::from_le_bytes(id[..4].try_into().unwrap());
    let extra = (id.len() == 6).then(|| u16::from_le_bytes([id[4], id[5]]));
    let mut candidates = Vec::new();
    for (source_index, raw) in include_str!("flash-data/nand.csv").lines().enumerate() {
        let fields: Vec<_> = raw.split(',').collect();
        if u32::from_str_radix(fields[2], 16).unwrap() != primary {
            continue;
        }
        let ext = u16::from_str_radix(fields[6], 16).unwrap();
        let extended_match = match extra {
            None => "not_supplied",
            Some(_) if ext == 0 => "unspecified_in_database",
            Some(e) if e == ext => "exact",
            Some(_) => "different",
        };
        candidates.push(Candidate {
            source_index,
            vendor: fields[0].into(),
            part: fields[1].into(),
            cell_type: fields[3].into(),
            die_count_code: fields[4].into(),
            capacity_value: fields[5].parse().unwrap(),
            extended_match: extended_match.into(),
            raw_record: raw.into(),
        });
    }
    candidates.sort_by_key(|c| match c.extended_match.as_str() {
        "exact" | "not_supplied" => 0,
        "unspecified_in_database" => 1,
        _ => 2,
    });
    Ok(Nand { id: hex(id), candidates, repeated_offsets: vec![] })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Family {
    Phison,
    SmiLegacy,
    SmiLegacy30,
    SmiNew,
}

impl Family {
    pub const ALL: [Self; 4] = [Self::Phison, Self::SmiLegacy, Self::SmiLegacy30, Self::SmiNew];
    pub fn label(self) -> &'static str {
        match self {
            Self::Phison => "Phison",
            Self::SmiLegacy => "Silicon Motion legacy",
            Self::SmiLegacy30 => "Silicon Motion legacy subtype 3",
            Self::SmiNew => "Silicon Motion new",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Identification {
    pub parser_version: u32,
    pub source: String,
    pub database: serde_json::Value,
    pub family: Option<Family>,
    pub model: Option<String>,
    pub identity_validation: String,
    pub nand: Option<Nand>,
    pub diagnostics: Vec<String>,
    pub raw_response: Vec<u8>,
    pub identity: Option<CapturedIdentity>,
    /// Optional saved OS identity supplied for comparison; never a live observation.
    pub comparison_identity: Option<DiskInfo>,
}

impl Identification {
    pub fn new(source: &str, family: Option<Family>) -> Self {
        Self {
            parser_version: PARSER_VERSION,
            source: source.into(),
            database: serde_json::from_str(PROVENANCE).unwrap(),
            family,
            model: None,
            identity_validation: "not_checked".into(),
            nand: None,
            diagnostics: vec![],
            raw_response: vec![],
            identity: None,
            comparison_identity: None,
        }
    }
    pub fn lines(&self) -> Vec<String> {
        let source = match self.source.as_str() {
            "user_supplied_nand_id" => "NAND ID entered by the user",
            "imported_nand_response" => "saved NAND response",
            "imported_identity_response" => "saved controller identity response",
            "imported_smi_discovery_response" => "saved Silicon Motion discovery response",
            other => other,
        };
        let mut lines = vec![format!("Source: {source}")];
        if let Some(f) = self.family {
            lines.push(format!("Response layout: {}", f.label()));
        }
        if let Some(m) = &self.model {
            lines.push(format!("Controller data: {m}"));
        }
        if let Some(i) = &self.identity {
            lines.push(format!("Identity layout: {} ({})", i.family, i.layout));
            lines.push(format!("Captured USB ID: {}:{}", i.vid.as_deref().unwrap_or("unknown"), i.pid.as_deref().unwrap_or("unknown")));
            lines.push(format!(
                "Captured vendor / product / revision: {} / {} / {}",
                i.vendor.as_deref().unwrap_or("unavailable"),
                i.product.as_deref().unwrap_or("unavailable"),
                i.revision.as_deref().unwrap_or("unavailable")
            ));
        }
        if self.family.is_some() || self.identity.is_some() || self.model.is_some() {
            let validation = match self.identity_validation.as_str() {
                "not_checked" => "not checked",
                "mismatch_or_insufficient_evidence" => "the supplied identity differs or has too few fields to compare",
                other => other,
            };
            lines.push(format!("Identity comparison: {validation}"));
        }
        if let Some(n) = &self.nand {
            lines.push(format!("NAND ID: {}. Matching database records: {}.", n.id, n.candidates.len()));
            for c in &n.candidates {
                let extra = match c.extended_match.as_str() {
                    "exact" => "exact match",
                    "unspecified_in_database" => "unspecified in the database",
                    "not_supplied" => "not supplied",
                    _ => "different",
                };
                lines.push(format!("{} {} ({}, extended ID: {extra})", c.vendor, c.part, c.cell_type));
            }
            if n.candidates.is_empty() {
                lines.push("No matching NAND part in the embedded database.".into());
            }
        }
        lines.extend(self.diagnostics.clone());
        lines.push("These results cannot confirm usable capacity or whether the drive is genuine.".into());
        lines
    }
}

pub fn report(result: Identification) -> Report {
    let mut rep = Report::new("identify", Target { path: Some("User-supplied identification data".into()), ..Default::default() });
    rep.set(
        Status::Info,
        if result.nand.is_some() {
            "NAND database lookup."
        } else if result.identity.is_some() || result.model.is_some() {
            "Controller identity from saved data (unverified)."
        } else {
            "Could not identify the device from this response."
        },
    );
    rep.verdict.details = result.lines();
    rep.flash = Some(result);
    rep.finished = crate::engine::now();
    rep
}

fn slice(b: &[u8], offset: usize, len: usize) -> Result<&[u8], String> {
    b.get(offset..offset.checked_add(len).ok_or("reply offset overflow")?).ok_or_else(|| format!("Short reply: need {} bytes, got {}.", offset + len, b.len()))
}

fn ascii(b: &[u8]) -> Result<String, String> {
    let end = b.iter().position(|&v| v == 0).unwrap_or(b.len());
    if !b[..end].iter().all(|v| (0x20..=0x7e).contains(v)) {
        return Err("Malformed identity string.".into());
    }
    Ok(String::from_utf8(b[..end].to_vec()).unwrap().trim().into())
}

pub fn smi_model(b: &[u8]) -> Result<String, String> {
    // 0x456bdc tests marker starts at 0x0f..=0x1ef in a 512-byte
    // discovery reply. Extra captured bytes must not supply a marker or terminator.
    let b = &b[..b.len().min(512)];
    for offset in 0x0f..b.len().saturating_sub(2).min(0x1f0) {
        if &b[offset..offset + 3] != b"SM3" {
            continue;
        }
        let tail = &b[offset..b.len().min(offset + 24)];
        let end = tail.iter().position(|v| !v.is_ascii_alphanumeric() && *v != b'-');
        if let Some(end) = end.filter(|e| *e >= 6 && tail[3..6].iter().all(u8::is_ascii_digit)) {
            return ascii(&tail[..end]);
        }
    }
    Err("No complete SM3 model marker in the supplied discovery reply.".into())
}

pub fn parse_nand(family: Family, b: &[u8]) -> Result<Nand, String> {
    let base = match family {
        Family::Phison => 0,
        Family::SmiLegacy => 0x20,
        _ => 0x30,
    };
    let id = slice(b, base, 6)?;
    if id[..4].iter().all(|v| *v == 0) || id[..4].iter().all(|v| *v == 0xff) {
        return Err("Reply contains an empty NAND ID.".into());
    }
    let prefix = format!("{:02X}{:02X}", id[0], id[1]);
    let use_extended = family == Family::Phison || include_str!("flash-data/prefixes.txt").lines().any(|p| p == prefix);
    let mut nand = lookup(if use_extended { id } else { &id[..4] })?;
    if let Ok(region) = slice(b, base, 0x44) {
        nand.repeated_offsets = (4..=0x40).filter(|&i| region[i..i + 4] == id[..4]).map(|i| base + i).collect();
    }
    Ok(nand)
}

#[derive(Clone, Debug, Deserialize)]
pub struct Profile {
    pub key: String,
    pub family: String,
    vid: Option<usize>,
    pid: Option<usize>,
    big_endian: bool,
    vendor: Option<usize>,
    product: Option<usize>,
    revision: Option<usize>,
}

pub fn profiles() -> &'static [Profile] {
    static PROFILES: std::sync::OnceLock<Vec<Profile>> = std::sync::OnceLock::new();
    PROFILES.get_or_init(|| serde_json::from_str(include_str!("flash-data/profiles.json")).unwrap())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapturedIdentity {
    pub layout: String,
    pub family: String,
    pub vid: Option<String>,
    pub pid: Option<String>,
    // None means an unavailable field (unused or unresolved), distinct from a decoded blank.
    // serde also accepts the strings written by parser version 1 as Some(string).
    pub vendor: Option<String>,
    pub product: Option<String>,
    pub revision: Option<String>,
}

fn inquiry_offsets(profile: &Profile, b: &[u8]) -> Result<[Option<usize>; 3], String> {
    if profile.key != "0x4639d8" {
        return Ok([profile.vendor, profile.product, profile.revision]);
    }
    // Alcor constructor 0x4639d8: manufacturer payload is at 0x18;
    // the two preceding USB string descriptors determine the inquiry start.
    let next_descriptor = |header: usize| -> Result<usize, String> {
        let len = slice(b, header, 1)?[0] as usize;
        if len < 2 || len % 2 != 0 {
            return Err(format!("Invalid Alcor string descriptor length {len} at 0x{header:X}."));
        }
        slice(b, header, len)?;
        Ok(header + len)
    };
    let product_header = next_descriptor(0x16)?;
    let vendor = next_descriptor(product_header)?;
    Ok([Some(vendor), Some(vendor + 8), Some(vendor + 24)])
}

pub fn parse_identity(profile: &Profile, b: &[u8]) -> Result<CapturedIdentity, String> {
    if b.is_empty() || b.iter().all(|v| *v == 0) || b.iter().all(|v| *v == 0xff) {
        return Err("Empty identity reply.".into());
    }
    let word = |offset: Option<usize>| -> Result<Option<String>, String> {
        // Zero is a valid reply offset (notably the SSS PID). Null means unavailable.
        let Some(offset) = offset else { return Ok(None) };
        let raw = slice(b, offset, 2)?.try_into().unwrap();
        let word = if profile.big_endian { u16::from_be_bytes(raw) } else { u16::from_le_bytes(raw) };
        Ok(Some(format!("{word:04X}")))
    };
    let string = |offset: Option<usize>, len| -> Result<Option<String>, String> {
        match offset {
            Some(o) => ascii(slice(b, o, len)?).map(Some),
            None => Ok(None),
        }
    };
    let [vendor, product, revision] = inquiry_offsets(profile, b)?;
    Ok(CapturedIdentity {
        layout: profile.key.clone(),
        family: profile.family.clone(),
        vid: word(profile.vid)?,
        pid: word(profile.pid)?,
        vendor: string(vendor, 8)?,
        product: string(product, 16)?,
        revision: string(revision, 4)?,
    })
}

/// Compare only independently supplied observations. A selected layout alone never
/// verifies a family; an imported DiskInfo is user-supplied evidence, not a fresh scan.
fn compare_capture(identity: &CapturedIdentity, expected: &DiskInfo) -> Result<(), String> {
    let usb = expected.usb.as_ref().ok_or("Saved identity has no USB VID/PID.")?;
    let (Some(vid), Some(pid)) = (&identity.vid, &identity.pid) else {
        return Err("Insufficient evidence: captured VID/PID is unavailable for comparison.".into());
    };
    if !vid.eq_ignore_ascii_case(&usb.vid) || !pid.eq_ignore_ascii_case(&usb.pid) {
        return Err("Captured VID/PID conflicts with the saved identity.".into());
    }
    let mut matched = 0;
    for (actual, saved) in [(&identity.vendor, &expected.vendor), (&identity.product, &expected.product), (&identity.revision, &expected.revision)] {
        if saved.trim().is_empty() {
            continue;
        }
        let Some(actual) = actual else { continue };
        if !actual.eq_ignore_ascii_case(saved.trim()) {
            return Err("Captured inquiry strings conflict with the saved identity.".into());
        }
        matched += 1;
    }
    if matched == 0 {
        return Err("Insufficient evidence: no inquiry fields are available in both the capture and saved identity.".into());
    }
    Ok(())
}

pub fn decode_nand(family: Family, bytes: Vec<u8>) -> Identification {
    let mut result = Identification::new("imported_nand_response", Some(family));
    match parse_nand(family, &bytes) {
        Ok(n) => result.nand = Some(n),
        Err(e) => result.diagnostics.push(e),
    }
    result.identity_validation = "unverified; layout supplied by user".into();
    result.raw_response = bytes;
    result
}

pub fn decode_smi_model(bytes: Vec<u8>) -> Identification {
    let mut result = Identification::new("imported_smi_discovery_response", None);
    match smi_model(&bytes) {
        Ok(m) => result.model = Some(m),
        Err(e) => result.diagnostics.push(e),
    }
    result.identity_validation = "unverified; model marker only".into();
    result.raw_response = bytes;
    result
}

pub fn decode_identity(profile: &Profile, bytes: Vec<u8>, expected: Option<&DiskInfo>) -> Identification {
    let mut result = Identification::new("imported_identity_response", None);
    result.comparison_identity = expected.cloned();
    result.diagnostics.push(format!(
        "Layout {} ({}). Reads the USB ID and inquiry fields where their locations are known. USB descriptor strings and other validation bytes are not decoded.",
        profile.key, profile.family
    ));
    match parse_identity(profile, &bytes) {
        Ok(identity) => {
            let unavailable: Vec<_> = [("vendor", &identity.vendor), ("product", &identity.product), ("revision", &identity.revision)]
                .into_iter()
                .filter_map(|(name, value)| value.is_none().then_some(name))
                .collect();
            if !unavailable.is_empty() {
                result.diagnostics.push(format!("Unavailable inquiry fields: {}. These fields were not compared.", unavailable.join(", ")));
            }
            result.identity_validation = match expected {
                Some(d) => match compare_capture(&identity, d) {
                    Ok(()) => "matches supplied VID/PID and available inquiry fields; hardware unverified".into(),
                    Err(e) => {
                        result.diagnostics.push(e);
                        "mismatch_or_insufficient_evidence".into()
                    }
                },
                None => "unverified; layout supplied by user".into(),
            };
            if profile.family == "Phison" {
                if let Ok(raw) = slice(&bytes, 0x17e, 2) {
                    let word = u16::from_le_bytes(raw.try_into().unwrap());
                    if word != 0 && word != 0xffff {
                        result.model = Some(format!("Phison complemented model code {:04X} (not a resolved part number)", !word));
                    }
                }
            }
            result.identity = Some(identity);
        }
        Err(e) => result.diagnostics.push(e),
    }
    result.raw_response = bytes;
    result
}

/// Accept regular capture files only, bounded before allocation. No physical device
/// paths or pass-through commands are involved in offline decoding.
pub fn read_capture(path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > 65536 {
        return Err("Capture must be a regular file of at most 64 KiB.".into());
    }
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() > 65536 {
        return Err("Capture exceeds 64 KiB.".into());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_and_ambiguity() {
        assert_eq!(include_str!("flash-data/nand.csv").lines().count(), 1457);
        let n = lookup(&parse_id("AD:D3:10:A6:34:00").unwrap()).unwrap();
        assert_eq!(n.candidates.len(), 3);
        assert_eq!(n.candidates.iter().filter(|c| c.extended_match == "exact").count(), 2);
        assert!(lookup(&[0, 0, 0, 0]).unwrap().candidates.is_empty());
        for text in ["", "AD D3 10", "0xADD310A6", "AD D3 10 A6 34", "AD D3 10 A6 34 00 00", "💥💥"] {
            assert!(parse_id(text).is_err());
        }
        for raw in include_str!("flash-data/nand.csv").lines() {
            let f: Vec<_> = raw.split(',').collect();
            let id = u32::from_str_radix(f[2], 16).unwrap().to_le_bytes();
            assert!(lookup(&id).unwrap().candidates.iter().any(|c| c.raw_record == raw));
        }
    }

    #[test]
    fn reply_boundaries() {
        for family in Family::ALL {
            for len in 0..=600 {
                let b = vec![0; len];
                assert!(parse_nand(family, &b).is_err());
                assert!(smi_model(&b).is_err());
            }
            assert!(parse_nand(family, &[0xff; 512]).is_err());
        }
        assert!(smi_model(b"SM3").is_err());
        assert!(smi_model(b"SM3257ENAA").is_err());
        let mut discovery = vec![0; 0x0f];
        discovery.extend_from_slice(b"SM3257ENAA\0");
        assert_eq!(smi_model(&discovery).unwrap(), "SM3257ENAA");
        let mut b = [0u8; 512];
        b[..6].copy_from_slice(&[0xad, 0xd3, 0x10, 0xa6, 0x34, 0]);
        b[64..68].copy_from_slice(&[0xad, 0xd3, 0x10, 0xa6]);
        assert_eq!(parse_nand(Family::Phison, &b).unwrap().repeated_offsets, vec![64]);
    }

    #[test]
    fn extended_ids_keep_alternatives_and_source_tokens() {
        let primary = lookup(&[0xad, 0xd3, 0x10, 0xa6]).unwrap();
        assert!(primary.candidates.iter().all(|c| c.extended_match == "not_supplied"));
        let different = lookup(&[0xad, 0xd3, 0x10, 0xa6, 0xff, 0xff]).unwrap();
        assert_eq!(different.candidates.len(), primary.candidates.len());
        assert!(different.candidates.iter().all(|c| c.extended_match != "exact"));
        for token in ["16", "A"] {
            let raw = include_str!("flash-data/nand.csv").lines().find(|r| r.split(',').nth(4) == Some(token)).unwrap();
            let id = u32::from_str_radix(raw.split(',').nth(2).unwrap(), 16).unwrap().to_le_bytes();
            assert!(lookup(&id).unwrap().candidates.iter().any(|c| c.die_count_code == token));
        }
        let mut b = vec![0; 512];
        b[0x30..0x36].copy_from_slice(&[0xad, 0xd3, 0x10, 0xa6, 0x34, 0]);
        assert_eq!(parse_nand(Family::SmiNew, &b).unwrap().id, "AD D3 10 A6 34 00");
        assert_eq!(parse_nand(Family::SmiLegacy30, &b).unwrap().id, "AD D3 10 A6 34 00");
        assert!(parse_nand(Family::SmiLegacy, &b).is_err());
    }

    #[test]
    fn captured_identity_boundaries_and_conflicts() {
        assert_eq!(profiles().len(), 16);
        for profile in profiles() {
            let mut b = vec![b' '; 1024];
            for len in 0..=1024 {
                let _ = parse_identity(profile, &b[..len]);
            }
            assert!(parse_identity(profile, &b).is_ok());
            if let Some(o) = profile.vendor.filter(|o| *o != 0) {
                b[o] = 0x80;
                assert!(parse_identity(profile, &b).is_err());
            }
        }
        let profile = profiles().iter().find(|p| p.family == "Phison").unwrap();
        let mut b = vec![0; 528];
        b[8..12].copy_from_slice(&[0xfe, 0x13, 0x23, 0x1f]);
        b[0xb0..0xb8].copy_from_slice(b"Vendor  ");
        b[0x9c..0xac].copy_from_slice(b"USB DISK        ");
        b[0xb8..0xbc].copy_from_slice(b"1.00");
        let mut expected = DiskInfo {
            vendor: "Vendor".into(),
            product: "USB DISK".into(),
            revision: "1.00".into(),
            usb: Some(crate::platform::UsbInfo { vid: "13fe".into(), pid: "1f23".into(), ..Default::default() }),
            ..Default::default()
        };
        let result = decode_identity(profile, b.clone(), Some(&expected));
        assert!(result.identity_validation.starts_with("matches supplied"));
        expected.product = "Another drive".into();
        let result = decode_identity(profile, b.clone(), Some(&expected));
        assert_eq!(result.identity_validation, "mismatch_or_insufficient_evidence");
        assert_eq!(result.raw_response, b);
        expected.usb.as_mut().unwrap().vid = "090C".into();
        assert!(compare_capture(result.identity.as_ref().unwrap(), &expected).is_err());
        // Opaque validation bytes at 0x1e0 are never used as a NAND ID.
        b[0x1e0..0x1e6].copy_from_slice(&[0xad, 0xd3, 0x10, 0xa6, 0x34, 0]);
        assert!(decode_identity(profile, b, None).nand.is_none());
        let result = decode_nand(Family::Phison, vec![0xff; 5]);
        assert!(result.nand.is_none());
        assert_eq!(result.raw_response, vec![0xff; 5]);
    }

    #[test]
    fn reports_roundtrip_and_escape_capture_text() {
        let mut result = Identification::new("user_supplied_nand_id", None);
        result.nand = Some(lookup(&[0xad, 0xd3, 0x10, 0xa6, 0x34, 0]).unwrap());
        result.diagnostics.push("<script>alert(1)</script>".into());
        let report = report(result);
        assert!(!report.html().contains("<script>alert"));
        assert!(report.html().contains("&lt;script&gt;"));
        let json = serde_json::to_string(&report).unwrap();
        let restored: Report = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.flash.unwrap().nand.unwrap().candidates.len(), 3);
        let old: Report = serde_json::from_str(&json.replace(&format!(",\"flash\":{}", serde_json::to_string(report.flash.as_ref().unwrap()).unwrap()), "")).unwrap();
        assert!(old.flash.is_none());
    }

    #[test]
    fn sss_pid_at_zero_matches_saved_identity() {
        let p = profiles().iter().find(|p| p.key == "0x46423c").unwrap();
        let mut b = vec![0; 512];
        b[..4].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]);
        b[0x61..0x69].copy_from_slice(b"Vendor  ");
        b[0x69..0x79].copy_from_slice(b"USB DISK        ");
        b[0x79..0x7d].copy_from_slice(b"1.00");
        let expected = saved_identity();
        let decoded = decode_identity(p, b, Some(&expected));
        assert_eq!(decoded.identity.as_ref().unwrap().pid.as_deref(), Some("1234"));
        assert!(decoded.identity_validation.starts_with("matches supplied"));
    }

    fn saved_identity() -> DiskInfo {
        DiskInfo {
            vendor: "Vendor".into(),
            product: "USB DISK".into(),
            revision: "1.00".into(),
            usb: Some(crate::platform::UsbInfo { vid: "5678".into(), pid: "1234".into(), ..Default::default() }),
            ..Default::default()
        }
    }

    fn alcor_reply(manufacturer_len: u8, product_len: u8) -> Vec<u8> {
        let mut b = vec![0; 1024];
        b[12..16].copy_from_slice(&[0x78, 0x56, 0x34, 0x12]);
        b[0x16] = manufacturer_len;
        let product_header = 0x16 + manufacturer_len as usize;
        b[product_header] = product_len;
        let vendor = product_header + product_len as usize;
        b[vendor..vendor + 8].copy_from_slice(b"Vendor  ");
        b[vendor + 8..vendor + 24].copy_from_slice(b"USB DISK        ");
        b[vendor + 24..vendor + 28].copy_from_slice(b"1.00");
        b
    }

    #[test]
    fn alcor_inquiry_offsets_follow_each_reply() {
        let p = profiles().iter().find(|p| p.key == "0x4639d8").unwrap();
        let expected = saved_identity();
        for (manufacturer_len, product_len) in [(16, 10), (8, 22), (2, 2), (254, 254)] {
            let b = alcor_reply(manufacturer_len, product_len);
            let decoded = decode_identity(p, b.clone(), Some(&expected));
            assert!(decoded.identity_validation.starts_with("matches supplied"), "{:?}", decoded.diagnostics);
            let end = 0x16 + manufacturer_len as usize + product_len as usize + 28;
            for len in 0..end {
                assert!(parse_identity(p, &b[..len]).is_err(), "accepted truncation at {len}");
            }
        }
        for value in [0, 1, 3, 255] {
            for offset in [0x16, 0x26] {
                let mut b = alcor_reply(16, 10);
                b[offset] = value;
                let decoded = decode_identity(p, b.clone(), Some(&expected));
                assert!(decoded.identity.is_none());
                assert_eq!(decoded.raw_response, b);
                assert!(!decoded.diagnostics.iter().any(|d| d.contains("conflict")));
            }
        }
    }

    #[test]
    fn smi_search_uses_recovered_marker_region() {
        let mut b = vec![0; 65536];
        b[..7].copy_from_slice(b"SM3999\0");
        b[0x160..0x16b].copy_from_slice(b"SM3257ENAA\0");
        assert_eq!(smi_model(&b).unwrap(), "SM3257ENAA");
        for offset in [0x0e, 0x0f, 0x1ef, 0x1f0, 512, 600] {
            let mut b = vec![0; 1024];
            b[offset..offset + 7].copy_from_slice(b"SM3257\0");
            assert_eq!(smi_model(&b).is_ok(), matches!(offset, 0x0f | 0x1ef), "offset {offset:#x}");
        }
        let mut b = vec![0; 1024];
        b[0x1ef..0x202].copy_from_slice(b"SM3257ABCDEFGHIJKL\0");
        assert!(smi_model(&b).is_err(), "terminator outside the 512-byte reply");
    }

    #[test]
    fn sandisk_decodes_only_fields_used_by_its_consumer() {
        let p = profiles().iter().find(|p| p.key == "0x4641b0").unwrap();
        let mut b = vec![0; 512];
        b[0x36..0x46].copy_from_slice(b"USB DISK        ");
        // 0x45fc28 consumes product and serial only; the other zero fields in
        // 0x4641b0 are unused, unlike SSS's genuine PID offset of zero.
        for prefix in [*b"ABCD1234", [0x80; 8]] {
            b[..8].copy_from_slice(&prefix);
            let decoded = decode_identity(p, b.clone(), Some(&saved_identity()));
            let identity = decoded.identity.unwrap();
            assert_eq!(identity.product.as_deref(), Some("USB DISK"));
            assert_eq!((identity.vid, identity.pid, identity.vendor, identity.revision), (None, None, None, None));
            assert!(decoded.diagnostics.iter().any(|d| d.contains("Insufficient evidence")));
            assert!(!decoded.diagnostics.iter().any(|d| d.contains("conflict")));
        }
    }

    #[test]
    fn unresolved_inquiry_fields_and_blank_strings_are_distinct() {
        let mut b = vec![0; 512];
        let mut p = profiles().iter().find(|p| p.key == "0x463c20").unwrap().clone();
        p.vendor = None;
        p.product = None;
        p.revision = None;
        b[8..12].copy_from_slice(&[0x78, 0x56, 0x34, 0x12]);
        let result = decode_identity(&p, b.clone(), Some(&saved_identity()));
        assert_eq!(result.identity.as_ref().unwrap().vendor, None);
        assert!(result.diagnostics.iter().any(|d| d.contains("Insufficient evidence")));
        assert!(!result.diagnostics.iter().any(|d| d.contains("conflict")));
        // A decoded blank is real evidence and should still conflict with a nonblank vendor.
        p.vendor = Some(0xb0);
        let result = decode_identity(&p, b, Some(&saved_identity()));
        assert_eq!(result.identity.as_ref().unwrap().vendor.as_deref(), Some(""));
        assert!(result.diagnostics.iter().any(|d| d.contains("conflict")));
        // Version 1 report strings remain readable with the new optional fields.
        let old: CapturedIdentity =
            serde_json::from_str(r#"{"layout":"0x463c20","family":"Phison","vid":"13FE","pid":"1F23","vendor":"Vendor","product":"USB DISK","revision":"1.00"}"#).unwrap();
        assert_eq!(old.vendor.as_deref(), Some("Vendor"));
    }

    #[test]
    fn identification_evidence_never_claims_a_capacity_test() {
        let mut success = Identification::new("user_supplied_nand_id", None);
        success.nand = Some(lookup(&[0xad, 0xd3, 0x10, 0xa6]).unwrap());
        for result in [success, decode_nand(Family::Phison, vec![0; 3])] {
            let original = report(result);
            // Include the render path: a saved JSON report must produce the same evidence.
            let restored: Report = serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();
            let html = restored.evidence_html();
            assert!(html.contains("Flash identification evidence"));
            assert!(html.contains("Offline identification"));
            assert!(!html.contains("Every 512-byte sector was written"));
            assert!(!html.contains("Claimed capacity"));
            assert!(!html.contains("Capacity map"));
        }
        let normal = Report::new("test", Target::default()).evidence_html();
        assert!(normal.contains("Flash drive test evidence"));
        assert!(normal.contains("does not establish that the drive's full claimed capacity stores data"));
    }
}
