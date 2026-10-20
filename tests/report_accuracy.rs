use flashtester::{
    engine::{BadRange, Event, IoStats, Stop, Tally, VerifyResult},
    perf::{IopsResult, IopsRow},
    probe::{Finding, ProbeResult},
    report::{Report, Status, Target},
};

const MIB: u64 = 1 << 20;

fn verified(read: u64, missing: u64) -> Report {
    let mut report = Report::new("verify", Target::default());
    report.tested = read + missing;
    let mut tally = Tally::default();
    tally.ok = read / 512;
    tally.unreadable = missing / 512;
    if missing > 0 {
        tally.bad_ranges.push(BadRange { start: read, end: read + missing, kind: "unreadable".into() });
    }
    let mut stats = IoStats::default();
    stats.bytes = read;
    report.verify.push(VerifyResult {
        stats,
        tally,
        stop: Stop::Done,
        events: vec![],
    });
    report
}

#[test]
fn sparse_probe_is_not_evidence_of_full_capacity() {
    for status in [Status::Ok, Status::Info] {
        let mut report = Report::new("probe", Target { claimed: 128 * MIB, ..Default::default() });
        report.set(status, "Samples retained their data");
        report.probe = Some(ProbeResult { finding: Finding::Genuine, usable: 128 * MIB, claimed: 128 * MIB, ..Default::default() });
        assert_eq!(report.real_capacity(), None);
        let html = report.evidence_html();
        assert!(!html.contains("Estimated usable capacity"));
        assert!(!html.contains("Capacity that actually stores data"));
        assert!(html.contains("Space between samples is not verified"));
        assert!(!html.contains("Every 512-byte sector was written"));
    }
    let mut report = Report::new("probe", Target::default());
    report.probe = Some(ProbeResult { finding: Finding::Fake, usable: 32 * MIB, claimed: 128 * MIB, ..Default::default() });
    assert_eq!(report.real_capacity(), Some(32 * MIB));
    assert!(report.evidence_html().contains("Estimated usable capacity"));
}

#[test]
fn unreadable_tail_remains_in_evidence_map() {
    let report = verified(64 * MIB, 64 * MIB);
    let html = report.evidence_html();
    assert_eq!(html.matches("<i class=p-ok ").count(), 100);
    assert_eq!(html.matches("<i class=p-bad ").count(), 100);
    assert!(!html.contains("written and read back"));
    assert!(html.contains("did not write new test data"));
}

#[test]
fn cancelled_verification_leaves_unchecked_space_gray() {
    let mut report = verified(64 * MIB, 0);
    report.tested = 128 * MIB;
    report.verify[0].stop = Stop::Cancelled;
    let html = report.evidence_html();
    assert_eq!(html.matches("<i class=p-ok ").count(), 100);
    assert_eq!(html.matches("<i class=p-unknown ").count(), 100);
}

#[test]
fn missing_range_details_are_not_painted_as_verified() {
    let mut report = verified(64 * MIB, 64 * MIB);
    report.verify[0].tally.bad_ranges_truncated = true;
    report.verify[0].tally.bad_ranges.clear();
    assert_eq!(report.evidence_html().matches("<i class=p-unknown ").count(), 200);
}

#[test]
fn imported_report_fields_are_escaped_in_html() {
    let payload = "<script>alert('fixture')</script>";
    let mut report = verified(MIB, MIB);
    report.verify[0].tally.bad_ranges[0].kind = payload.into();
    report.verify[0].events.push(Event { time: payload.into(), kind: payload.into(), offset: 0, len: 512, detail: payload.into() });
    report.iops = Some(IopsResult {
        rows: vec![IopsRow { kind: payload.into(), qd: 1, iops: 1.0, mb_s: 1.0, p50_ms: 1.0, p99_ms: 1.0, max_ms: 1.0 }],
        ..Default::default()
    });
    // Exercise the same deserialization boundary as `render`.
    let report: Report = serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
    let html = report.html();
    assert!(!html.contains(payload));
    assert_eq!(html.matches("&lt;script&gt;alert('fixture')&lt;/script&gt;").count(), 5);
}

#[test]
fn hidden_serials_leave_no_copy_in_saved_files() {
    use flashtester::platform::{CardInfo, DiskInfo, UsbInfo};
    let disk = DiskInfo {
        id: "3".into(),
        serial: "AA11BB22CC33".into(),
        instance_id: r"USBSTOR\DISK&VEN_X&PROD_Y\0123456789ABCDEF&0".into(),
        usb: Some(UsbInfo { serial: Some("0123456789ABCDEF".into()), ..Default::default() }),
        card: Some(CardInfo { kind: "SD".into(), serial: "0x5EC2E7A1".into(), ..Default::default() }),
        placeholder_serial: Some("well-known placeholder serial (0123456789ABCDEF)".into()),
        ..Default::default()
    };
    let mut report = Report::new("test", Target { disk: Some(disk), ..Default::default() });
    report.identity_changes.push("Serial changed from 'AA11BB22CC33' to 'DD44EE55FF66'".into());
    report.identity_changes.push("Capacity changed from '1' to '2'".into());
    assert!(report.html().contains("AA11BB22CC33"));
    report.hide_serials();
    let json = serde_json::to_string(&report).unwrap();
    for out in [report.html(), report.evidence_html(), report.text(), json] {
        for secret in ["AA11BB22CC33", "DD44EE55FF66", "0123456789ABCDEF", "5EC2E7A1", "USBSTOR"] {
            assert!(!out.contains(secret), "{secret} left in output");
        }
    }
    assert!(report.html().contains("Serial numbers are hidden in this report."));
    assert!(report.evidence_html().contains("Serial numbers are hidden in this report."));
    assert!(report.warnings.iter().any(|w| w.contains("placeholder serial (hidden)")));
    assert_eq!(report.identity_changes, ["Serial changed", "Capacity changed from '1' to '2'"]);
    let back: Report = serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
    assert!(back.serials_hidden && back.html().contains("Serial numbers are hidden"));
}

