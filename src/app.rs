//! Jobs both front-ends run: target resolution with the safety guards, and one
//! entry point per command that returns a finished report.

use crate::ctx::Ctx;
use crate::dev::{GIB, MIB};
use crate::fulltest::{self, TestOpts, VerifySource, Where, data_dir};
use crate::platform::{self, DiskInfo, RawDisk};
use crate::probe::{self, FreeSpaceDev};
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
    /// Destroys data on the whole disk (needs explicit confirmation).
    pub fn destructive(&self) -> bool {
        match self {
            Job::Test { w, .. } | Job::Probe { w } => matches!(w, Where::Disk(_)),
            Job::Iops { w, write, .. } | Job::Bench { w, write, .. } => *write && matches!(w, Where::Disk(_)),
            Job::Fix { .. } => true,
            _ => false,
        }
    }
    pub fn needs_admin(&self) -> bool {
        match self {
            Job::Test { w, .. } | Job::Verify { w, .. } | Job::Scan { w, .. } | Job::Iops { w, .. } | Job::Bench { w, .. } => matches!(w, Where::Disk(_)),
            Job::Probe { .. } | Job::Fix { .. } => true,
            Job::SmallFiles { .. } | Job::PlugPull { .. } => false,
        }
    }
}

/// A folder (or drive letter) to test in file mode.
pub fn resolve_path(p: &str) -> Result<Where, String> {
    let mut s = p.trim().trim_matches('"').to_string();
    if cfg!(windows) && s.len() == 2 && s.ends_with(':') {
        s.push('\\');
    }
    let root = PathBuf::from(&s);
    if !root.is_dir() {
        return Err(format!("{s} is not an existing folder or drive."));
    }
    let (vol, disk) = platform::check_path(&root)?;
    Ok(Where::Dir { root, vol, disk })
}

/// A whole disk to test in raw mode.
pub fn resolve_disk(n: &str) -> Result<Where, String> {
    Ok(Where::Disk(platform::check_disk(n)?))
}

fn refused(cmd: &str, w: Option<&Where>, msg: String) -> Report {
    let mut r = Report::new(cmd, w.map(|w| w.target()).unwrap_or_default());
    r.set(Status::Refused, msg);
    r
}

