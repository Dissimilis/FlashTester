use flashtester::{
    engine::{BadRange, Event, IoStats, Stop, Tally, VerifyResult},
    perf::{IopsResult, IopsRow},
    probe::ProbeResult,
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
        report.probe = Some(ProbeResult { finding: "genuine".into(), usable: 128 * MIB, claimed: 128 * MIB, ..Default::default() });
        assert_eq!(report.real_capacity(), None);
        let html = report.evidence_html();
        assert!(!html.contains("Estimated usable capacity"));
        assert!(!html.contains("Capacity that actually stores data"));
        assert!(html.contains("Space between samples is not verified"));
        assert!(!html.contains("Every 512-byte sector was written"));
    }
    let mut report = Report::new("probe", Target::default());
    report.probe = Some(ProbeResult { finding: "fake".into(), usable: 32 * MIB, claimed: 128 * MIB, ..Default::default() });
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
