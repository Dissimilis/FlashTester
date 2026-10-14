//! Command-line front-end. Every function of the app is reachable from here.

use crate::app::{self, FixSize, Job};
use crate::ctx::Ctx;
use crate::fulltest::{TestOpts, Where};
use crate::platform;
use crate::report::{Report, Status, size};
use clap::{Args, Parser, Subcommand};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "flashtester", version, about = "Test flash drives (and other disks) for fake capacity, data loss and speed.", after_help = AFTER)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Print the result as JSON on stdout (progress still goes to stderr).
    #[arg(long, global = true)]
    json: bool,
    /// Also save the result: .json for JSON, anything else for an HTML report.
    #[arg(long, global = true, value_name = "FILE")]
    report: Option<PathBuf>,
    /// Also save a one-page HTML evidence sheet (for a refund claim).
    #[arg(long, global = true, value_name = "FILE")]
    evidence: Option<PathBuf>,
    /// No progress output.
    #[arg(long, short, global = true)]
    quiet: bool,
}

macro_rules! after {
    ($gui:expr) => {
        concat!(
            "Targets: a drive letter or folder (file mode, non-destructive, e.g. E:) or --disk N (raw mode, whole disk).
The disk the running OS boots from can never be tested. Raw-mode writes need --destroy and an elevated terminal.
Sizes: 500M, 8G, 1T (decimal) or 8GiB (binary), or plain bytes.
Exit codes: 0 OK, 1 problem found (fake/defective/read-only/unreliable), 2 refused, 3 error, 130 cancelled.",
            $gui
        )
    };
}
#[cfg(any(windows, target_os = "linux"))]
const AFTER: &str = after!("
Run without arguments, or with `gui`, for the GUI.");
#[cfg(not(any(windows, target_os = "linux")))]
const AFTER: &str = after!("");

/// What `fix` formats with unless told otherwise: FreeBSD's base system has no exFAT.
const DEFAULT_FS: &str = if cfg!(target_os = "freebsd") { "fat32" } else { "exfat" };

#[derive(Args, Clone)]
struct TargetArg {
    /// Drive letter or folder, e.g. E: (file mode).
    path: Option<String>,
    /// Disk from `flashtester list` (raw mode): a number on Windows, sdb / nvme1n1 on Linux, da0 / ada1 on FreeBSD.
    #[arg(long, conflicts_with = "path")]
    disk: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the GUI (the default without arguments; on Linux it needs an X11 or XWayland desktop).
    #[cfg(any(windows, target_os = "linux"))]
    Gui {
        /// X display and cookie file for the root copy the GUI starts through pkexec,
        /// which clears the environment.
        #[arg(long, hide = true)]
        display: Option<String>,
        #[arg(long, hide = true)]
        xauthority: Option<PathBuf>,
    },
    /// List disks and volumes, with identity, link speed and whether they may be tested.
    List,
    /// Show the identity of a drive: vendor, model, serial, USB ID, link speed.
    Info(TargetArg),
    /// Show the drive's own SMART counters (read-only; works on the system disk too).
    Smart(TargetArg),
    /// Full test: fill with a seeded pattern, then read everything back (h2testw-style).
    Test {
        #[command(flatten)]
        t: TargetArg,
        #[command(flatten)]
        o: TestArgs,
        /// Number of write+verify passes (0 = until an error or Ctrl+C).
        #[arg(long, default_value_t = 1)]
        passes: u32,
    },
    /// Endurance: repeat write+verify with a new seed each pass.
    Endurance {
        #[command(flatten)]
        t: TargetArg,
        #[command(flatten)]
        o: TestArgs,
        /// Number of passes (0 = until an error or Ctrl+C).
        #[arg(long, default_value_t = 0)]
        passes: u32,
    },
    /// Verify existing test data (FlashTester or h2testw), optionally in a loop.
    Verify {
        #[command(flatten)]
        t: TargetArg,
        /// Verify h2testw's 1.h2w, 2.h2w, ... files in the folder.
        #[arg(long)]
        h2w: bool,
        /// Manifest to use (default: the one on the drive, then the local copy).
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Read the data this many times (0 = until an error or Ctrl+C).
        #[arg(long, default_value_t = 1)]
        loops: u32,
    },
    /// Quick fake-capacity probe: minutes instead of hours.
    /// With --disk: raw, destructive. With a drive letter: inside free space (admin).
    Probe {
        #[command(flatten)]
        t: TargetArg,
        /// Confirm that the whole disk may be overwritten (raw mode).
        #[arg(long)]
        destroy: bool,
    },
    /// Read-only surface scan of a whole disk: bad and slow regions.
    Scan {
        #[arg(long)]
        disk: String,
        /// Scan only the first part of the disk (e.g. 50G).
        #[arg(long)]
        size: Option<String>,
    },
    /// Random 4K I/O at queue depth 1 and 32.
    Iops {
        #[command(flatten)]
        t: TargetArg,
        /// Seconds per run.
        #[arg(long, default_value_t = 8.0)]
        seconds: f64,
        /// Also random writes (always in file mode; on a raw disk needs --destroy).
        #[arg(long)]
        write: bool,
        #[arg(long)]
        destroy: bool,
    },
    /// Sequential read/write speed (speed only: does not check the capacity).
    Bench {
        #[command(flatten)]
        t: TargetArg,
        /// Amount of data to write and read.
        #[arg(long, default_value = "1GiB")]
        size: String,
        /// On a raw disk also write (destroys data at the start of the disk; needs --destroy).
        #[arg(long)]
        write: bool,
        #[arg(long)]
        destroy: bool,
    },
    /// Many small files: create, rename, delete, verify (file-system integrity, files/s).
    SmallFiles {
        path: String,
        #[arg(long, default_value_t = 5000, value_parser = clap::value_parser!(u64).range(10..))]
        count: u64,
        /// Keep the files afterwards.
        #[arg(long)]
        keep: bool,
    },
    /// Guided test: pull the drive out while it writes, then check flushed data survived.
    PlugPull {
        path: String,
        /// Flush interval.
        #[arg(long, default_value = "256M")]
        checkpoint: String,
    },
    /// Repartition and format a drive, to its real capacity if it is fake.
    Fix {
        #[arg(long)]
        disk: String,
        /// Partition size: bytes/size, "probe" to measure it first, or "full".
        #[arg(long, default_value = "probe")]
        size: String,
        #[arg(long, default_value = DEFAULT_FS)]
        fs: String,
        #[arg(long, default_value = "FLASH")]
        label: String,
        /// Confirm that the whole disk may be overwritten.
        #[arg(long)]
        destroy: bool,
    },
    /// Delete FlashTester test data from a drive.
    Clean { path: String },
    /// Render a saved JSON result as an HTML report or evidence sheet.
    Render {
        /// A result saved with --report file.json
        input: PathBuf,
        #[arg(long)]
        html: Option<PathBuf>,
        #[arg(long = "evidence-out")]
        evidence_out: Option<PathBuf>,
    },
}

#[derive(Args, Clone)]
struct TestArgs {
    /// Test only this much (default: all free space / the whole disk).
    #[arg(long)]
    size: Option<String>,
    /// I/O block size.
    #[arg(long, default_value = "8MiB")]
    block: String,
    /// Don't re-read earlier data while writing (no early abort on fakes).
    #[arg(long)]
    no_early_abort: bool,
    /// Ask to unplug and replug the drive between writing and verifying.
    #[arg(long)]
    replug: bool,
    /// Continue with further passes after errors.
    #[arg(long)]
    keep_going: bool,
    /// Delete the test data after a successful test.
    #[arg(long)]
    cleanup: bool,
    /// Confirm that the whole disk may be overwritten (raw mode).
    #[arg(long)]
    destroy: bool,
}

pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim().to_ascii_uppercase();
    let (num, mul) = match t.find(|c: char| c.is_ascii_alphabetic()) {
        None => (t.as_str(), 1u64),
        Some(i) => {
            let m = match &t[i..] {
                "K" | "KB" => 1_000,
                "M" | "MB" => 1_000_000,
                "G" | "GB" => 1_000_000_000,
                "T" | "TB" => 1_000_000_000_000,
                "KIB" => 1 << 10,
                "MIB" => 1 << 20,
                "GIB" => 1 << 30,
                "TIB" => 1 << 40,
                "B" => 1,
                u => return Err(format!("unknown size unit {u}")),
            };
            (t[..i].trim(), m)
        }
    };
    let v: f64 = num.parse().map_err(|_| format!("bad size {s}"))?;
    Ok((v * mul as f64) as u64)
}

