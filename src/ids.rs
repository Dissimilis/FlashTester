//! Built-in identification tables: who a USB vendor ID belongs to, the chip behind a
//! USB ID that names exactly one chip, SD and eMMC card makers, and NVMe controllers
//! by PCI ID. Linux can add names from the system's own usb.ids (see platform).

/// Who a USB vendor ID belongs to, for common flash controller, bridge chip and drive
/// makers (names as in the USB ID database). A drive with a chip maker's ID carries
/// no vendor ID of its own brand, as no-name sticks do.
pub fn usb_vendor(vid: &str) -> Option<String> {
    const CHIPS: &[(&str, &str)] = &[
        ("090C", "Silicon Motion"), ("13FE", "Phison"), ("0D7D", "Phison"), ("058F", "Alcor Micro"),
        ("1E3D", "Chipsbank"), ("0204", "Chipsbank"), ("048D", "ITE"), ("1F75", "Innostor"),
        ("0C76", "JMTek"), ("0EF5", "PointChips"), ("0EA0", "Ours Technology"), ("04FC", "Sunplus"),
        ("152D", "JMicron"), ("174C", "ASMedia"), ("0BDA", "Realtek"), ("05E3", "Genesys Logic"),
        ("2109", "VIA Labs"), ("067B", "Prolific"), ("14CD", "Super Top"),
    ];
    const BRANDS: &[(&str, &str)] = &[
        ("0951", "Kingston"), ("0781", "SanDisk"), ("1307", "Transcend"), ("8564", "Transcend"),
        ("0930", "Toshiba"), ("0480", "Toshiba"), ("054C", "Sony"), ("1005", "Apacer"),
        ("125F", "ADATA"), ("0DD8", "Netac"), ("18A5", "Verbatim"), ("04E8", "Samsung"),
        ("1B1C", "Corsair"), ("0718", "Imation"), ("154B", "PNY"), ("05DC", "Lexar"),
        ("3538", "PQI"), ("1E68", "TrekStor"), ("0BC2", "Seagate"), ("1058", "Western Digital"),
    ];
    let vid = vid.to_ascii_uppercase();
    let find = |t: &[(&str, &'static str)]| t.iter().find(|(v, _)| *v == vid).map(|(_, n)| *n);
    if let Some(n) = find(CHIPS) {
        return Some(format!("{n}, chip maker"));
    }
    if let Some(n) = find(BRANDS) {
        return Some(n.to_string());
    }
    ["0000", "FFFF"].contains(&vid.as_str()).then(|| "no vendor ID".to_string())
}

/// The chip behind a USB ID whose usb.ids entry names exactly one chip. IDs shared by
/// several chips (174C:55AA, 090C:1000, ...) are left out. A fake can copy any ID, so
/// this says what the ID belongs to, not what is inside.
pub fn usb_chip(vid: &str, pid: &str) -> Option<&'static str> {
    const T: &[(&str, &str)] = &[
        // flash drive controllers
        ("0204:6026", "Chipsbank CBM1180 flash controller"), ("048D:1165", "ITE IT1165 flash controller"),
        ("058F:6998", "Alcor Micro AU6998 flash controller"), ("0D7D:1320", "Phison PS2031 flash controller"),
        ("0D7D:1420", "Phison PS2044 flash controller"), ("13FE:1F23", "Phison PS2232 flash controller"),
        ("1F75:0902", "Innostor IS902 flash controller"), ("1F75:0916", "Innostor IS916 flash controller"),
        ("1F75:0917", "Innostor IS917 flash controller"), ("1F75:0918", "Innostor IS918 flash controller"),
        ("0EA0:6803", "Ours Technology OTI-6803 flash controller"), ("0EA0:6808", "Ours Technology OTI-6808 flash controller"),
        ("0EA0:6828", "Ours Technology OTI-6828 flash controller"),
        // card readers
        ("058F:6364", "Alcor Micro AU6477 card reader"), ("058F:6377", "Alcor Micro AU6375 card reader"),
        ("05E3:0723", "Genesys Logic GL827L card reader"), ("0BDA:0111", "Realtek RTS5111 card reader"),
        ("0BDA:0116", "Realtek RTS5116 card reader"), ("0BDA:0129", "Realtek RTS5129 card reader"),
        ("0BDA:0138", "Realtek RTS5138 card reader"), ("0BDA:0139", "Realtek RTS5139 card reader"),
        ("0BDA:0159", "Realtek RTS5159 card reader"), ("0BDA:0184", "Realtek RTS5182 card reader"),
        ("1E3D:4082", "Chipsbank CBM4082 card reader"),
        // SATA, IDE and NVMe bridges
        ("05E3:0702", "Genesys Logic GL811E IDE bridge"), ("05E3:0731", "Genesys Logic GL3310 SATA bridge"),
        ("04FC:0C15", "Sunplus SPIF215A SATA bridge"), ("04FC:0C25", "Sunplus SPIF225A SATA bridge"),
        ("067B:2507", "Prolific PL2507 IDE bridge"), ("067B:2773", "Prolific PL2773 SATA bridge"),
        ("0BDA:9210", "Realtek RTL9210 NVMe bridge"), ("152D:0551", "JMicron JMS551 SATA bridge"),
        ("152D:0561", "JMicron JMS551 SATA bridge"), ("152D:0562", "JMicron JMS567 SATA bridge"),
        ("152D:0567", "JMicron JMS567 SATA bridge"), ("152D:0578", "JMicron JMS578 SATA bridge"),
        ("152D:578E", "JMicron JMS578 SATA bridge"), ("152D:0583", "JMicron JMS583 NVMe bridge"),
        ("152D:1561", "JMicron JMS561U SATA bridge"), ("152D:2329", "JMicron JM20329 SATA bridge"),
        ("152D:2338", "JMicron JM20337 SATA/PATA bridge"), ("152D:2339", "JMicron JM20339 SATA bridge"),
        ("152D:2551", "JMicron JMS551 SATA bridge"), ("152D:2566", "JMicron JMS566 SATA bridge"),
        ("152D:2590", "JMicron JMS567 SATA bridge"), ("152D:3562", "JMicron JMS567 SATA bridge"),
        ("152D:3569", "JMicron JMS566 SATA bridge"), ("174C:1151", "ASMedia ASM1151W SATA bridge"),
        ("174C:1153", "ASMedia ASM1153 SATA bridge"), ("174C:5106", "ASMedia ASM1051 SATA bridge"),
        ("174C:5136", "ASMedia ASM1053 SATA bridge"), ("174C:51D6", "ASMedia ASM1051W SATA bridge"),
        ("1F75:0611", "Innostor IS611 SATA/PATA bridge"), ("1F75:0621", "Innostor IS621 SATA bridge"),
        ("1F75:0888", "Innostor IS888 SATA bridge"), ("2109:0700", "VIA Labs VL700 SATA bridge"),
        ("2109:0701", "VIA Labs VL701 SATA bridge"), ("2109:0711", "VIA Labs VL711 SATA bridge"),
        ("14CD:6116", "Super Top M6116 SATA bridge"),
    ];
    let key = format!("{}:{}", vid.to_ascii_uppercase(), pid.to_ascii_uppercase());
    T.iter().find(|(k, _)| *k == key).map(|(_, n)| *n)
}

/// Preserve the distinction highlighted by the usbflashinfo research: USB IDs
/// select possible families; only a validated controller reply can narrow a model.
/// Exact reader/bridge entries take priority over the broader vendor hint.
pub fn usb_controller_hint(vid: &str, pid: &str) -> String {
    if let Some(chip) = usb_chip(vid, pid) { return format!("{chip} (USB ID hint)"); }
    let family = match vid.to_ascii_uppercase().as_str() {
        "090C" => "Silicon Motion",
        "13FE" | "0D7D" => "Phison",
        "058F" => "Alcor Micro",
        "1E3D" | "0204" => "Chipsbank",
        "048D" => "ITE",
        "1F75" => "Innostor",
        _ => return String::new(),
    };
    format!("{family} family (USB vendor ID hint; model unknown)")
}

/// Card maker from the manufacturer ID in an SD or (e)MMC card's CID register, as
/// compiled by mmc-utils (sdcard.ids, multimediacard.ids). The SD Association keeps
/// its list private and some IDs are shared, hence "A or B".
pub fn card_maker(sd: bool, mid: u32) -> Option<&'static str> {
    const SD: &[(u32, &str)] = &[
        (0x01, "Panasonic"), (0x02, "Toshiba, Kingston or Viking"), (0x03, "SanDisk"), (0x08, "Silicon Power"),
        (0x09, "ATP"), (0x18, "Infineon"), (0x1B, "Samsung or Transcend"), (0x1C, "Transcend"),
        (0x1D, "ADATA or Corsair"), (0x1E, "Transcend"), (0x1F, "Kingston"), (0x27, "Phison or Delkin"),
        (0x28, "Lexar"), (0x30, "SanDisk"), (0x31, "Silicon Power"), (0x33, "STMicroelectronics"),
        (0x41, "Kingston"), (0x6F, "STMicroelectronics"), (0x74, "Transcend"), (0x76, "Patriot"), (0x82, "Sony or Gobe"),
    ];
    const MMC: &[(u32, &str)] = &[
        (0x00, "SanDisk"), (0x02, "Kingston or SanDisk"), (0x03, "Toshiba"), (0x11, "Toshiba"), (0x13, "Micron"),
        (0x15, "Samsung, SanDisk or LG"), (0x2C, "Kingston"), (0x37, "KingMax"), (0x44, "ATP"), (0x45, "SanDisk"),
        (0x70, "Kingston"), (0xFE, "Micron"),
    ];
    (if sd { SD } else { MMC }).iter().find(|(m, _)| *m == mid).map(|(_, n)| *n)
}