pub fn run(job: &Job, ctx: &Ctx) -> Report {
    if job.needs_admin() && !platform::is_admin() {
        return refused("", None, "This needs administrator rights: run FlashTester from an elevated (Run as administrator) terminal.".into());
    }
    let t0 = Instant::now();
    let _awake = platform::KeepAwake::new();
    let mut rep = match job {
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
    if rep.finished.is_empty() {
        rep.finish(t0);
    }
    rep
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
                    let _ = std::fs::remove_dir_all(&dir);
                    Err(e)
                }
            }
        }
    };
    match result {
        Ok(p) => {
            let (st, head) = match p.finding.as_str() {
                "genuine" => (Status::Ok, format!("Capacity is genuine: all {} probes across {} kept their data.", p.positions.len(), size(p.claimed))),
                "fake" => (Status::Fake, format!("Fake capacity: only about {} of the claimed {} stores data.", size(p.usable), size(p.claimed))),
                "defective" => (Status::Defective, format!("{} of {} probes lost their data: the flash is defective.", p.bad, p.positions.len())),
                _ => (Status::ReadOnly, "The drive does not keep written data (read-only or dead).".to_string()),
            };
            rep.set(st, head);
            // Free space may cover only the low addresses, where every fake still works.
            let top = p.positions.last().copied().unwrap_or(0);
            if p.mode == "free-space" && p.finding == "genuine" && (p.positions.len() < 16 || top < p.claimed / 10 * 9) {
                rep.set(
                    Status::Info,
                    format!("Inconclusive: free space reaches only up to {} of {}, and a fake can hide above that. Free up space or use the raw probe.", size(top), size(p.claimed)),
                );
            } else if p.mode == "free-space" && p.positions.len() < 64 {
                rep.warnings.push(format!("Only {} probe positions fell inside free space; the result is coarse.", p.positions.len()));
            }
            if p.finding == "fake"
                && let Where::Disk(d) = w {
                    rep.verdict.details.push(format!("Make the drive usable at its real size: flashtester fix --disk {} --size {} --destroy", d.id, p.usable));
                }
            if let Where::Disk(d) = w {
                rep.notes.push(format!("The probe overwrote the partition table. Recreate it with: flashtester fix --disk {} --destroy", d.id));
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
            } else if s.bad_bytes > 0 {
                (Status::Defective, format!("{} could not be read.", size(s.bad_bytes)))
            } else {
                (Status::Ok, format!("All {} readable at {:.1} MB/s.", size(s.size), s.stats.mb_s))
            };
            rep.set(st, head);
            rep.tested = s.stats.bytes;
            if !s.slow.is_empty() {
                rep.warnings.push(format!("{} region(s) read much slower than the rest (weak or worn flash).", s.slow.len()));
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
            let free = fulltest::free_space(root)?;
            let len = GIB.min(free / 2) / MIB * MIB;
            if len < 64 * MIB {
                return Err(std::io::Error::other("need at least 128 MB free"));
            }
            crate::perf::make_iops_file(&path, len, ctx)?;
            let p2 = path.clone();
            let r = crate::perf::run_iops(&move |wr| crate::perf::open_unbuffered(&p2, wr), len, true, secs, ctx);
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_dir(&dir);
            r
        })(),
        Where::Disk(d) => (|| {
            // Writing holds the volume locks for the duration.
            let raw = RawDisk::open(d, write)?;
            let path = d.path.clone();
            let r = crate::perf::run_iops(&move |wr| crate::perf::open_unbuffered(Path::new(&path), wr), raw.size, write, secs, ctx);
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
                    rep.warnings.push("Random writes are very slow: running programs or an OS from this drive will be painful.".into());
                }
            }
            rep.iops = Some(r);
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
        rep.set(Status::Defective, "File system errors: files missing, wrong, or not deleted.");
    }
    if r.ok() && !keep {
        let _ = std::fs::remove_dir_all(&dir);
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
            let usable = p.probe.as_ref().map(|x| (x.finding.clone(), x.usable));
            rep.probe = p.probe.clone();
            match usable {
                Some((f, u)) if f == "fake" => Some(crate::fix::partition_mib(u)),
                Some((f, _)) if f == "genuine" => None,
                _ => {
                    rep.verdict = p.verdict;
                    rep.verdict.details.push("Not repartitioning: the probe did not find a clean capacity boundary.".into());
                    rep.finish(t0);
                    return rep;
                }
            }
        }
    };
    ctx.phase("Repartitioning (diskpart)", 0);
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
                crate::dev::no_link(&dir)?;
                let _ = std::fs::remove_dir_all(&dir);
                fulltest::make_dir(&dir)?;
                let free = fulltest::free_space(root)?;
                let len = len.min(free.saturating_sub(64 * MIB)) / MIB * MIB;
                if len < 16 * MIB {
                    return Err(std::io::Error::other("need at least 80 MB free"));
                }
                let mut fs = crate::dev::FileSet::new(&dir, len);
                let wr = write_pass(&mut fs, pat, 0, len, block, ctx, None);
                fs.close()?;
                if wr.stop != Stop::Done {
                    return Err(std::io::Error::other(format!("write stopped: {:?}", wr.stop)));
                }
                let vr = verify_pass(&mut fs, pat, 0, wr.end, block, ctx);
                fs.close()?;
                let _ = std::fs::remove_dir_all(&dir);
                let _ = std::fs::remove_dir(data_dir(root));
                if vr.tally.lost() > 0 {
                    rep.warnings.push(format!("{} of the benchmark data came back wrong: run a full test.", size(vr.tally.lost() * 512)));
                }
                Ok(crate::perf::BenchResult { size: len, block: block as u64, write: Some(wr.stats), read: vr.stats })
            }
            Where::Disk(d) => {
                let mut dev = RawDisk::open(d, write)?;
                let len = len.min(dev.size) / 4096 * 4096;
                let wst = if write {
                    let wr = write_pass(&mut dev, pat, 0, len, block, ctx, None);
                    if wr.stop != Stop::Done {
                        return Err(std::io::Error::other(format!("write stopped: {:?}", wr.stop)));
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
                rep.tested = b.size;
            } else {
                rep.verdict.status = Status::Defective;
            }
            rep.bench = Some(b);
        }
        Err(e) => rep.set(Status::Error, format!("Benchmark failed: {e}")),
    }
    if write
        && let Where::Disk(d) = w {
            rep.notes.push(format!("The start of disk {} was overwritten. Recreate a file system with: flashtester fix --disk {} --destroy", d.id, d.id));
        }
    rep.finish(t0);
    rep
}