fn target(t: &TargetArg) -> Result<Where, String> {
    match (&t.path, t.disk.as_deref()) {
        (Some(p), None) => app::resolve_path(p),
        (None, Some(n)) => app::resolve_disk(n),
        _ => Err("Give a drive letter/folder or --disk N.".into()),
    }
}

fn test_opts(o: &TestArgs, passes: u32) -> Result<TestOpts, String> {
    let block = parse_size(&o.block)? as usize;
    if !block.is_power_of_two() || !(1 << 20..=64 << 20).contains(&block) {
        return Err("--block must be a power of two between 1MiB and 64MiB".into());
    }
    Ok(TestOpts {
        size: o.size.as_deref().map(parse_size).transpose()?,
        block,
        early_abort: !o.no_early_abort,
        replug: o.replug,
        passes,
        stop_on_error: !o.keep_going,
        keep: !o.cleanup,
    })
}

fn refuse(msg: impl Into<String>) -> Report {
    let mut r = Report::new("", Default::default());
    r.set(Status::Refused, msg);
    r
}

/// Prints progress and log lines to stderr until `done` is set.
fn progress_thread(ctx: Ctx, quiet: bool, done: std::sync::Arc<std::sync::atomic::AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut shown = 0;
        let mut last_len = 0;
        let mut err = std::io::stderr();
        loop {
            let fin = done.load(std::sync::atomic::Ordering::SeqCst);
            let new = ctx.log_since(shown);
            if !new.is_empty() {
                shown += new.len();
                // -q hides progress, never a request to act (unplug, replug, pull).
                let _ = write!(err, "\r{}\r", " ".repeat(last_len));
                for l in new.iter().filter(|l| !quiet || l.starts_with(crate::ctx::ACTION)) {
                    let _ = writeln!(err, "{l}");
                }
            }
            if fin {
                if !quiet && last_len > 0 {
                    let _ = write!(err, "\r{}\r", " ".repeat(last_len));
                }
                return;
            }
            let s = ctx.status();
            if !quiet && !s.phase.is_empty() && s.prompt.is_none() {
                let el = s.phase_start.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
                let line = if s.total > 0 {
                    let pct = s.done as f64 * 100.0 / s.total as f64;
                    let big = s.total > 100_000;
                    let rate = if el > 0.5 && big { format!("  {:.1} MB/s", s.done as f64 / 1e6 / el) } else { String::new() };
                    let eta = if s.done > 0 && el > 2.0 {
                        format!("  ETA {}", crate::report::duration(el * (s.total - s.done.min(s.total)) as f64 / s.done as f64))
                    } else {
                        String::new()
                    };
                    let amount = if big { format!("{} / {}", size(s.done), size(s.total)) } else { format!("{} / {}", s.done, s.total) };
                    format!("{}: {pct:5.1}%  {amount}{rate}{eta}", s.phase)
                } else {
                    format!("{}  {}", s.phase, crate::report::duration(el))
                };
                let _ = write!(err, "\r{line:<last_len$}");
                last_len = line.len().max(last_len.min(line.len() + 10));
                let _ = err.flush();
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    })
}

