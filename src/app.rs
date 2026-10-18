//! Jobs both front-ends run: target resolution with the safety guards, and one
//! entry point per command that returns a finished report.

use crate::ctx::Ctx;
use crate::dev::{GIB, MIB};
use crate::fulltest::{self, TestOpts, VerifySource, Where, data_dir};
use crate::platform::{self, DiskInfo, RawDisk};
use crate::probe::{self, Finding, FreeSpaceDev};
use crate::report::{Report, Status, size};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Clone)]
pub enum FixSize {
    Whole,
    /// Run the capacity probe first and use the capacity it finds.
    Probe,
    Bytes(u64),
}

#[derive(Clone)]
pub enum Job {
    Backup { w: Where, path: PathBuf },
    Restore { w: Where, path: PathBuf, folder: Option<PathBuf> },
    Test { w: Where, o: TestOpts },
    Verify { w: Where, h2w: bool, manifest: Option<PathBuf>, loops: u32 },
    Probe { w: Where },
    /// `size`: scan only the first part of the disk.
    Scan { w: Where, size: Option<u64> },
    Iops { w: Where, secs: f64, write: bool },
    SmallFiles { w: Where, count: u64, keep: bool },
    PlugPull { w: Where, checkpoint: u64 },
    /// Sequential speed; on a raw disk read-only unless `write`.
    Bench { w: Where, size: u64, write: bool },
    Fix { disk: DiskInfo, size: FixSize, fs: String, label: String },
}

impl Job {
    pub fn disk(&self) -> Option<&DiskInfo> {
        match self {
            Job::Fix { disk, .. } => Some(disk),
            Job::Backup { w, .. } | Job::Restore { w, .. }
            | Job::Test { w, .. } | Job::Verify { w, .. } | Job::Probe { w }
            | Job::Scan { w, .. } | Job::Iops { w, .. } | Job::SmallFiles { w, .. }
            | Job::PlugPull { w, .. } | Job::Bench { w, .. } => w.disk(),
        }
    }

    /// Only these whole-disk operations may run on a protected disk.
    /// A file benchmark still creates files, even with `write: false`.
    pub fn reads_disk_only(&self) -> bool {
        matches!(self, Job::Scan { w: Where::Disk(_), .. }
            | Job::Bench { w: Where::Disk(_), write: false, .. })
    }

    fn check_access(&self) -> Result<(), String> {
        if let Job::Iops { secs, .. } = self {
            crate::perf::iops_duration(*secs).map_err(|e| e.to_string())?;
        }
        let w = match self {
            Job::Fix { disk, .. } => {
                return disk.check_identity(&platform::check_disk(&disk.id)?);
            }
            Job::Test { w, .. } | Job::Verify { w, .. } | Job::Probe { w }
            | Job::Scan { w, .. } | Job::Iops { w, .. } | Job::SmallFiles { w, .. }
            | Job::PlugPull { w, .. } | Job::Bench { w, .. } => w,
            Job::Backup { w, .. } | Job::Restore { w, .. } => w,
        };
        match w {
            Where::Disk(d) => {
                let now = if self.reads_disk_only() {
                    platform::disk(&d.id).map_err(|e| e.to_string())?
                } else {
                    platform::check_disk(&d.id)?
                };
                d.check_identity(&now)
            }
            Where::Dir { root, .. } => platform::check_path(root).map(|_| ()),
        }
    }

    /// Destroys data on the whole disk (needs explicit confirmation).
    pub fn destructive(&self) -> bool {
        match self {
            Job::Test { w, .. } | Job::Probe { w } => matches!(w, Where::Disk(_)),
            Job::Iops { w, write, .. } | Job::Bench { w, write, .. } => *write && matches!(w, Where::Disk(_)),
            Job::Fix { .. } => true,
            Job::Restore { w, .. } => matches!(w, Where::Disk(_)),
            _ => false,
        }
    }
    pub fn needs_admin(&self) -> bool {
        match self {
            Job::Test { w, .. } | Job::Verify { w, .. } | Job::Scan { w, .. } | Job::Iops { w, .. } | Job::Bench { w, .. } => matches!(w, Where::Disk(_)),
            Job::Probe { .. } | Job::Fix { .. } => true,
            Job::SmallFiles { .. } | Job::PlugPull { .. } => false,
            Job::Backup { w, .. } | Job::Restore { w, .. } => matches!(w, Where::Disk(_)),
        }
    }
}

