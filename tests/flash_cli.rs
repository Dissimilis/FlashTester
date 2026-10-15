use std::{fs, process::Command};

#[test]
fn refused_identification_keeps_offline_evidence() {
    let dir = std::env::temp_dir().join(format!("flashtester-offline-errors-{}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    let evidence = dir.join("evidence.html");
    let missing = dir.join("missing.bin");
    for args in [vec!["flash-id", "zz"], vec!["decode-flash", missing.to_str().unwrap(), "--nand-layout", "phison"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_flashtester"))
            .args(args)
            .arg("--json")
            .arg("--evidence")
            .arg(&evidence)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{:?}", output);
        let report: flashtester::report::Report = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report.command, "identify");
        // The saved report must also retain the context when rendered later.
        for html in [fs::read_to_string(&evidence).unwrap(), report.evidence_html()] {
            assert!(html.contains("REFUSED"));
            assert!(html.contains("Flash identification evidence"));
            assert!(!html.contains("Every 512-byte sector was written"));
            assert!(!html.contains("Claimed capacity"));
        }
    }
    fs::remove_file(evidence).unwrap();
    fs::remove_dir(dir).unwrap();
}
#[test]
fn failed_report_or_evidence_save_returns_error() {
    let path = std::env::temp_dir().join(format!("flashtester-save-error-{}-{}", std::process::id(), flashtester::pattern::new_tag()));
    std::fs::create_dir(&path).unwrap();
    for option in ["--report", "--evidence"] {
        // An existing directory cannot be overwritten by a report file.
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_flashtester"))
            .args(["flash-id", "AD D3 10 A6", option]).arg(&path).output().unwrap();
        assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(String::from_utf8_lossy(&out.stderr).contains("Cannot save"));
    }
    std::fs::remove_dir(path).unwrap();
}