fn list(json: bool) -> i32 {
    let disks = platform::list_disks();
    if json {
        println!("{}", serde_json::to_string_pretty(&disks).unwrap());
        return 0;
    }
    if !platform::is_admin() {
        println!("(not elevated: raw-disk commands need an administrator terminal)\n");
    }
    println!("{:<8} {:<10} {:<6} {:<28} {:<34} {:<12} Status", "Disk", "Size", "Bus", "Name", "Link", "Volumes");
    for d in &disks {
        let link = d.usb.as_ref().map(|u| format!("{}:{} {}", u.vid, u.pid, u.link.split(" (").next().unwrap_or(""))).unwrap_or_default();
        let status = match &d.protected {
            Some(p) => format!("PROTECTED: {p}"),
            None => "can be tested".into(),
        };
        println!("{:<8} {:<10} {:<6} {:<28} {:<34} {:<12} {}", d.id, size(d.size), d.bus, d.name(), link, d.letters(), status);
    }
    0
}

fn smart(t: &TargetArg, json: bool) -> i32 {
    let d = match disk_of(t) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if !platform::is_admin() {
        eprintln!("Reading SMART data needs {}.", if cfg!(windows) { "administrator rights: run from an elevated terminal" } else { "root" });
        return 2;
    }
    let s = match crate::smart::read(&d) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Disk {} ({}): no SMART data. {e}", d.id, d.name());
            return 3;
        }
    };
    let code = if s.alarms.is_empty() { 0 } else { 1 };
    if json {
        println!("{}", serde_json::to_string_pretty(&s).unwrap());
        return code;
    }
    println!("Disk {}: {}, serial {}, firmware {} (read via {})", d.id, s.model, s.serial, s.firmware, s.via);
    println!("Values as the drive's firmware reports them; FlashTester cannot check them.\n");
    for a in &s.alarms {
        println!("  ! {a}");
    }
    // The terminal may not be UTF-8 (serial consoles, Windows code pages): "41 C".
    let ascii = |v: &str| v.replace('°', "");
    for k in &s.key {
        println!("  {:<42} {}", k.what.label(), ascii(&k.what.show(k.value)));
    }
    println!("\nAll counters:");
    if s.raw.iter().any(|r| !r.worst.is_empty()) {
        println!("  {:>3}  {:<44} {:>5} {:>5} {:>6}  Raw", "ID", "Attribute", "Value", "Worst", "Thresh");
        for r in &s.raw {
            println!("  {:>3}  {:<44} {:>5} {:>5} {:>6}  {}", r.id, r.name, r.value, r.worst, r.thresh, r.raw);
        }
    } else {
        for r in &s.raw {
            println!("  {:<44} {}", r.name, ascii(&r.value));
        }
    }
    code
}