/// A typed folder or drive letter as a path: quotes dropped, "E:" read as "E:\".
pub fn folder(p: &str) -> PathBuf {
    let mut s = p.trim().trim_matches('"').to_string();
    if cfg!(windows) && s.len() == 2 && s.ends_with(':') {
        s.push('\\');
    }
    PathBuf::from(s)
}

/// A folder (or drive letter) to test in file mode.
pub fn resolve_path(p: &str) -> Result<Where, String> {
    let root = folder(p);
    if !root.is_dir() {
        return Err(format!("{} is not an existing folder or drive.", root.display()));
    }
    let (vol, disk) = platform::check_path(&root)?;
    Ok(Where::Dir { root, vol, disk })
}

/// A whole disk to test in raw mode.
pub fn resolve_disk(n: &str) -> Result<Where, String> {
    Ok(Where::Disk(platform::check_disk(n)?))
}

pub fn resolve_read_disk(n: &str) -> Result<Where, String> {
    Ok(Where::Disk(platform::disk(n).map_err(|e| e.to_string())?))
}

pub fn identification_report(d: &DiskInfo) -> Report {
    let mut rep = Report::new("info", Where::Disk(d.clone()).target());
    rep.read_only = true;
    rep.set(Status::Info, "Drive identification. No storage test was run.");
    rep.verdict.details = d.details().into_iter().map(|(k, v)| format!("{k}: {v}")).collect();
    rep.notes.push("Identity comes from the operating system. Controller hints do not confirm the controller or NAND inside the drive. Reported capacity has not been verified.".into());
    rep.finish(Instant::now());
    rep
}

fn refused(cmd: &str, w: Option<&Where>, msg: String) -> Report {
    let mut r = Report::new(cmd, w.map(|w| w.target()).unwrap_or_default());
    r.set(Status::Refused, msg);
    r
}

fn refused_job(job: &Job, msg: String) -> Report {
    let (command, target) = match job {
        Job::Backup { w, .. } => ("backup", w.target()),
        Job::Restore { w, .. } => ("restore", w.target()),
        Job::Test { w, .. } => ("test", w.target()),
        Job::Verify { w, .. } => ("verify", w.target()),
        Job::Probe { w } => ("probe", w.target()),
        Job::Scan { w, .. } => ("scan", w.target()),
        Job::Iops { w, .. } => ("iops", w.target()),
        Job::SmallFiles { w, .. } => ("small-files", w.target()),
        Job::PlugPull { w, .. } => ("plug-pull", w.target()),
        Job::Bench { w, .. } => ("bench", w.target()),
        Job::Fix { disk, .. } => ("fix", Where::Disk(disk.clone()).target()),
    };
    let mut rep = Report::new(command, target);
    rep.read_only = job.reads_disk_only();
    rep.set(Status::Refused, msg);
    rep.finish(Instant::now());
    rep
}

