use flashtester::{ctx::Ctx, fulltest, perf};
use std::path::Path;

fn directory_link(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        let result = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", "New-Item -ItemType Junction -Path $env:FT_TEST_LINK -Value $env:FT_TEST_TARGET -ErrorAction Stop | Out-Null"])
            .env("FT_TEST_LINK", link).env("FT_TEST_TARGET", target).output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    }
}

#[test]
fn cleanup_and_small_files_refuse_a_linked_parent() {
    let base = std::env::temp_dir().join(format!("flashtester-links-{}-{}", std::process::id(), flashtester::pattern::new_tag()));
    let root = base.join("drive");
    let outside = base.join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    for name in ["bench", "small", "probe"] {
        std::fs::create_dir(outside.join(name)).unwrap();
        std::fs::write(outside.join(name).join("sentinel"), b"keep me").unwrap();
    }
    let link = root.join("FlashTester");
    directory_link(&outside, &link);
    for name in ["bench", "small", "probe"] {
        assert!(fulltest::remove_data_dir(&link.join(name)).is_err());
        assert!(fulltest::make_dir(&link.join(name)).is_err());
    }
    let result = perf::run_small_files(&link.join("small"), 1, &Ctx::new());
    assert!(!result.ok());
    for name in ["bench", "small", "probe"] {
        assert_eq!(std::fs::read(outside.join(name).join("sentinel")).unwrap(), b"keep me");
    }
    // Remove the link itself before recursively removing our temporary fixtures.
    #[cfg(windows)]
    std::fs::remove_dir(&link).unwrap();
    #[cfg(unix)]
    std::fs::remove_file(&link).unwrap();
    std::fs::remove_dir_all(base).unwrap();
}