/// The disk a drive letter / folder is on, or the disk given with --disk.
fn disk_of(t: &TargetArg) -> Result<platform::DiskInfo, String> {
    match (&t.path, t.disk.as_deref()) {
        (Some(p), None) => {
            let mut s = p.clone();
            if cfg!(windows) && s.len() == 2 {
                s.push('\\');
            }
            match platform::volume_of_path(std::path::Path::new(&s)) {
                Ok(v) => match v.extents.first() {
                    Some(e) => platform::disk(&e.0).map_err(|e| e.to_string()),
                    None => Err("volume is not on a local disk".into()),
                },
                Err(e) => Err(e.to_string()),
            }
        }
        (None, Some(n)) => platform::disk(n).map_err(|e| e.to_string()),
        _ => Err("Give a drive letter or --disk N.".into()),
    }
}

fn info(t: &TargetArg, json: bool) -> i32 {
    let d = match disk_of(t) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&d).unwrap());
        return 0;
    }
    println!("Disk {}: {} ({})", d.id, d.name(), d.path);
    println!("  Vendor / product / rev: {} / {} / {}", d.vendor, d.product, d.revision);
    println!("  Serial:   {}", if d.serial.is_empty() { "(none)" } else { &d.serial });
    println!("  Capacity: {} ({} bytes, {}-byte sectors)", size(d.size), d.size, d.sector_size);
    println!("  Bus:      {}{}", d.bus, if d.removable { ", removable" } else { "" });
    if let Some(u) = &d.usb {
        println!("  USB:      VID {} PID {}, device reports USB {}", u.vid, u.pid, u.usb_version);
        let note = if u.slow_link && u.superspeed_capable == Some(true) {
            "  <- USB 3 drive running at USB 2 speed"
        } else if u.superspeed_plus_capable == Some(true) && !u.link.contains("SuperSpeed+") {
            "  <- 10 Gbit/s drive running at 5 Gbit/s"
        } else {
            ""
        };
        println!("  Link:     {}{note}", u.link);
        println!("  USB serial: {}", u.serial.as_deref().unwrap_or("(none)"));
    }
    println!("  Device:   {}", d.instance_id);
    for v in &d.volumes {
        // Unmounted: name the partition (/dev/sdb1; on Windows the GUID path says nothing useful).
        let (place, free) = if v.mount_points.is_empty() {
            (if cfg!(windows) { String::new() } else { v.guid.clone() }, "not mounted".to_string())
        } else {
            (v.mount_points.join(" "), format!("{} free", size(v.free)))
        };
        println!("  Volume:   {place} {} \"{}\" {} ({free})", v.fs, v.label, size(v.size));
    }
    if let Some(p) = &d.placeholder_serial {
        println!("  ! Suspicious identity: {p}");
    }
    match &d.protected {
        Some(p) => println!("  PROTECTED: {p} - cannot be tested."),
        None => println!("  Can be tested."),
    }
    0
}