pub fn run(job: &Job, ctx: &Ctx) -> Report {
    if let Err(e) = job.check_access() {
        return refused_job(job, e);
    }
    if job.needs_admin() && !platform::is_admin() {
        return refused_job(job, "This operation requires administrator or root access. Restart FlashTester with those permissions.".into());
    }
    let t0 = Instant::now();
    let _awake = platform::KeepAwake::new();
    let watch = smart_disk(job).filter(|_| platform::is_admin()).and_then(|d| crate::smart::Watch::start(&d, ctx));
    let mut rep = match job {
        Job::Backup { w, path } => crate::backup::run(w, path, false, None, ctx),
        Job::Restore { w, path, folder } => crate::backup::run(w, path, true, folder.as_deref(), ctx),
        Job::Test { w, o } => fulltest::run_test(w, o, ctx),
        Job::Verify { w, h2w, manifest, loops } => verify(w, *h2w, manifest.as_deref(), *loops, ctx),
        Job::Probe { w } => run_probe(w, ctx),
        Job::Scan { w, size } => scan(w, *size, ctx),
        Job::Iops { w, secs, write } => iops(w, *secs, *write, ctx),
        Job::SmallFiles { w, count, keep } => small_files(w, *count, *keep, ctx),
        Job::PlugPull { w, checkpoint } => crate::trust::run_plug_pull(w, *checkpoint, ctx),
        Job::Bench { w, size, write } => bench(w, *size, *write, ctx),
        Job::Fix { disk, size, fs, label } => fix(disk, size, fs, label, ctx),
    };
    if let Some(w) = watch {
        w.finish(&mut rep);
    }
    if rep.finished.is_empty() {
        rep.finish(t0);
    }
    if job.reads_disk_only() {
        rep.read_only = true;
        rep.notes.push("Read-only check. No test data was written. Reads cannot confirm usable capacity. Other programs using the drive can affect speed and may respond more slowly during the check.".into());
    }
    rep
}

/// The drive a job runs on, for the SMART before/after snapshot. Not for the
/// plug-pull test (the drive vanishes on purpose) or fix (diskpart rewrites it).
fn smart_disk(job: &Job) -> Option<DiskInfo> {
    let w = match job {
        Job::Test { w, .. } | Job::Verify { w, .. } | Job::Probe { w } | Job::Scan { w, .. } | Job::Iops { w, .. } | Job::SmallFiles { w, .. } | Job::Bench { w, .. } => w,
        Job::PlugPull { .. } | Job::Fix { .. } | Job::Backup { .. } | Job::Restore { .. } => return None,
    };
    match w {
        Where::Disk(d) => Some(d.clone()),
        Where::Dir { disk, .. } => disk.clone(),
    }
}

fn verify(w: &Where, h2w: bool, manifest: Option<&Path>, loops: u32, ctx: &Ctx) -> Report {
    let src = if h2w {
        VerifySource::H2testw
    } else {
        let m = match (manifest, w) {
            (Some(p), Where::Disk(d)) => fulltest::load_manifest(p).map_err(|e| format!("{}: {e}", p.display())).and_then(|m| {
                if m.disk_instance.as_deref().is_some_and(|i| i != d.instance_id) || m.claimed != d.size {
                    Err(format!("{} was written for another disk ({}, {}), not disk {} ({}).", p.display(), m.disk_instance.as_deref().unwrap_or("?"), size(m.claimed), d.id, size(d.size)))
                } else {
                    Ok(m)
                }
            }),
            (Some(p), _) => fulltest::load_manifest(p).map_err(|e| format!("{}: {e}", p.display())),
            (None, Where::Dir { root, vol, .. }) => fulltest::load_manifest(&data_dir(root).join(fulltest::MANIFEST))
                .or_else(|_| fulltest::find_local_manifest(|m| m.volume_guid.as_deref() == Some(&vol.guid)).ok_or(()))
                .map_err(|_| format!("No FlashTester data found in {} (no manifest). Use --h2w for h2testw files.", data_dir(root).display())),
            (None, Where::Disk(d)) => fulltest::find_local_manifest(|m| m.mode == "raw" && m.disk_instance.as_deref() == Some(&d.instance_id))
                .ok_or_else(|| format!("No record of a raw test on disk {}; pass --manifest.", d.id)),
        };
        match m {
            Ok(m) => VerifySource::Manifest(m),
            Err(e) => return refused("verify", Some(w), e),
        }
    };
    fulltest::run_verify(w, src, loops, ctx)
}

