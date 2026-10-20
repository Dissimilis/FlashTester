//! Command-line front-end. Every function of the app is reachable from here.

use crate::app::{self, Job};
use crate::ctx::Ctx;
use crate::fulltest::{TestOpts, Where};
use crate::platform;
use crate::report::{Report, Status, size};
use clap::{Args, Parser, Subcommand};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "flashtester", version, about = "Check drive capacity, test for data loss, and measure speed.", after_help = AFTER)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Print the result as JSON on stdout (progress still goes to stderr).
    #[arg(long, global = true)]
    json: bool,
    /// Save the result as JSON (.json) or HTML (any other extension).
    #[arg(long, global = true, value_name = "FILE")]
    report: Option<PathBuf>,
    /// Save a one-page HTML summary of the evidence.
    #[arg(long, global = true, value_name = "FILE")]
    evidence: Option<PathBuf>,
    /// Leave the drive's serial numbers out of the result: printed, --report, --evidence and render.
    #[arg(long, global = true)]
    hide_serial: bool,
    /// Hide progress output. Action prompts are still shown.
    #[arg(long, short, global = true)]
    quiet: bool,
}

macro_rules! after {
    ($gui:expr) => {
        concat!(
            "Targets: a drive letter or folder (file mode, e.g. E:) or --disk N (raw mode, whole disk).
File mode writes test files into free space. On a fake drive, these writes can overwrite existing files. Back up the drive first.
System disks allow info, scan and read-only bench --disk. Writes and file-mode tests are blocked. Raw-mode writes erase data and require --destroy and administrator or root access.
Sizes: 500M, 8G, 1T (decimal) or 8GiB (binary), or plain bytes.
Exit codes: 0 OK, 1 problem found (fake/defective/read-only/unreliable), 2 refused, 3 error, 130 cancelled.",
            $gui
        )
    };
}
#[cfg(any(windows, target_os = "linux", target_os = "freebsd"))]
const AFTER: &str = after!("
Run without arguments, or with `gui`, for the GUI.");
#[cfg(not(any(windows, target_os = "linux", target_os = "freebsd")))]
const AFTER: &str = after!("");

/// What `fix` formats with unless told otherwise.
const DEFAULT_FS: &str = crate::fix::FILESYSTEMS[0];

#[derive(Args, Clone)]
struct TargetArg {
    /// Drive letter or folder, e.g. E: (file mode).
    path: Option<String>,
    /// Disk from `flashtester list` (raw mode): a number on Windows, sdb / nvme1n1 on Linux, da0 / ada1 on FreeBSD, usb/0/0 on Haiku.
    #[arg(long, conflicts_with = "path")]
    disk: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Back up files to a ZIP archive, or a whole disk to an .img and .img.json.
    Backup {
        #[command(flatten)]
        t: TargetArg,
        /// New backup file, on another physical disk. Existing files are refused.
        #[arg(long)]
        output: PathBuf,
    },
    /// Check and restore a FlashTester ZIP backup or whole-disk image.
    Restore {
        /// ZIP backup or .img file (keep its .img.json alongside it).
        backup: PathBuf,
        /// New folder on a non-system disk for a file backup.
        #[arg(long, conflicts_with = "disk", required_unless_present = "disk")]
        to: Option<PathBuf>,
        /// Destination disk for an image. All existing data will be overwritten.
        #[arg(long, conflicts_with = "to")]
        disk: Option<String>,
        /// Confirm overwriting the destination disk.
        #[arg(long)]
        destroy: bool,
    },
    /// Start the GUI (the default without arguments; on Linux and FreeBSD it needs an X11 or XWayland desktop).
    #[cfg(any(windows, target_os = "linux", target_os = "freebsd"))]
    Gui {
        /// X display and cookie file for the root copy the GUI starts through pkexec,
        /// which clears the environment.
        #[arg(long, hide = true)]
        display: Option<String>,
        #[arg(long, hide = true)]
        xauthority: Option<PathBuf>,
    },
    /// Internal: a root GUI saves the desktop user's reports through this, run as that user.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    #[command(hide = true)]
    WriteNewFile { path: PathBuf },
    /// List disks and volumes, with identity, link speed and whether they may be tested.
    List,
    /// Show the identity of a drive: vendor, model, serial, USB ID, link speed.
    Info(TargetArg),
    /// Show the drive's own SMART counters (read-only; works on the system disk too).
    Smart(TargetArg),
    /// Look up four or six NAND ID bytes offline; returns all matching parts.
    FlashId { id: String },
    /// List the recovered layouts supported by offline identity decoding.
    FlashLayouts,
    /// Decode a saved binary reply offline. Does not query or open a drive.
    #[command(group(clap::ArgGroup::new("layout").required(true).args(["nand_layout", "identity_layout", "smi_model"])))]
    DecodeFlash {
        input: PathBuf,
        #[arg(long, value_enum)]
        nand_layout: Option<crate::flash::Family>,
        /// Layout ID from flash-layouts, e.g. 0x463c20 for Phison.
        #[arg(long)]
        identity_layout: Option<String>,
        /// Decode an SMI model discovery reply (SM3 marker).
        #[arg(long)]
        smi_model: bool,
        /// Compare to a saved `info --json` file; only for identity replies.
        #[arg(long, requires = "identity_layout", conflicts_with_all = ["nand_layout", "smi_model"])]
        device_info: Option<PathBuf>,
    },
    /// Write test data, then read it back and check for errors.
    Test {
        #[command(flatten)]
        t: TargetArg,
        #[command(flatten)]
        o: TestArgs,
        /// Number of write+verify passes (0 = until an error or Ctrl+C).
        #[arg(long, default_value_t = 1)]
        passes: u32,
    },
    /// Repeat the write and read-back test with new data each pass.
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
    /// Check for false capacity by testing sampled positions across the drive.
    /// With --disk, writes erase data. With a volume, writes use free space but can overwrite files on a fake drive. Requires administrator or root access.
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
    /// Create, rename and delete small files, then check their contents and measure speed.
    SmallFiles {
        path: String,
        #[arg(long, default_value_t = 5000, value_parser = clap::value_parser!(u64).range(10..))]
        count: u64,
        /// Keep the files afterwards.
        #[arg(long)]
        keep: bool,
    },
    /// Unplug the drive during a write, then check the data confirmed as saved.
    PlugPull {
        path: String,
        /// Flush interval.
        #[arg(long, default_value = "256M")]
        checkpoint: String,
    },
    /// Erase and format a drive, using its estimated usable capacity if it is fake.
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
    /// Ask to disconnect and reconnect the drive before checking the written data.
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
    let bytes = v * mul as f64;
    if !bytes.is_finite() || bytes < 1.0 || bytes >= u64::MAX as f64 {
        return Err(format!("size must be at least one byte and fit in 64 bits: {s}"));
    }
    Ok(bytes as u64)
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
        println!("(Whole-disk commands require administrator or root access.)\n");
    }
    let w = disks.iter().map(|d| d.id.len()).max().unwrap_or(0).max(8);
    println!("{:<w$} {:<10} {:<6} {:<28} {:<34} {:<12} Status", "Disk", "Size", "Bus", "Name", "Link", "Volumes");
    for d in &disks {
        let link = d.usb.as_ref().map(|u| format!("{}:{} {}", u.vid, u.pid, u.link.split(" (").next().unwrap_or(""))).unwrap_or_default();
        let status = match &d.protected {
            Some(p) => format!("PROTECTED: {p}"),
            None => "can be tested".into(),
        };
        println!("{:<w$} {:<10} {:<6} {:<28} {:<34} {:<12} {}", d.id, size(d.size), d.bus, d.name(), link, d.letters(), status);
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
    let w = s.raw.iter().map(|r| r.name.chars().count()).max().unwrap_or(0).max(9);
    if s.raw.iter().any(|r| !r.worst.is_empty()) {
        println!("  {:>3}  {:<w$} {:>5} {:>5} {:>6}  Raw", "ID", "Attribute", "Value", "Worst", "Thresh");
        for r in &s.raw {
            println!("  {:>3}  {:<w$} {:>5} {:>5} {:>6}  {}", r.id, r.name, r.value, r.worst, r.thresh, r.raw);
        }
    } else {
        for r in &s.raw {
            println!("  {:<w$} {}", r.name, ascii(&r.value));
        }
    }
    code
}

/// The disk a drive letter / folder is on, or the disk given with --disk.
fn disk_of(t: &TargetArg) -> Result<platform::DiskInfo, String> {
    match (&t.path, t.disk.as_deref()) {
        (Some(p), None) => {
            match platform::volume_of_path(&app::folder(p)) {
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
    println!("  Vendor / product / revision: {} / {} / {}", d.vendor, d.product, d.revision);
    println!("  Serial:   {}", if d.serial.is_empty() { "(none)" } else { &d.serial });
    println!("  Reported capacity: {} ({} bytes, {}-byte sectors)", size(d.size), d.size, d.sector_size);
    println!("  Bus:      {}{}", d.bus, if d.removable { ", removable" } else { "" });
    if let Some(u) = &d.usb {
        println!("  USB:      {}, device reports USB {}", u.id_text(), u.usb_version);
        let note = if u.slow_link && u.superspeed_capable == Some(true) {
            " (USB 3 capable; connected at USB 2 speed)"
        } else if u.superspeed_plus_capable == Some(true) && !u.link.contains("SuperSpeed+") {
            " (10 Gbit/s capable; connected at 5 Gbit/s)"
        } else {
            ""
        };
        println!("  Link:     {}{note}", if u.link.is_empty() { "(unknown)" } else { &u.link });
        println!("  USB device: {}", u.device_text());
        println!("  USB serial: {}", u.serial.as_deref().unwrap_or("(none)"));
    }
    if !d.controller.is_empty() {
        println!("  Controller: {}", d.controller);
    }
    if let Some(c) = &d.card {
        println!("  Card:     {}, serial {}", c.text(), c.serial);
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
        println!("  ! Check the serial number: {p}");
    }
    match &d.protected {
        Some(p) => println!("  READ ONLY: {p}. Identification, read scan and read speed are available; writes and file tests are blocked."),
        None => println!("  Can be tested."),
    }
    0
}

fn emit(rep: &Report, cli: &Cli) -> i32 {
    let mut save_failed = false;
    // A report path must not overwrite the backup or its checksum manifest.
    let save = |p: &std::path::Path, body: String| -> std::io::Result<()> {
        if matches!(rep.command.as_str(), "backup" | "restore") {
            let mut f = std::fs::File::create_new(p)?;
            f.write_all(body.as_bytes())?;
            f.sync_all()
        } else {
            std::fs::write(p, body)
        }
    };
    let mut shared = rep.clone();
    if cli.hide_serial {
        shared.hide_serials();
    }
    let rep = &shared;
    if cli.json {
        println!("{}", serde_json::to_string_pretty(rep).unwrap());
    } else {
        print!("{}", rep.text());
    }
    if let Some(p) = &cli.report {
        let is_json = p.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"));
        let body = if is_json { serde_json::to_string_pretty(rep).unwrap() } else { rep.html() };
        match save(p, body) {
            Ok(()) => eprintln!("Report saved to {}", p.display()),
            Err(e) => { eprintln!("Cannot save report {}: {e}", p.display()); save_failed = true; }
        }
    }
    if let Some(p) = &cli.evidence {
        match save(p, rep.evidence_html()) {
            Ok(()) => eprintln!("Evidence sheet saved to {}", p.display()),
            Err(e) => { eprintln!("Cannot save evidence {}: {e}", p.display()); save_failed = true; }
        }
    }
    if save_failed { 3 } else { rep.exit_code() }
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
        Cmd::Backup { t, output } => Job::Backup { w: target(t)?, path: output.clone() },
        Cmd::Restore { backup, to, disk, destroy } => {
            let (w, folder) = if let Some(id) = disk {
                let w = app::resolve_disk(id)?;
                need_destroy(*destroy, &w)?;
                (w, None)
            } else {
                let dst = std::path::absolute(to.as_ref().ok_or("Choose --to or --disk.")?).map_err(|e| e.to_string())?;
                let parent = dst.parent().ok_or("Restore folder needs an existing parent.")?;
                let name = dst.file_name().ok_or("Choose a new restore folder.")?;
                (app::resolve_path(parent.to_str().ok_or("Folder path must be Unicode.")?)?, Some(PathBuf::from(name)))
            };
            Job::Restore { w, path: backup.clone(), folder }
        }
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
        Cmd::Scan { disk, size } => Job::Scan { w: app::resolve_read_disk(disk)?, size: size.as_deref().map(parse_size).transpose()? },
        Cmd::Iops { t, seconds, write, destroy } => {
            let w = target(t)?;
            if *write {
                need_destroy(*destroy, &w)?;
            }
            Job::Iops { w, secs: *seconds, write: *write }
        }
        Cmd::Bench { t, size, write, destroy } => {
            let w = match (&t.path, &t.disk, write) {
                (None, Some(n), false) => app::resolve_read_disk(n)?,
                _ => target(t)?,
            };
            if *write {
                need_destroy(*destroy, &w)?;
            }
            Job::Bench { w, size: parse_size(size)?, write: *write }
        }
        Cmd::SmallFiles { path, count, keep } => Job::SmallFiles { w: app::resolve_path(path)?, count: *count, keep: *keep },
        Cmd::PlugPull { path, checkpoint } => Job::PlugPull { w: app::resolve_path(path)?, checkpoint: parse_size(checkpoint)?.max(1 << 20) },
        #[cfg(target_os = "haiku")]
        Cmd::Fix { .. } => return Err(platform::NO_FIX.into()),
        #[cfg(not(target_os = "haiku"))]
        Cmd::Fix { disk, size, fs, label, destroy } => {
            let w = app::resolve_disk(disk)?;
            need_destroy(*destroy, &w)?;
            let sz = match size.to_ascii_lowercase().as_str() {
                "probe" => app::FixSize::Probe,
                "full" | "whole" => app::FixSize::Whole,
                s => app::FixSize::Bytes(parse_size(s)?),
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
        #[cfg(any(windows, target_os = "linux", target_os = "freebsd"))]
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
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        Cmd::WriteNewFile { path } => {
            let mut data = vec![];
            return match std::io::Read::read_to_end(&mut std::io::stdin(), &mut data).and_then(|_| crate::platform::write_new_file(path, &data)) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("Cannot write {}: {e}", path.display());
                    3
                }
            };
        }
        Cmd::List => return list(cli.json),
        Cmd::Info(t) => return info(t, cli.json),
        Cmd::Smart(t) => return smart(t, cli.json),
        Cmd::FlashId { id } => {
            return match crate::flash::parse_id(id).and_then(|id| crate::flash::lookup(&id)) {
                Ok(nand) => {
                    let mut result = crate::flash::Identification::new("user_supplied_nand_id", None);
                    result.nand = Some(nand);
                    emit(&crate::flash::report(result), &cli)
                },
                Err(e) => {
                    let mut report = refuse(e);
                    report.command = "identify".into();
                    emit(&report, &cli)
                },
            };
        },
        Cmd::FlashLayouts => {
            let rows: Vec<_> = crate::flash::profiles().iter().map(|p| serde_json::json!({"layout": p.key, "family": p.family})).collect();
            if cli.json { println!("{}", serde_json::to_string_pretty(&rows).unwrap()); }
            else { for p in crate::flash::profiles() { println!("{}  {}", p.key, p.family); } }
            return 0;
        },
        Cmd::DecodeFlash { input, nand_layout, identity_layout, smi_model, device_info } => {
            let decode = || -> Result<crate::flash::Identification, String> {
                let bytes = crate::flash::read_capture(input)?;
                if let Some(family) = nand_layout { return Ok(crate::flash::decode_nand(*family, bytes)); }
                if *smi_model { return Ok(crate::flash::decode_smi_model(bytes)); }
                let profile = crate::flash::profiles().iter().find(|p| Some(&p.key) == identity_layout.as_ref()).ok_or("Unknown layout; run flash-layouts.")?;
                let expected: Option<platform::DiskInfo> = device_info.as_ref().map(|p| {
                    let bytes = crate::flash::read_capture(p)?;
                    serde_json::from_slice(&bytes).map_err(|e| format!("Invalid device-info JSON: {e}"))
                }).transpose()?;
                Ok(crate::flash::decode_identity(profile, bytes, expected.as_ref()))
            };
            return match decode() {
                Ok(result) => emit(&crate::flash::report(result), &cli),
                Err(e) => {
                    let mut report = refuse(e);
                    report.command = "identify".into();
                    emit(&report, &cli)
                },
            };
        },
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
            let mut rep: Report = match std::fs::read(input).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string())) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("{}: {e}", input.display());
                    return 3;
                }
            };
            if cli.hide_serial {
                rep.hide_serials();
            }
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
    fn offline_decode_requires_one_layout() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["ft", "decode-flash", "reply.bin"]).is_err());
        assert!(Cli::try_parse_from(["ft", "decode-flash", "reply.bin", "--nand-layout", "phison", "--smi-model"]).is_err());
        assert!(Cli::try_parse_from(["ft", "decode-flash", "reply.bin", "--nand-layout", "smi-new"]).is_ok());
        assert!(Cli::try_parse_from(["ft", "decode-flash", "reply.bin", "--device-info", "disk.json", "--smi-model"]).is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("8G").unwrap(), 8_000_000_000);
        assert_eq!(parse_size("8GiB").unwrap(), 8 << 30);
        assert_eq!(parse_size("1.5MB").unwrap(), 1_500_000);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert!(parse_size("3X").is_err());
        for size in ["0", "-1GiB", "0.1", "18446744073709551616", "999999999999999999999TB"] {
            assert!(parse_size(size).is_err(), "accepted {size}");
        }
    }
}