fn emit(rep: &Report, cli: &Cli) -> i32 {
    if cli.json {
        println!("{}", serde_json::to_string_pretty(rep).unwrap());
    } else {
        print!("{}", rep.text());
    }
    if let Some(p) = &cli.report {
        let is_json = p.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"));
        let body = if is_json { serde_json::to_string_pretty(rep).unwrap() } else { rep.html() };
        match std::fs::write(p, body) {
            Ok(()) => eprintln!("Report saved to {}", p.display()),
            Err(e) => eprintln!("Cannot save report {}: {e}", p.display()),
        }
    }
    if let Some(p) = &cli.evidence {
        match std::fs::write(p, rep.evidence_html()) {
            Ok(()) => eprintln!("Evidence sheet saved to {}", p.display()),
            Err(e) => eprintln!("Cannot save evidence {}: {e}", p.display()),
        }
    }
    rep.exit_code()
}

fn need_destroy(destroy: bool, w: &Where) -> Result<(), String> {
    if let Where::Disk(d) = w
        && !destroy {
            let vols = if d.volumes.is_empty() { String::new() } else { format!(" including {}", d.letters()) };
            return Err(format!(
                "This overwrites ALL data on disk {} ({}, {}){vols}. Add --destroy to confirm.",
                d.id,
                d.name(),
                size(d.size)
            ));
        }
    Ok(())
}

