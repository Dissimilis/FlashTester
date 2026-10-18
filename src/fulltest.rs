//! Full test (write everything, read everything back) in file mode or raw mode,
//! endurance loops, verify-only loops, the manifest, and the verdict.

use crate::ctx::Ctx;
use crate::dev::{Dev, FileSet, GIB, MIB};
use crate::engine::{EarlyCheck, Stop, VerifyResult, WriteResult, verify_pass, write_pass};
use crate::pattern::{Pattern, new_tag};
use crate::platform::{DiskInfo, RawDisk, VolumeInfo};
use crate::report::{PassSummary, Report, Status, Target, size};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const DATA_DIR: &str = "FlashTester";
pub const MANIFEST: &str = "manifest.json";

#[derive(Clone)]
pub enum Where {
    /// File mode: a folder on a mounted volume.
    Dir { root: PathBuf, vol: VolumeInfo, disk: Option<DiskInfo> },
    /// Raw mode: a whole physical disk.
    Disk(DiskInfo),
}

impl Where {
    pub fn target(&self) -> Target {
        match self {
            Where::Dir { root, vol, disk } => Target { mode: "file".into(), path: Some(root.display().to_string()), disk: disk.clone(), volume: Some(vol.clone()), claimed: vol.size },
            Where::Disk(d) => Target { mode: "raw".into(), path: None, disk: Some(d.clone()), volume: None, claimed: d.size },
        }
    }
    pub fn disk(&self) -> Option<&DiskInfo> {
        match self {
            Where::Dir { disk, .. } => disk.as_ref(),
            Where::Disk(d) => Some(d),
        }
    }
}

#[derive(Clone)]
pub struct TestOpts {
    pub size: Option<u64>,
    pub block: usize,
    pub early_abort: bool,
    pub replug: bool,
    /// 1 = one pass; 0 = until an error or cancel.
    pub passes: u32,
    pub stop_on_error: bool,
    pub keep: bool,
}

impl Default for TestOpts {
    fn default() -> Self {
        TestOpts { size: None, block: 8 << 20, early_abort: true, replug: false, passes: 1, stop_on_error: true, keep: true }
    }
}

/// What was written, so a later run can verify it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Manifest {
    pub tool: String,
    pub created: String,
    pub mode: String,
    pub pattern: Pattern,
    pub bytes: u64,
    pub complete: bool,
    pub file_size: u64,
    pub volume_guid: Option<String>,
    pub disk_instance: Option<String>,
    /// `--disk` id of the disk (number on Windows, kernel name on Linux).
    #[serde(default)]
    pub disk_id: Option<String>,
    pub claimed: u64,
}

/// Create a test folder. Right after `clean`, Windows can still hold the old
/// folder as delete-pending and refuse access for a moment: retry briefly.
pub fn make_dir(p: &Path) -> io::Result<()> {
    check_data_dir(p)?;
    for _ in 0..20 {
        match std::fs::create_dir_all(p) {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => std::thread::sleep(std::time::Duration::from_millis(250)),
            r => return r,
        }
    }
    std::fs::create_dir_all(p)
}

fn check_data_dir(p: &Path) -> io::Result<()> {
    // Neither the folder nor the FlashTester folder above it may be a link.
    for a in p.ancestors().take(2) {
        crate::dev::no_link(a)?;
    }
    Ok(())
}

