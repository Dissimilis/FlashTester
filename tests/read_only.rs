use flashtester::{app::{self, Job}, fulltest::{TestOpts, Where}, platform::{DiskInfo, VolumeInfo}, report::{Report, Status}};

fn disk() -> DiskInfo {
    DiskInfo { id: "fixture".into(), path: "fixture-device".into(), instance_id: "device-a".into(),
        serial: "serial-a".into(), size: 16 << 20, sector_size: 512,
        protected: Some("operating system".into()), ..Default::default() }
}

#[test]
fn system_disk_permission_is_an_explicit_read_allowlist() {
    let w = Where::Disk(disk());
    assert!(Job::Scan { w: w.clone(), size: None }.reads_disk_only());
    assert!(Job::Bench { w: w.clone(), size: 1 << 20, write: false }.reads_disk_only());
    for job in [
        Job::Bench { w: w.clone(), size: 1 << 20, write: true },
        Job::Probe { w: w.clone() },
        Job::Test { w: w.clone(), o: TestOpts::default() },
        Job::Iops { w: w.clone(), secs: 1.0, write: false },
        Job::Verify { w, h2w: false, manifest: None, loops: 1 },
        Job::Fix { disk: disk(), size: app::FixSize::Whole, fs: "exfat".into(), label: "TEST".into() },
        Job::Bench { w: Where::Dir { root: "fixture".into(), vol: VolumeInfo::default(), disk: Some(disk()) }, size: 1 << 20, write: false },
    ] {
        assert!(!job.reads_disk_only());
    }
}

#[test]
fn stale_or_redirected_disk_identity_is_refused() {
    let d = disk();
    assert!(d.check_identity(&d.clone()).is_ok());
    for changed in [
        DiskInfo { path: "other-device".into(), ..d.clone() },
        DiskInfo { id: "other".into(), ..d.clone() },
        DiskInfo { instance_id: "device-b".into(), ..d.clone() },
        DiskInfo { serial: "serial-b".into(), ..d.clone() },
        DiskInfo { size: d.size * 2, ..d.clone() },
        DiskInfo { sector_size: 4096, ..d.clone() },
    ] {
        assert!(d.check_identity(&changed).is_err());
    }
}

#[test]
fn identification_and_failed_read_reports_never_claim_written_patterns() {
    let identity = app::identification_report(&disk());
    assert!(identity.text().contains("No storage test was run"));
    assert!(identity.html().contains("device-a"));
    assert!(identity.evidence_html().contains("Reported capacity has not been verified"));
    assert!(!identity.evidence_html().contains("sector was written"));
    for command in ["scan", "bench"] {
        for status in [Status::Error, Status::Cancelled, Status::Info] {
            let mut rep = Report::new(command, Where::Disk(disk()).target());
            rep.read_only = true;
            rep.set(status, "Fixture result");
            let saved = serde_json::to_string(&rep).unwrap();
            let loaded: Report = serde_json::from_str(&saved).unwrap();
            assert!(loaded.evidence_html().contains("No test data is written"));
            assert!(!loaded.evidence_html().contains("sector was written"));
        }
    }
}

#[test]
fn refused_read_job_keeps_identity_and_read_only_evidence() {
    for job in [Job::Scan { w: Where::Disk(disk()), size: None }, Job::Bench { w: Where::Disk(disk()), size: 1 << 20, write: false }] {
        // The fixture ID cannot resolve to a real disk; run must stop before I/O.
        let rep = app::run(&job, &flashtester::ctx::Ctx::new());
        assert_eq!(rep.verdict.status, Status::Refused);
        assert!(rep.read_only);
        assert!(!rep.command.is_empty());
        assert_eq!(rep.target.disk.as_ref().unwrap().instance_id, "device-a");
        assert!(!rep.evidence_html().contains("sector was written"));
        assert!(rep.evidence_html().contains("No test data is written"));
    }
}

#[test]
fn cli_missing_disk_evidence_does_not_claim_a_write_test() {
    let path = std::env::temp_dir().join(format!("flashtester-refused-read-{}.html", std::process::id()));
    for command in ["scan", "bench"] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_flashtester"))
            .args([command, "--disk", "nonexistent-fixture-device", "--evidence"])
            .arg(&path).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        let evidence = std::fs::read_to_string(&path).unwrap();
        assert!(evidence.contains("refused before testing"));
        assert!(!evidence.contains("sector was written"));
    }
    std::fs::remove_file(path).unwrap();
}