fn run_probe(w: &Where, ctx: &Ctx) -> Report {
    let t0 = Instant::now();
    let mut rep = Report::new("probe", w.target());
    let result = match w {
        Where::Disk(d) => RawDisk::open(d, true).and_then(|mut dev| {
            let claimed = dev.size;
            let (step, pos) = probe::grid(claimed, probe::BLOCK as u64, 1024);
            ctx.log(format!("Probing {} positions every {} (raw, destructive)", pos.len(), size(step)));
            probe::run(&mut dev, pos, step, claimed, "raw", ctx)
        }),
        Where::Dir { root, vol, disk } => {
            let Some(d) = disk else {
                return refused("probe", Some(w), "The free-space probe needs a volume on a single disk.".into());
            };
            rep.warnings.push("On a fake drive, probe writes past the real capacity land on existing data. Back up the drive first.".into());
            let dir = data_dir(root).join("probe");
            match FreeSpaceDev::create(root, &dir, vol, d, ctx) {
                Ok(mut dev) => {
                    let (step, pos) = dev.positions(probe::BLOCK as u64);
                    ctx.log(format!("Probing {} positions inside {} of free space", pos.len(), size(vol.free)));
                    let r = probe::run(&mut dev, pos, step, d.size, "free-space", ctx);
                    dev.remove();
                    let _ = std::fs::remove_dir(&dir);
                    let _ = std::fs::remove_dir(data_dir(root));
                    r
                }
                Err(e) => {
                    let _ = fulltest::remove_data_dir(&dir);
                    Err(e)
                }
            }
        }
    };
    match result {
        Ok(p) => {
            let (st, head) = match p.finding {
                Finding::Genuine => (Status::Ok, format!("All {} sampled positions across {} kept their data. Run a full test to check the space between them.", p.positions.len(), size(p.claimed))),
                Finding::Fake => (Status::Fake, format!("Fake capacity: only about {} of the claimed {} stores data.", size(p.usable), size(p.claimed))),
                Finding::Defective => (Status::Defective, format!("{} of {} sampled positions lost data. The drive or its connection may be faulty.", p.bad, p.positions.len())),
                Finding::ReadOnly => (Status::ReadOnly, "The drive did not retain the written data. It may be read-only or faulty.".to_string()),
            };
            rep.set(st, head);
            // Free space may cover only the low addresses, where every fake still works.
            let top = p.positions.last().copied().unwrap_or(0);
            if p.mode == "free-space" && p.finding == Finding::Genuine && (p.positions.len() < 16 || top < p.claimed / 10 * 9) {
                rep.set(
                    Status::Info,
                    format!("Inconclusive: sampled free space reaches only {} of {}. Capacity beyond that was not checked. Free up space or use a whole-disk probe, which erases data.", size(top), size(p.claimed)),
                );
            } else if p.mode == "free-space" && p.positions.len() < 64 {
                rep.warnings.push(format!("Only {} positions were tested in free space, so the capacity estimate has limited precision.", p.positions.len()));
            }
            if p.finding == Finding::Fake
                && let Where::Disk(d) = w {
                    rep.verdict.details.push(format!("To erase the drive and format the estimated usable space: {}", platform::fix_hint(&d.id, Some(p.usable))));
                }
            if let Where::Disk(d) = w {
                rep.notes.push(format!("The probe overwrote the partition table. Recreate it with: {}", platform::fix_hint(&d.id, None)));
            }
            rep.probe = Some(p);
        }
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => rep.set(Status::Cancelled, "Cancelled."),
        Err(e) => rep.set(Status::Error, format!("Probe failed: {e}")),
    }
    rep.finish(t0);
    rep
}