#[test]
fn speed_chart_marks_positions_in_gb() {
    use flashtester::engine::MapPoint;
    let mut report = verified(8_000_000_000, 0);
    report.verify[0].stats.speed_map = (0..=80).map(|i| MapPoint { pos: i * 100_000_000, mb_s: 30.0, worst_ms: 1.0 }).collect();
    let html = report.html();
    assert!(html.contains("position (GB)"));
    for tick in [">0</text>", ">2</text>", ">4</text>", ">6</text>", ">8</text>"] {
        assert!(html.contains(tick), "missing tick {tick}");
    }
    // A run with no speed samples gets no position ticks.
    assert!(!verified(MIB, 0).html().contains("text-anchor=\"middle\">0</text>"));
}

#[test]
fn hidden_serials_cover_flash_captures_and_short_serials() {
    use flashtester::{flash::Identification, platform::DiskInfo};
    let saved = DiskInfo { serial: "SAVED9876XYZ".into(), instance_id: r"USB\VID_090C\SAVED9876XYZ".into(), ..Default::default() };
    let mut flash = Identification::new("imported_identity_response", None);
    flash.raw_response = b"..SAVED9876XYZ..".to_vec();
    flash.comparison_identity = Some(saved);
    let mut report = Report::new("decode-flash", Target::default());
    report.flash = Some(flash);
    report.hide_serials();
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("SAVED9876XYZ"));
    assert!(report.flash.as_ref().unwrap().raw_response.is_empty());

    let disk = DiskInfo { serial: "111".into(), placeholder_serial: Some("serial is one repeated character (111)".into()), ..Default::default() };
    let mut report = Report::new("info", Target { disk: Some(disk), ..Default::default() });
    report.verdict.details = vec!["Serial number: 111".into(), "Reported capacity: 1110 MB".into()];
    report.hide_serials();
    assert_eq!(report.verdict.details, ["Serial number: hidden", "Reported capacity: 1110 MB"]);
    assert!(report.warnings.iter().any(|w| w.contains("repeated character (hidden)")));
    assert!(!report.html().contains("(111)"));
}

#[test]
fn hidden_serials_remove_whole_device_ids_and_spare_short_numbers() {
    use flashtester::platform::DiskInfo;
    let id = r"USBSTOR\DISK&VEN_SANDISK&PROD_CRUZER\078155721234&0";
    let disk = DiskInfo { serial: "078155721234".into(), instance_id: id.into(), ..Default::default() };
    let mut report = Report::new("info", Target { disk: Some(disk), ..Default::default() });
    report.verdict.details = vec![format!("Device ID: {id}")];
    report.hide_serials();
    assert_eq!(report.verdict.details, ["Device ID: hidden"]);

    let disk = DiskInfo { serial: "1234".into(), ..Default::default() };
    let mut report = Report::new("info", Target { disk: Some(disk), ..Default::default() });
    report.verdict.details = vec!["Reported capacity: 12340 MB".into(), "Serial number: 1234".into()];
    report.hide_serials();
    assert_eq!(report.verdict.details, ["Reported capacity: 12340 MB", "Serial number: hidden"]);

    let mut report = Report::new("decode-flash", Target::default());
    report.hide_serials();
    assert!(report.html().contains("Serial numbers are hidden in this report."));
}

#[test]
fn hidden_serials_cover_quick_diagnostics() {
    use flashtester::platform::DiskInfo;
    let disk = DiskInfo { serial: "AA11BB22CC33".into(), ..Default::default() };
    let change = "Drive identity changed: Serial changed from 'AA11BB22CC33' to 'DD44EE55FF66'";
    let d = flashtester::diagnose::Diagnostics {
        reason: change.into(),
        warnings: vec!["Check the serial number AA11BB22CC33 with the seller.".into()],
        identity_changes: vec![change.into()],
        ..Default::default()
    };
    let mut report = flashtester::diagnose::report(&disk, d);
    report.target.disk.as_mut().unwrap().serial = "AA11BB22CC33".into();
    report.hide_serials();
    let json = serde_json::to_string(&report).unwrap();
    for out in [report.html(), report.evidence_html(), report.text(), json] {
        for secret in ["AA11BB22CC33", "DD44EE55FF66"] {
            assert!(!out.contains(secret), "{secret} left in output");
        }
    }
}