/// NVMe controller by PCI vendor and device ID ("0x1987", "5018"), named from the
/// embedded subset of the PCI ID Repository: "Phison ... E18 ... (PCI 1987:5018)".
pub fn nvme_controller(vendor: &str, device: &str) -> String {
    let (v, d) = (vendor.trim().trim_start_matches("0x").to_ascii_lowercase(), device.trim().trim_start_matches("0x").to_ascii_lowercase());
    let name = include_str!("nvme.ids").lines().filter(|l| !l.starts_with('#')).find_map(|l| {
        let mut it = l.trim_end().splitn(3, ' ');
        (it.next()? == v && it.next()? == d).then(|| it.next().unwrap_or("").replace(" | ", " "))
    });
    let id = format!("PCI {}:{}", v.to_ascii_uppercase(), d.to_ascii_uppercase());
    match name {
        Some(n) => format!("{n} ({id})"),
        None => id,
    }
}

/// Vendor and product names for a USB ID in the text of a usb.ids file.
pub fn usb_ids_lookup(text: &str, vid: &str, pid: &str) -> Option<(String, String)> {
    let (vid, pid) = (format!("{}  ", vid.to_ascii_lowercase()), format!("{}  ", pid.to_ascii_lowercase()));
    let mut lines = text.lines();
    let vendor = lines.by_ref().find_map(|l| l.strip_prefix(vid.as_str()))?.trim().to_string();
    let product = lines.take_while(|l| l.starts_with('\t') || l.starts_with('#')).find_map(|l| l.strip_prefix('\t')?.strip_prefix(pid.as_str())).unwrap_or("").trim().to_string();
    Some((vendor, product))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups() {
        assert_eq!(usb_controller_hint("090c", "1000"), "Silicon Motion family (USB vendor ID hint; model unknown)");
        assert!(usb_controller_hint("058f", "6364").contains("card reader (USB ID hint)"));
        assert!(usb_controller_hint("0951", "1234").is_empty());
        assert_eq!(usb_vendor("090c").unwrap(), "Silicon Motion, chip maker");
        assert_eq!(usb_vendor("0951").unwrap(), "Kingston");
        assert_eq!(usb_vendor("0000").unwrap(), "no vendor ID");
        assert!(usb_vendor("ABCD").is_none());
        assert_eq!(usb_chip("152d", "0583"), Some("JMicron JMS583 NVMe bridge"));
        assert!(usb_chip("174C", "55AA").is_none());
        assert_eq!(card_maker(true, 0x1B), Some("Samsung or Transcend"));
        assert_eq!(card_maker(false, 0x15), Some("Samsung, SanDisk or LG"));
        assert_eq!(nvme_controller("0x1987", "0x5018"), "Phison Electronics Corporation E18 PCIe4 NVMe Controller (PCI 1987:5018)");
        assert_eq!(nvme_controller("1987", "ffff"), "PCI 1987:FFFF");
        let ids = "# c\n090c  Silicon Motion\n\t1000  Flash Drive\n\t\t00  iface\n\t3267  USB DISK\n0951  Kingston\n\t1000  Other\n";
        assert_eq!(usb_ids_lookup(ids, "090C", "3267"), Some(("Silicon Motion".into(), "USB DISK".into())));
        assert_eq!(usb_ids_lookup(ids, "090c", "9999"), Some(("Silicon Motion".into(), String::new())));
        assert_eq!(usb_ids_lookup(ids, "0951", "1000").unwrap().1, "Other");
        assert!(usb_ids_lookup(ids, "1234", "1000").is_none());
    }
}