fn scan(w: &Where, limit: Option<u64>, ctx: &Ctx) -> Report {
    let t0 = Instant::now();
    let mut rep = Report::new("scan", w.target());
    let Where::Disk(d) = w else {
        return refused("scan", Some(w), "The surface scan reads a whole disk: give --disk N.".into());
    };
    match RawDisk::open(d, false) {
        Ok(mut dev) => {
            let s = crate::scan::run_scan(&mut dev, limit, 8 << 20, ctx);
            let (st, head) = if s.stop == crate::engine::Stop::Cancelled {
                (Status::Cancelled, format!("Cancelled after {}.", size(s.stats.bytes)))
            } else if s.stop == crate::engine::Stop::Disconnected {
                (Status::Unreliable, "The drive disconnected during the scan.".to_string())
            } else if let crate::engine::Stop::Error(e) = &s.stop {
                (Status::Error, format!("Scan failed: {e}"))
            } else if s.bad_bytes > 0 {
                (Status::Defective, format!("{} could not be read.", size(s.bad_bytes)))
            } else if s.events.iter().any(|e| e.kind == "io-error") {
                (Status::Unreliable, format!("All {} was readable after retries, but some reads failed. Check the drive and its connection.", size(s.size)))
            } else {
                (Status::Ok, format!("All {} readable at {:.1} MB/s.", size(s.size), s.stats.mb_s))
            };
            rep.set(st, head);
            rep.tested = s.stats.bytes;
            if !s.slow.is_empty() {
                rep.warnings.push(format!("Slow areas found: {}. Read speed alone does not identify the cause.", s.slow.len()));
            }
            rep.scan = Some(s);
        }
        Err(e) => rep.set(Status::Error, format!("Cannot open disk {}: {e}", d.id)),
    }
    rep.finish(t0);
    rep
}