fn build_job(cmd: &Cmd) -> Result<Job, String> {
    Ok(match cmd {
        Cmd::Test { t, o, passes } | Cmd::Endurance { t, o, passes } => {
            let w = target(t)?;
            need_destroy(o.destroy, &w)?;
            Job::Test { w, o: test_opts(o, *passes)? }
        }
        Cmd::Verify { t, h2w, manifest, loops } => Job::Verify { w: target(t)?, h2w: *h2w, manifest: manifest.clone(), loops: *loops },
        Cmd::Probe { t, destroy } => {
            let w = target(t)?;
            need_destroy(*destroy, &w)?;
            Job::Probe { w }
        }
        Cmd::Scan { disk, size } => Job::Scan { w: app::resolve_disk(disk)?, size: size.as_deref().map(parse_size).transpose()? },
        Cmd::Iops { t, seconds, write, destroy } => {
            let w = target(t)?;
            if *write {
                need_destroy(*destroy, &w)?;
            }
            Job::Iops { w, secs: *seconds, write: *write }
        }
        Cmd::Bench { t, size, write, destroy } => {
            let w = target(t)?;
            if *write {
                need_destroy(*destroy, &w)?;
            }
            Job::Bench { w, size: parse_size(size)?, write: *write }
        }
        Cmd::SmallFiles { path, count, keep } => Job::SmallFiles { w: app::resolve_path(path)?, count: *count, keep: *keep },
        Cmd::PlugPull { path, checkpoint } => Job::PlugPull { w: app::resolve_path(path)?, checkpoint: parse_size(checkpoint)?.max(1 << 20) },
        Cmd::Fix { disk, size, fs, label, destroy } => {
            let w = app::resolve_disk(disk)?;
            need_destroy(*destroy, &w)?;
            let sz = match size.to_ascii_lowercase().as_str() {
                "probe" => FixSize::Probe,
                "full" | "whole" => FixSize::Whole,
                s => FixSize::Bytes(parse_size(s)?),
            };
            let Where::Disk(d) = w else { unreachable!() };
            Job::Fix { disk: d, size: sz, fs: fs.clone(), label: label.clone() }
        }
        _ => unreachable!(),
    })
}

pub fn main() -> i32 {
    let cli = Cli::parse();
    match &cli.cmd {
        #[cfg(any(windows, target_os = "linux"))]
        Cmd::Gui { display, xauthority } => {
            // SAFETY: no other threads exist yet.
            unsafe {
                if let Some(d) = display {
                    std::env::set_var("DISPLAY", d);
                }
                if let Some(x) = xauthority {
                    std::env::set_var("XAUTHORITY", x);
                }
            }
            return crate::gui::run();
        }
        Cmd::List => return list(cli.json),
        Cmd::Info(t) => return info(t, cli.json),
        Cmd::Smart(t) => return smart(t, cli.json),
        Cmd::Clean { path } => {
            let w = match app::resolve_path(path) {
                Ok(w) => w,
                Err(e) => return emit(&refuse(e), &cli),
            };
            let Where::Dir { root, .. } = &w else { unreachable!() };
            return match crate::fulltest::clean(root) {
                Ok((n, b)) => {
                    println!("Removed {n} FlashTester items ({}) from {}", size(b), root.display());
                    0
                }
                Err(e) => {
                    eprintln!("Clean failed: {e}");
                    3
                }
            };
        }
        Cmd::Render { input, html, evidence_out } => {
            let rep: Report = match std::fs::read(input).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string())) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("{}: {e}", input.display());
                    return 3;
                }
            };
            if let Some(h) = html {
                let _ = std::fs::write(h, rep.html());
            }
            if let Some(h) = evidence_out {
                let _ = std::fs::write(h, rep.evidence_html());
            }
            if html.is_none() && evidence_out.is_none() {
                print!("{}", rep.text());
            }
            return 0;
        }
        _ => {}
    }
    let job = match build_job(&cli.cmd) {
        Ok(j) => j,
        Err(e) => return emit(&refuse(e), &cli),
    };
    let ctx = Ctx::new();
    {
        let c = ctx.clone();
        let _ = ctrlc::set_handler(move || {
            if c.cancelled() {
                std::process::exit(130);
            }
            eprintln!("\nCancelling... (Ctrl+C again to quit at once)");
            c.cancel();
        });
    }
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let h = progress_thread(ctx.clone(), cli.quiet, done.clone());
    let rep = app::run(&job, &ctx);
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = h.join();
    emit(&rep, &cli)
}

#[cfg(test)]
mod tests {
    use super::{Cli, parse_size};
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("8G").unwrap(), 8_000_000_000);
        assert_eq!(parse_size("8GiB").unwrap(), 8 << 30);
        assert_eq!(parse_size("1.5MB").unwrap(), 1_500_000);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert!(parse_size("3X").is_err());
    }
}