/// Remove an owned test subfolder only after checking it and its parent.
pub fn remove_data_dir(p: &Path) -> io::Result<()> {
    check_data_dir(p)?;
    match std::fs::remove_dir_all(p) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

pub fn data_dir(root: &Path) -> PathBuf {
    root.join(DATA_DIR)
}

/// Save on the drive (file mode) and in platform::manifest_dir(), in case the drive loses
/// it. Err: the local copy could not be kept.
pub fn save_manifest(m: &Manifest, dir: Option<&Path>) -> io::Result<()> {
    let json = serde_json::to_string_pretty(m).unwrap();
    if let Some(d) = dir {
        if crate::dev::no_link(d).and_then(|_| crate::dev::no_link(&d.join(MANIFEST))).is_ok() {
            let _ = std::fs::write(d.join(MANIFEST), &json);
        }
    }
    let tag = match m.pattern {
        Pattern::Seeded { tag } => tag,
        Pattern::H2testw => 0,
    };
    std::fs::write(crate::platform::manifest_dir()?.join(format!("{tag:016x}.json")), &json)
}

/// The first save of a test's manifest: say so if no local copy could be kept.
fn first_manifest(m: &Manifest, dir: Option<&Path>, ctx: &Ctx) {
    if let Err(e) = save_manifest(m, dir) {
        ctx.log(format!("No local copy of the test record ({e}). \"Check saved test data\" may not find this test."));
    }
}

/// The newest local manifest matching a volume or disk.
pub fn find_local_manifest(pred: impl Fn(&Manifest) -> bool) -> Option<Manifest> {
    let mut v: Vec<Manifest> = std::fs::read_dir(crate::platform::manifest_dir().ok()?)
        .ok()?
        .flatten()
        .filter_map(|e| serde_json::from_slice::<Manifest>(&std::fs::read(e.path()).ok()?).ok())
        .filter(|m| pred(m))
        .collect();
    v.sort_by(|a, b| a.created.cmp(&b.created));
    v.pop()
}

pub fn load_manifest(path: &Path) -> io::Result<Manifest> {
    serde_json::from_slice(&std::fs::read(path)?).map_err(io::Error::other)
}

/// Delete FlashTester test data under `root`. Returns (files, bytes) removed.
pub fn clean(root: &Path) -> io::Result<(u64, u64)> {
    let dir = data_dir(root);
    let (mut n, mut bytes) = (0, 0);
    crate::dev::no_link(&dir)?;
    if !dir.exists() {
        return Ok((0, 0));
    }
    for e in std::fs::read_dir(&dir)?.flatten() {
        let p = e.path();
        let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
        if p.is_dir() && matches!(p.file_name().and_then(|x| x.to_str()), Some("small" | "probe")) {
            remove_data_dir(&p)?;
            n += 1;
        } else if matches!(ext, "ftd" | "ftp" | "ftc" | "fti") || p.file_name().and_then(|x| x.to_str()) == Some(MANIFEST) {
            bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
            std::fs::remove_file(&p)?;
            n += 1;
        }
    }
    let _ = std::fs::remove_dir(&dir);
    Ok((n, bytes))
}

fn open_dev(w: &Where, len: u64, write: bool) -> io::Result<Box<dyn Dev>> {
    Ok(match w {
        Where::Dir { root, .. } => Box::new(FileSet::new(&data_dir(root), len)),
        Where::Disk(d) => Box::new(RawDisk::open(d, write)?),
    })
}

/// Create the data folder, remove old test data, write the manifest; returns the planned length.
fn prepare(w: &Where, o: &TestOpts, pat: Pattern, ctx: &Ctx) -> io::Result<(Box<dyn Dev>, u64)> {
    match w {
        Where::Dir { root, vol, disk } => {
            let dir = data_dir(root);
            let (n, b) = clean(root)?;
            if n > 0 {
                ctx.log(format!("Removed {n} old FlashTester files ({})", size(b)));
            }
            make_dir(&dir)?;
            let mut m = Manifest {
                tool: crate::report::tool_name(),
                created: crate::engine::now(),
                mode: "file".into(),
                pattern: pat,
                bytes: 0,
                complete: false,
                file_size: crate::dev::FILE_SIZE,
                volume_guid: Some(vol.guid.clone()),
                disk_instance: disk.as_ref().map(|d| d.instance_id.clone()),
                disk_id: disk.as_ref().map(|d| d.id.clone()),
                claimed: vol.size,
            };
            first_manifest(&m, Some(&dir), ctx);
            let free = crate::platform::free_space(root)?;
            let len = o.size.unwrap_or(u64::MAX).min(free) / MIB * MIB;
            m.bytes = len;
            let _ = save_manifest(&m, Some(&dir));
            Ok((open_dev(w, len, true)?, len))
        }
        Where::Disk(d) => {
            let dev = open_dev(w, 0, true)?;
            let len = o.size.unwrap_or(u64::MAX).min(dev.len()) / 4096 * 4096;
            let m = Manifest {
                tool: crate::report::tool_name(),
                created: crate::engine::now(),
                mode: "raw".into(),
                pattern: pat,
                bytes: len,
                complete: false,
                file_size: 0,
                volume_guid: None,
                disk_instance: Some(d.instance_id.clone()),
                disk_id: Some(d.id.clone()),
                claimed: d.size,
            };
            first_manifest(&m, None, ctx);
            Ok((dev, len))
        }
    }
}

fn finish_manifest(w: &Where, pat: Pattern, end: u64, complete: bool) {
    let pred = |m: &Manifest| m.pattern == pat;
    if let Some(mut m) = find_local_manifest(pred) {
        m.bytes = end;
        m.complete = complete;
        let dir = match w {
            Where::Dir { root, .. } => Some(data_dir(root)),
            _ => None,
        };
        let _ = save_manifest(&m, dir.as_deref());
    }
}

/// After the user unplugs and replugs the drive: check it is the same drive
/// (and, in file mode, the same volume), then reopen the test data for verifying.
fn reconnect(w: &mut Where, end: u64, rep: &mut Report, ctx: &Ctx) -> Result<Box<dyn Dev>, String> {
    let (changes, now) = crate::trust::replug(w, ctx)?;
    if !changes.is_empty() {
        rep.identity_changes.extend(changes);
        return Err("The drive identity changed after reconnecting. Testing stopped.".into());
    }
    if let Where::Dir { root, vol, .. } = &*w {
        let again = crate::platform::check_path(root);
        let same = again.as_ref().is_ok_and(|(v, _)| v.fs == vol.fs && v.label == vol.label && v.size.abs_diff(vol.size) <= MIB);
        if !same {
            let why = again.err().unwrap_or_else(|| format!("a different volume is now at {}", root.display()));
            return Err(format!("After reconnecting: {why}. Testing stopped."));
        }
    }
    if let (Where::Disk(d), Some(n)) = (&mut *w, now) {
        *d = n;
    }
    open_dev(w, end, false).map_err(|e| format!("Cannot reopen the drive after reconnecting: {e}"))
}

/// Full write + verify test; with `passes != 1` an endurance loop.
pub fn run_test(w: &Where, o: &TestOpts, ctx: &Ctx) -> Report {
    let t0 = Instant::now();
    let mut w = w.clone();
    let w = &mut w;
    let mut rep = Report::new(if o.passes == 1 { "test" } else { "endurance" }, w.target());
    let mut pass = 1u32;
    loop {
        let pat = Pattern::Seeded { tag: new_tag() };
        if o.passes != 1 {
            ctx.log(format!("Pass {pass}{}", if o.passes > 1 { format!(" of {}", o.passes) } else { String::new() }));
        }
        let (mut dev, len) = match prepare(w, o, pat, ctx) {
            Ok(x) => x,
            Err(e) => {
                rep.set(Status::Error, format!("Cannot start: {e}"));
                break;
            }
        };
        if len < MIB {
            rep.set(Status::Error, "Not enough free space to test (less than 1 MB).");
            break;
        }
        if let Where::Dir { vol, .. } = &*w {
            let used = vol.size.saturating_sub(vol.free);
            if pass == 1 && used > 64 * MIB {
                let msg = format!("The drive already holds {} of files. If it is fake, filling it can overwrite them: back them up first.", size(used));
                ctx.log(format!("Warning: {msg}"));
                rep.warnings.push(msg);
            }
        }
        ctx.log(format!("Writing {} with run tag {:016x}", size(len), tag_of(pat)));
        let early = o.early_abort.then(|| EarlyCheck { every: GIB.min((len / 8).max(64 * MIB)), samples: 64 });
        let wr = write_pass(dev.as_mut(), pat, 0, len, o.block, ctx, early.as_ref());
        let end = wr.end;
        finish_manifest(w, pat, end, matches!(wr.stop, Stop::Done | Stop::DiskFull));
        rep.tested = rep.tested.max(end);
        let wstop = wr.stop.clone();
        rep.write.push(wr);
        if wstop == Stop::Cancelled || wstop == Stop::Disconnected && !o.replug {
            break;
        }
        if o.replug {
            drop(dev);
            dev = match reconnect(w, end, &mut rep, ctx) {
                Ok(d) => d,
                Err(e) => {
                    rep.set(Status::Error, e);
                    break;
                }
            };
        }
        let vr = verify_pass(dev.as_mut(), pat, 0, end, o.block, ctx);
        drop(dev);
        let bad = vr.tally.lost() > 0 || matches!(wstop, Stop::EarlyAbort(_) | Stop::Error(_) | Stop::Disconnected) || vr.stop != Stop::Done;
        rep.passes.push(PassSummary {
            pass,
            tag: format!("{:016x}", tag_of(pat)),
            bytes: end,
            write_mb_s: rep.write.last().unwrap().stats.mb_s,
            read_mb_s: vr.stats.mb_s,
            lost_sectors: vr.tally.lost(),
            result: pass_result(rep.write.last().unwrap(), &vr),
        });
        let vstop = vr.stop.clone();
        rep.verify.push(vr);
        ctx.log(format!("Pass {pass}: {}", rep.passes.last().unwrap().result));
        if vstop == Stop::Cancelled || ctx.cancelled() || (bad && o.stop_on_error) || pass == o.passes {
            break;
        }
        pass += 1;
    }
    if rep.verdict.status != Status::Error {
        judge(&mut rep);
    }
    if !o.keep && rep.verdict.status == Status::Ok {
        if let Where::Dir { root, .. } = w {
            let _ = clean(root);
            rep.notes.push("Test data deleted.".into());
        }
    } else if let Where::Dir { root, .. } = w
        && rep.tested > 0 {
            rep.notes.push(format!("Test data kept in {}; check it again later with: flashtester verify {}", data_dir(root).display(), root.display()));
        }
    if let Where::Disk(d) = w
        && !rep.write.is_empty() {
            rep.notes.push(format!("Disk {} no longer has a usable file system. Recreate one with: {}", d.id, crate::platform::fix_hint(&d.id, None)));
        }
    if let (Where::Dir { vol, .. }, Some(_)) = (w, rep.verify.last())
        && rep.tested < vol.size / 10 * 9 && rep.verdict.status == Status::Ok {
            let why = if o.size.is_some() { "limited by --size" } else { "the rest is in use" };
            rep.warnings.push(format!("Only {} of the volume's {} was tested ({why}). Untested space may still lose data. For broader coverage, back up and empty the drive, or use a whole-disk probe, which erases data.", size(rep.tested), size(vol.size)));
        }
    rep.finish(t0);
    rep
}

fn tag_of(p: Pattern) -> u64 {
    match p {
        Pattern::Seeded { tag } => tag,
        Pattern::H2testw => 0,
    }
}

fn pass_result(w: &WriteResult, v: &VerifyResult) -> String {
    match (&w.stop, &v.stop) {
        (Stop::EarlyAbort(_), _) => "aborted early: earlier data lost".into(),
        (_, Stop::Cancelled) | (Stop::Cancelled, _) => "cancelled".into(),
        (Stop::Disconnected, _) | (_, Stop::Disconnected) => "drive disconnected".into(),
        (Stop::Error(e), _) | (_, Stop::Error(e)) => e.clone(),
        _ if v.tally.lost() > 0 => format!("{} lost", size(v.tally.lost() * 512)),
        _ => "OK".into(),
    }
}

/// Set the verdict from all write and verify results.
pub fn judge(rep: &mut Report) {
    // Merged across passes only for "did it ever alias / go stale"; amounts and the
    // capacity evidence come from single passes (losses must not add up across passes).
    let mut merged = crate::engine::Tally::default();
    for v in &rep.verify {
        merged.merge(&v.tally);
    }
    // Span of a pass: every classified sector, readable or not.
    let span = |t: &crate::engine::Tally| (t.ok + t.lost()) * 512;
    let worst = rep.verify.iter().map(|v| &v.tally).max_by_key(|t| t.lost()).cloned().unwrap_or_default();
    let t = worst;
    let checked: u64 = rep.verify.iter().map(|v| span(&v.tally)).max().unwrap_or(0);
    // A pass that lost (nearly) everything from some offset on, over a real stretch
    // (not one bad last sector): the capacity ends there.
    let tail_of = |t: &crate::engine::Tally| -> Option<u64> {
        let first = t.first_error.as_ref()?.offset;
        let tail = span(t).saturating_sub(first);
        (first > 0 && tail >= (4 * MIB).max(span(t) / 100) && t.lost() as f64 >= (tail / 512) as f64 * 0.9).then_some(first)
    };
    let tail_at = rep.verify.iter().filter_map(|v| tail_of(&v.tally)).min();
    let events: Vec<_> = rep.write.iter().flat_map(|w| &w.events).chain(rep.verify.iter().flat_map(|v| &v.events)).cloned().collect();
    let has = |k: &str| events.iter().any(|e| e.kind == k);
    let early = rep.write.iter().find_map(|w| if let Stop::EarlyAbort(m) = &w.stop { Some(m.clone()) } else { None });
    let cancelled = rep.write.iter().any(|w| w.stop == Stop::Cancelled) || rep.verify.iter().any(|v| v.stop == Stop::Cancelled);
    let lost = t.lost();
    let d = &mut rep.verdict.details;

    if let Some(m) = &early {
        d.push(m.clone());
    }
    if let (Some(w), Some(v)) = (rep.write.last(), rep.verify.last()) {
        d.push(format!("Wrote {} at {:.1} MB/s, read {} at {:.1} MB/s.", size(w.stats.bytes), w.stats.mb_s, size(v.stats.bytes), v.stats.mb_s));
    }
    if lost > 0 {
        let parts: Vec<String> = [
            (t.corrupted, "corrupted"),
            (t.changed, "slightly changed (<8 bits per sector)"),
            (t.aliased, "overwritten by writes to other positions (aliased)"),
            (t.stale, "old data (new data was not stored)"),
            (t.unreadable, "unreadable or missing"),
        ]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{} {what}", size(n * 512)))
        .collect();
        d.push(format!("{} of {} checked was lost: {}.", size(lost * 512), size(checked), parts.join(", ")));
    }
    if has("disconnect") {
        d.push("The drive disconnected during the test. Try another port, cable or card reader.".into());
    }
    if has("unstable") || has("transient-io") {
        d.push("Some reads failed or returned different data on retry. The drive or its connection may be faulty. Try another port, cable or reader.".into());
    }

    let readonly_early = early.as_deref().is_some_and(|m| m.starts_with(crate::engine::IGNORES_WRITES));
    let (status, headline) = if readonly_early || (t.stale > 0 && t.stale * 2 >= lost && lost > 0) {
        (Status::ReadOnly, "The drive ignores writes: data read back is old or never changed.".to_string())
    } else if merged.aliased > 0 || tail_at.is_some() {
        let real = merged.alias_period.or(tail_at).unwrap_or(0);
        (Status::Fake, format!("Fake capacity: only about {} of the claimed {} stores data.", size(real), size(rep.target.claimed)))
    } else if lost > 0 {
        (Status::Defective, format!("Data loss: {} of {} came back wrong or unreadable.", size(lost * 512), size(checked)))
    } else if early.is_some() {
        (Status::Unreliable, "Data written earlier changed during the test but read back correctly later.".to_string())
    } else if cancelled {
        (Status::Cancelled, format!("Cancelled. No errors in the {} checked so far.", size(checked)))
    } else if has("disconnect") || has("write-error") || rep.verify.iter().any(|v| v.stop != Stop::Done) {
        (Status::Unreliable, "The test could not finish: the drive failed or disconnected.".to_string())
    } else if has("transient-io") || has("unstable") || has("io-error") {
        (Status::Unreliable, "All data verified, but reads failed at times. The drive or its connection is unreliable.".to_string())
    } else if checked == 0 {
        (Status::Error, "No test data was checked.".to_string())
    } else {
        let what = if rep.command == "verify" { "read back" } else { "written and read back" };
        (Status::Ok, format!("All {} {what} without errors.", size(checked)))
    };
    rep.set(status, headline);
}

/// What to verify: FlashTester data (manifest) or h2testw files.
pub enum VerifySource {
    Manifest(Manifest),
    H2testw,
}

/// Verify existing test data `loops` times (0 = until an error or cancel).
pub fn run_verify(w: &Where, src: VerifySource, loops: u32, ctx: &Ctx) -> Report {
    let t0 = Instant::now();
    let mut rep = Report::new("verify", w.target());
    let (pat, len, mk): (Pattern, u64, Box<dyn Fn() -> io::Result<Box<dyn Dev>>>) = match (&src, w) {
        (VerifySource::H2testw, Where::Dir { root, .. }) => {
            let (len, n) = FileSet::existing_len(root, "h2w");
            rep.notes.push(format!("Verifying {n} h2testw files (1.h2w ...) with h2testw's fixed pattern."));
            let root = root.clone();
            (Pattern::H2testw, len, Box::new(move || Ok(Box::new(FileSet::h2testw(&root, len)) as Box<dyn Dev>)))
        }
        (VerifySource::Manifest(m), Where::Dir { root, .. }) => {
            let (have, _) = FileSet::existing_len(&data_dir(root), "ftd");
            if have < m.bytes {
                rep.warnings.push(format!("Only {} of the {} written is still present: files are missing or truncated.", size(have), size(m.bytes)));
            }
            let (root, len) = (root.clone(), m.bytes);
            (m.pattern, m.bytes, Box::new(move || Ok(Box::new(FileSet::new(&data_dir(&root), len)) as Box<dyn Dev>)))
        }
        (VerifySource::Manifest(m), Where::Disk(d)) => {
            let d = d.clone();
            (m.pattern, m.bytes, Box::new(move || Ok(Box::new(RawDisk::open(&d, false)?) as Box<dyn Dev>)))
        }
        (VerifySource::H2testw, Where::Disk(_)) => {
            rep.set(Status::Error, "h2testw data can only be verified in a folder.");
            rep.finish(t0);
            return rep;
        }
    };
    if let VerifySource::Manifest(m) = &src {
        rep.notes.push(format!("Data written {} by {}{}.", m.created, m.tool, if m.complete { "" } else { " (write did not finish)" }));
    }
    rep.tested = len;
    let mut i = 1;
    loop {
        if loops != 1 {
            ctx.log(format!("Verify loop {i}"));
        }
        let mut dev = match mk() {
            Ok(d) => d,
            Err(e) => {
                rep.set(Status::Error, format!("Cannot open: {e}"));
                break;
            }
        };
        let vr = verify_pass(dev.as_mut(), pat, 0, len, 8 << 20, ctx);
        let bad = vr.tally.lost() > 0 || vr.stop != Stop::Done;
        rep.passes.push(PassSummary {
            pass: i,
            tag: String::new(),
            bytes: len,
            write_mb_s: 0.0,
            read_mb_s: vr.stats.mb_s,
            lost_sectors: vr.tally.lost(),
            result: if vr.tally.lost() > 0 { format!("{} lost", size(vr.tally.lost() * 512)) } else if vr.stop != Stop::Done { format!("{:?}", vr.stop) } else { "OK".into() },
        });
        rep.verify.push(vr);
        if bad || ctx.cancelled() || i == loops {
            break;
        }
        i += 1;
    }
    if rep.verdict.status != Status::Error {
        judge(&mut rep);
        // In a verify, "stale" means the data is from another run: we can't call that read-only.
        if rep.verdict.status == Status::ReadOnly {
            rep.set(Status::Defective, "The stored data differs from the test data. It may belong to an earlier run, or the drive may have lost writes.");
        }
    }
    rep.finish(t0);
    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::Ctx;
    use crate::dev::{MemDev, SimMode};
    use crate::engine::{verify_pass, write_pass};

    fn judged(real: u64, claimed: u64, mode: SimMode, corrupt_at: Option<u64>) -> Report {
        let ctx = Ctx::new();
        let pat = Pattern::Seeded { tag: 5 };
        let mut d = MemDev::new(real, claimed, mode);
        let w = write_pass(&mut d, pat, 0, claimed, 1 << 20, &ctx, None);
        if let Some(at) = corrupt_at {
            d.data[at as usize] ^= 0xff;
        }
        let v = verify_pass(&mut d, pat, 0, claimed, 1 << 20, &ctx);
        let mut rep = Report::new("test", Target { claimed, ..Default::default() });
        rep.write.push(w);
        rep.verify.push(v);
        judge(&mut rep);
        rep
    }

    /// The OS's own I/O error code (Haiku's errno values are negative).
    #[cfg(windows)]
    const EIO: i32 = 1117;
    #[cfg(unix)]
    const EIO: i32 = libc::EIO;

    /// Reads beyond `real` fail with an I/O error (some fakes do this).
    struct ErrBeyond(MemDev, u64);
    impl crate::dev::Dev for ErrBeyond {
        fn len(&self) -> u64 {
            self.0.len()
        }
        fn write_at(&mut self, off: u64, buf: &[u8]) -> std::io::Result<()> {
            self.0.write_at(off, buf)
        }
        fn read_at(&mut self, off: u64, buf: &mut [u8]) -> std::io::Result<usize> {
            if off >= self.1 { Err(std::io::Error::from_raw_os_error(EIO)) } else { self.0.read_at(off, buf) }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn unreadable_tail_is_fake() {
        let ctx = Ctx::new();
        let pat = Pattern::Seeded { tag: 8 };
        let mut d = ErrBeyond(MemDev::new(128 * MIB, 128 * MIB, SimMode::Genuine), 64 * MIB);
        let w = write_pass(&mut d, pat, 0, 128 * MIB, 1 << 20, &ctx, None);
        let v = verify_pass(&mut d, pat, 0, 128 * MIB, 1 << 20, &ctx);
        let mut rep = Report::new("test", Target { claimed: 128 * MIB, ..Default::default() });
        rep.write.push(w);
        rep.verify.push(v);
        judge(&mut rep);
        assert_eq!(rep.verdict.status, Status::Fake, "{}", rep.verdict.headline);
    }

    #[test]
    fn interior_band_over_passes_is_not_fake() {
        let ctx = Ctx::new();
        let pat = Pattern::Seeded { tag: 9 };
        let mut rep = Report::new("endurance", Target { claimed: 64 * MIB, ..Default::default() });
        for _ in 0..4 {
            let mut d = MemDev::new(64 * MIB, 64 * MIB, SimMode::Genuine);
            rep.write.push(write_pass(&mut d, pat, 0, 64 * MIB, 1 << 20, &ctx, None));
            d.data[(32 * MIB) as usize..(36 * MIB) as usize].fill(0);
            rep.verify.push(verify_pass(&mut d, pat, 0, 64 * MIB, 1 << 20, &ctx));
        }
        judge(&mut rep);
        assert_eq!(rep.verdict.status, Status::Defective, "{}", rep.verdict.headline);
    }

    #[test]
    fn verdicts() {
        assert_eq!(judged(64 * MIB, 64 * MIB, SimMode::Genuine, None).verdict.status, Status::Ok);
        // one bad sector at the very end is a defect, not a fake
        assert_eq!(judged(64 * MIB, 64 * MIB, SimMode::Genuine, Some(64 * MIB - 100)).verdict.status, Status::Defective);
        assert_eq!(judged(64 * MIB, 64 * MIB, SimMode::Genuine, Some(10 * MIB)).verdict.status, Status::Defective);
        assert_eq!(judged(32 * MIB, 128 * MIB, SimMode::Drop, None).verdict.status, Status::Fake);
        assert_eq!(judged(32 * MIB, 128 * MIB, SimMode::Wrap, None).verdict.status, Status::Fake);
        assert_eq!(judged(64 * MIB, 64 * MIB, SimMode::ReadOnly, None).verdict.status, Status::ReadOnly);
    }
}