fn iops(w: &Where, secs: f64, write: bool, ctx: &Ctx) -> Report {
    let t0 = Instant::now();
    let mut rep = Report::new("iops", w.target());
    let res = match w {
        Where::Dir { root, .. } => (|| {
            let dir = data_dir(root);
            fulltest::make_dir(&dir)?;
            let path = dir.join("iops.fti");
            let free = platform::free_space(root)?;
            let len = GIB.min(free / 2) / MIB * MIB;
            if len < 64 * MIB {
                return Err(std::io::Error::other("need at least 128 MB free"));
            }
            crate::perf::make_iops_file(&path, len, ctx)?;
            let file = crate::perf::open_unbuffered(&path, true)?;
            let r = crate::perf::run_iops(&|wr| crate::dev::io_handle(&file, wr), len, true, secs, ctx);
            drop(file);
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_dir(&dir);
            r
        })(),
        Where::Disk(d) => (|| {
            // Writing holds the volume locks for the duration.
            let raw = RawDisk::open(d, write)?;
            // Every worker stays on this checked device even if its name is reused.
            let r = crate::perf::run_iops(&|wr| crate::dev::io_handle(&raw.file, wr), raw.size, write, secs, ctx);
            drop(raw);
            r
        })(),
    };
    match res {
        Ok(r) => {
            let q1 = r.rows.iter().find(|x| x.kind == "read" && x.qd == 1).map(|x| x.iops).unwrap_or(0.0);
            rep.set(Status::Info, format!("Random 4K read: {q1:.0} IOPS at queue depth 1."));
            if let Some(wq) = r.rows.iter().find(|x| x.kind == "write" && x.qd == 1) {
                rep.verdict.headline.push_str(&format!(" Random 4K write: {:.0} IOPS.", wq.iops));
                if wq.iops < 50.0 {
                    rep.warnings.push("Random writes are slow. Programs or an operating system stored here may respond slowly.".into());
                }
            }
            rep.iops = Some(r);
            if ctx.cancelled() {
                rep.set(Status::Cancelled, "Cancelled.");
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => rep.set(Status::Cancelled, "Cancelled."),
        Err(e) => rep.set(Status::Error, format!("IOPS test failed: {e}")),
    }
    rep.finish(t0);
    rep
}

fn small_files(w: &Where, count: u64, keep: bool, ctx: &Ctx) -> Report {
    let t0 = Instant::now();
    let mut rep = Report::new("small-files", w.target());
    let Where::Dir { root, .. } = w else {
        return refused("small-files", Some(w), "The small-files test needs a drive letter or folder.".into());
    };
    let dir = data_dir(root).join("small");
    let r = crate::perf::run_small_files(&dir, count, ctx);
    if ctx.cancelled() {
        rep.set(Status::Cancelled, "Cancelled.");
    } else if r.ok() {
        rep.set(Status::Ok, format!("{} small files written, renamed, deleted and verified without errors ({:.0} files/s).", r.files, r.create_per_s));
    } else {
        rep.set(Status::Defective, "File errors: missing files, incorrect contents, or files left after deletion.");
    }
    if r.ok() && !keep {
        let _ = fulltest::remove_data_dir(&dir);
        let _ = std::fs::remove_dir(data_dir(root));
    }
    rep.tested = r.bytes;
    rep.small_files = Some(r);
    rep.finish(t0);
    rep
}

fn fix(disk: &DiskInfo, sz: &FixSize, fs: &str, label: &str, ctx: &Ctx) -> Report {
    let t0 = Instant::now();
    let w = Where::Disk(disk.clone());
    let mut rep = Report::new("fix", w.target());
    let mib = match sz {
        FixSize::Whole => None,
        FixSize::Bytes(b) => Some(crate::fix::partition_mib(*b)),
        FixSize::Probe => {
            let p = run_probe(&w, ctx);
            let usable = p.probe.as_ref().map(|x| (x.finding, x.usable));
            rep.probe = p.probe.clone();
            match usable {
                Some((Finding::Fake, u)) => Some(crate::fix::partition_mib(u)),
                Some((Finding::Genuine, _)) => None,
                _ => {
                    rep.verdict = p.verdict;
                    rep.verdict.details.push("Formatting stopped because the probe could not establish a usable capacity boundary.".into());
                    rep.finish(t0);
                    return rep;
                }
            }
        }
    };
    ctx.phase("Creating the partition and file system", 0);
    match crate::fix::fix(disk, mib, fs, label) {
        Ok(out) => {
            let part = mib.map(|m| size(m * MIB)).unwrap_or_else(|| size(disk.size));
            rep.set(Status::Ok, format!("Disk {} now has one {fs} partition of {part}.", disk.id));
            rep.notes.push(out.lines().filter(|l| !l.trim().is_empty()).next_back().unwrap_or("").trim().to_string());
            if mib.is_some() {
                rep.warnings.push("Only the real capacity is partitioned. Don't extend or reformat the partition to the full claimed size.".into());
            }
        }
        Err(e) => rep.set(Status::Error, e),
    }
    rep.finish(t0);
    rep
}

fn bench(w: &Where, len: u64, write: bool, ctx: &Ctx) -> Report {
    use crate::engine::{Stop, verify_pass, write_pass};
    let t0 = Instant::now();
    let mut rep = Report::new("bench", w.target());
    let block = 8usize << 20;
    let pat = crate::pattern::Pattern::Seeded { tag: crate::pattern::new_tag() };
    let res: std::io::Result<crate::perf::BenchResult> = (|| {
        match w {
            Where::Dir { root, .. } => {
                let dir = data_dir(root).join("bench");
                fulltest::remove_data_dir(&dir)?;
                fulltest::make_dir(&dir)?;
                let result = (|| {
                    let free = platform::free_space(root)?;
                    let len = len.min(free.saturating_sub(64 * MIB)) / MIB * MIB;
                    if len < 16 * MIB {
                        return Err(std::io::Error::other("need at least 80 MB free"));
                    }
                    let mut fs = crate::dev::FileSet::new(&dir, len);
                    let wr = write_pass(&mut fs, pat, 0, len, block, ctx, None);
                    fs.close()?;
                    if wr.stop != Stop::Done {
                        return Err(benchmark_stop("write", wr.stop));
                    }
                    let vr = verify_pass(&mut fs, pat, 0, wr.end, block, ctx);
                    fs.close()?;
                    if vr.stop != Stop::Done {
                        return Err(benchmark_stop("verification", vr.stop));
                    }
                    if vr.tally.lost() > 0 {
                        rep.warnings.push(format!("{} of the benchmark data came back wrong: run a full test.", size(vr.tally.lost() * 512)));
                    }
                    Ok(crate::perf::BenchResult { size: len, block: block as u64, write: Some(wr.stats), read: vr.stats })
                })();
                if let Err(e) = fulltest::remove_data_dir(&dir) {
                    rep.warnings.push(format!("Cannot remove benchmark files in {}: {e}", dir.display()));
                }
                let _ = std::fs::remove_dir(data_dir(root));
                result
            }
            Where::Disk(d) => {
                let mut dev = RawDisk::open(d, write)?;
                let len = len.min(dev.size) / 4096 * 4096;
                let wst = if write {
                    let wr = write_pass(&mut dev, pat, 0, len, block, ctx, None);
                    if wr.stop != Stop::Done {
                        return Err(benchmark_stop("write", wr.stop));
                    }
                    Some(wr.stats)
                } else {
                    None
                };
                let read = crate::perf::seq_read(&mut dev, len, block, ctx)?;
                Ok(crate::perf::BenchResult { size: len, block: block as u64, write: wst, read })
            }
        }
    })();
    match res {
        Ok(b) => {
            let wtxt = b.write.as_ref().map(|w| format!("write {:.1} MB/s, ", w.mb_s)).unwrap_or_default();
            rep.set(Status::Info, format!("Sequential {wtxt}read {:.1} MB/s (speed only, capacity not checked).", b.read.mb_s));
            if ctx.cancelled() {
                rep.set(Status::Cancelled, "Cancelled.");
            }
            if !rep.warnings.iter().any(|w| w.contains("came back wrong")) {
                rep.tested = b.read.bytes;
            } else {
                rep.verdict.status = Status::Defective;
            }
            rep.bench = Some(b);
        }
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted || ctx.cancelled() => rep.set(Status::Cancelled, "Cancelled."),
        Err(e) => rep.set(Status::Error, format!("Benchmark failed: {e}")),
    }
    if write
        && let Where::Disk(d) = w {
            rep.notes.push(format!("The start of disk {} was overwritten. Recreate a file system with: {}", d.id, platform::fix_hint(&d.id, None)));
        }
    rep.finish(t0);
    rep
}

fn benchmark_stop(phase: &str, stop: crate::engine::Stop) -> std::io::Error {
    let kind = if stop == crate::engine::Stop::Cancelled { std::io::ErrorKind::Interrupted } else { std::io::ErrorKind::Other };
    std::io::Error::new(kind, format!("{phase} stopped: {stop:?}"))
}

#[cfg(test)]
mod benchmark_tests {
    use super::*;

    #[test]
    fn failed_and_cancelled_benchmarks_remove_their_folder() {
        let root = std::env::temp_dir().join(format!("flashtester-bench-cleanup-{}", crate::pattern::new_tag()));
        std::fs::create_dir(&root).unwrap();
        let target = Where::Dir { root: root.clone(), vol: Default::default(), disk: None };
        let ctx = Ctx::new();
        let failed = bench(&target, 1, false, &ctx);
        assert_eq!(failed.verdict.status, Status::Error);
        assert!(!data_dir(&root).exists());
        ctx.cancel();
        let cancelled = bench(&target, 16 * MIB, false, &ctx);
        assert_eq!(cancelled.verdict.status, Status::Cancelled);
        assert!(!data_dir(&root).exists());
        std::fs::remove_dir(root).unwrap();
    }
}

#[cfg(all(test, windows))]
mod access_tests {
    use super::*;

    #[test]
    fn system_disk_access_check_allows_reads_and_refuses_writes() {
        // Enumeration and access validation only: no raw handle or test starts.
        let disk = platform::list_disks().into_iter().find(|d| d.protected.is_some()).expect("Windows system disk must be protected");
        let w = Where::Disk(disk.clone());
        assert!(Job::Scan { w: w.clone(), size: Some(MIB) }.check_access().is_ok());
        assert!(Job::Bench { w: w.clone(), size: MIB, write: false }.check_access().is_ok());
        assert!(Job::Bench { w: w.clone(), size: MIB, write: true }.check_access().is_err());
        assert!(Job::Test { w: w.clone(), o: TestOpts::default() }.check_access().is_err());
        assert!(Job::Probe { w }.check_access().is_err());
        assert!(Job::Fix { disk, size: FixSize::Whole, fs: "exfat".into(), label: "TEST".into() }.check_access().is_err());
        let root = std::path::PathBuf::from(std::env::var("SystemRoot").unwrap());
        let w = Where::Dir { root, vol: Default::default(), disk: None };
        assert!(Job::Bench { w, size: MIB, write: false }.check_access().is_err());
    }
}
