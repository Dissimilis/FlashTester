//! Quick fake-capacity probe.
//!
//! Small blocks are written at positions on a power-of-two grid across the
//! claimed capacity and read back, twice: once in ascending order and once in
//! descending order, each with a fresh tag. Cheap fakes ignore high address
//! bits, so the drive wraps around at a power of two (the "module"); with a
//! power-of-two grid a wrapped write lands exactly on another probe, which then
//! holds the wrong position's data (aliased) and gives the module directly.
//! Drives that drop writes or return errors past the real end show up as a
//! tail of bad probes; the boundary is then refined with finer grids.
//!
//! Works on a raw disk (destructive) or, in free-space mode, inside files that
//! fill a volume's free space, mapped back to disk positions.

use crate::ctx::Ctx;
use crate::dev::{AlignedBuf, Dev, GIB, MIB, direct, is_disconnect, pread, pwrite};
use crate::pattern::{Class, Pattern, new_tag};
use crate::platform;
use crate::report::size;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const BLOCK: usize = 4096;

fn ctx_err(what: &str, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

/// What the probe concluded.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Finding {
    #[default]
    Genuine,
    Fake,
    Defective,
    ReadOnly,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ProbeResult {
    /// "raw" or "free-space"
    pub mode: String,
    pub claimed: u64,
    pub step: u64,
    pub block: u64,
    pub positions: Vec<u64>,
    /// One char per position: o = ok, x = lost/corrupted, a = holds another position's data,
    /// s = holds old data (write ignored), e = I/O error.
    pub status: String,
    pub bad: u64,
    pub first_bad: Option<u64>,
    /// Estimated capacity that really stores data.
    pub usable: u64,
    /// Wraparound period, if seen.
    pub module: Option<u64>,
    pub finding: Finding,
    pub seconds: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<u64>,
    pub examples: Vec<String>,
}

impl ProbeResult {
    pub fn summary_lines(&self) -> Vec<String> {
        let mut v = vec![format!(
            "Capacity probe ({}): {} positions every {} across {} claimed, 2 passes, {:.0} s.",
            self.mode,
            self.positions.len(),
            size(self.step),
            size(self.claimed),
            self.seconds
        )];
        match self.finding {
            Finding::Genuine => v.push(format!("All sampled positions retained their data. Sampling interval: {}. Run a full test to check the space between them.", size(self.step))),
            Finding::Fake => {
                v.push(format!("Only about {} really stores data ({:.1}% of the claimed {}).", size(self.usable), self.usable as f64 * 100.0 / self.claimed.max(1) as f64, size(self.claimed)));
                if let Some(m) = self.module {
                    v.push(format!("Writes past that point wrap around and overwrite the start of the drive (period {}).", size(m)));
                }
            }
            Finding::Defective => v.push(format!("{} of {} sampled positions lost data at scattered locations. First failure at {}. The drive or its connection may be faulty.", self.bad, self.positions.len(), size(self.first_bad.unwrap_or(0)))),
            Finding::ReadOnly => v.push("No sampled position retained the test data. The drive may be ignoring writes or may be faulty.".into()),
        }
        for e in self.examples.iter().take(5) {
            v.push(format!("  {e}"));
        }
        v
    }

    pub fn map_html(&self) -> String {
        let mut s = String::from("<div class=map>");
        for (p, c) in self.positions.iter().zip(self.status.chars()) {
            let cls = match c {
                'o' => "p-ok",
                'a' => "p-alias",
                _ => "p-bad",
            };
            s.push_str(&format!("<i class={cls} title=\"{}\"></i>", size(*p)));
        }
        s.push_str("</div>");
        s
    }
}

/// Power-of-two grid of about `points` positions over `[0, claimed)`, plus the last block.
pub fn grid(claimed: u64, block: u64, points: u64) -> (u64, Vec<u64>) {
    if claimed < 2 * block {
        return (block, vec![]);
    }
    let step = (claimed / points).next_power_of_two().max(MIB);
    let mut v: Vec<u64> = (0..).map(|i| i * step).take_while(|&p| p + block <= claimed).collect();
    let last = (claimed - block) / block * block;
    if v.last() != Some(&last) {
        v.push(last);
    }
    (step, v)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Got {
    Ok,
    Bad,
    Alias(u64),
    Stale,
    IoErr,
}

/// Write the given positions in order with a fresh tag, flush, read them all back.
#[allow(clippy::too_many_arguments)]
fn pass(dev: &mut dyn Dev, pos: &[u64], order: &[usize], block: usize, ctx: &Ctx, label: &str, tag: u64, strict: bool) -> io::Result<Vec<Got>> {
    let pat = Pattern::Seeded { tag };
    let mut buf = AlignedBuf::new(block);
    let mut got = vec![Got::Ok; pos.len()];
    ctx.phase(&format!("{label}: writing"), order.len() as u64);
    for (n, &i) in order.iter().enumerate() {
        if ctx.cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        pat.fill(pos[i], &mut buf);
        if let Err(e) = dev.write_at(pos[i], &buf) {
            if strict || is_disconnect(&e) || e.kind() == io::ErrorKind::TimedOut {
                return Err(e);
            }
            got[i] = Got::IoErr;
        }
        ctx.progress(n as u64 + 1);
    }
    dev.flush()?;
    ctx.phase(&format!("{label}: reading"), order.len() as u64);
    for (n, &i) in order.iter().enumerate() {
        if ctx.cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        if got[i] == Got::IoErr {
            continue;
        }
        got[i] = match dev.read_at(pos[i], &mut buf) {
            Err(e) if strict || is_disconnect(&e) || e.kind() == io::ErrorKind::TimedOut => return Err(e),
            Err(_) => Got::IoErr,
            Ok(r) if r < block => Got::IoErr,
            Ok(_) => classify_block(pat, pos[i], &buf),
        };
        ctx.progress(n as u64 + 1);
    }
    Ok(got)
}

fn classify_block(pat: Pattern, pos: u64, buf: &[u8]) -> Got {
    if pat.buffer_ok(pos, buf) {
        return Got::Ok;
    }
    let (c, _) = pat.classify(pos, &buf[..512]);
    match c {
        Class::Aliased(src) => Got::Alias(src),
        Class::Stale => Got::Stale,
        _ => Got::Bad,
    }
}

/// Status of one position from the ascending pass `a` and the descending pass `b`.
/// In the ascending pass a real position can be overwritten by a later write to a
/// higher position that wraps onto it: that makes it a victim, not a bad position.
fn status_char(pos: u64, a: Got, b: Got) -> char {
    let a = match a {
        Got::Alias(src) if src > pos => Got::Ok,
        g => g,
    };
    match (a, b) {
        (Got::Ok, Got::Ok) => 'o',
        (Got::Alias(_), _) | (_, Got::Alias(_)) => 'a',
        (Got::IoErr, _) | (_, Got::IoErr) => 'e',
        (Got::Stale, _) | (_, Got::Stale) => 's',
        _ => 'x',
    }
}

/// Run the probe over `positions` (sorted) of a device claiming `claimed` bytes.
/// Time a few scattered writes; on drives with very slow random writes, keep
/// every k-th grid position (k a power of two, so the grid stays a power-of-two
/// grid) to finish both passes in about `budget_s` seconds.
fn thin_for_speed(dev: &mut dyn Dev, positions: Vec<u64>, step: u64, block: usize, budget_s: f64, strict: bool, ctx: &Ctx) -> io::Result<(Vec<u64>, u64)> {
    if positions.len() <= 64 {
        return Ok((positions, step));
    }
    let mut buf = AlignedBuf::new(block);
    let n = positions.len();
    // Median of 8 timed write+read pairs: the first access is often slow (wake-up).
    let samples = 8;
    let mut times = vec![];
    for i in 0..samples {
        let pos = positions[n * (2 * i + 1) / (2 * samples)];
        let t = Instant::now();
        // A fake may error past its real end: that is for the passes to classify.
        let pair = dev.write_at(pos, &buf).and_then(|_| dev.read_at(pos, &mut buf));
        if strict && let Err(e) = pair { return Err(e); }
        if pair.is_ok() {
            times.push(t.elapsed().as_secs_f64());
        }
    }
    if times.len() < 3 {
        return Ok((positions, step));
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let per_write = times[times.len() / 2];
    let max_positions = (budget_s / (2.0 * per_write)).max(64.0) as usize;
    let mut k = 1usize;
    while n / k > max_positions && n / (k * 2) >= 64 {
        k *= 2;
    }
    if k == 1 {
        return Ok((positions, step));
    }
    ctx.log(format!("Random writes are slow on this drive ({:.0} ms each): probing every {} instead of every {}", per_write * 1000.0, size(step * k as u64), size(step)));
    let last = *positions.last().unwrap();
    let mut v: Vec<u64> = positions.iter().copied().filter(|p| (p / step) % k as u64 == 0).collect();
    if v.last() != Some(&last) {
        v.push(last);
    }
    Ok((v, step * k as u64))
}

pub fn run(dev: &mut dyn Dev, positions: Vec<u64>, step: u64, claimed: u64, mode: &str, ctx: &Ctx) -> io::Result<ProbeResult> {
    run_inner(dev, positions, step, claimed, mode, None, ctx)
}

/// Diagnostics supplies a deadline-enforcing Dev; established fakes need no refinement.
pub fn run_limited(dev: &mut dyn Dev, positions: Vec<u64>, step: u64, budget_s: f64, ctx: &Ctx) -> io::Result<ProbeResult> {
    let claimed = dev.len();
    run_inner(dev, positions, step, claimed, "raw", Some(budget_s), ctx)
}

#[allow(clippy::too_many_arguments)]
fn run_inner(dev: &mut dyn Dev, positions: Vec<u64>, step: u64, claimed: u64, mode: &str, budget: Option<f64>, ctx: &Ctx) -> io::Result<ProbeResult> {
    let t0 = Instant::now();
    if positions.is_empty() {
        return Err(io::Error::other("no probe positions (device or free space too small)"));
    }
    let block = BLOCK.max(dev.align());
    let (positions, step) = thin_for_speed(dev, positions, step, block, budget.unwrap_or(300.0), budget.is_some(), ctx)?;
    let asc: Vec<usize> = (0..positions.len()).collect();
    let desc: Vec<usize> = asc.iter().rev().copied().collect();
    let a = pass(dev, &positions, &asc, block, ctx, "Probe pass 1/2", new_tag(), budget.is_some())?;
    let tag = new_tag();
    let b = pass(dev, &positions, &desc, block, ctx, "Probe pass 2/2", tag, budget.is_some())?;

    let mut r = ProbeResult { mode: mode.into(), claimed, step, block: block as u64, tag: Some(tag), ..Default::default() };
    r.status = a.iter().zip(&b).enumerate().map(|(i, (&x, &y))| status_char(positions[i], x, y)).collect();
    for (i, (&x, &y)) in a.iter().zip(&b).enumerate() {
        for g in [x, y] {
            if let Got::Alias(src) = g {
                let d = src.abs_diff(positions[i]);
                if d > 0 && r.module.is_none_or(|m| d < m) {
                    r.module = Some(d);
                }
                if r.examples.len() < 5 {
                    r.examples.push(format!("Position {} returned the data written to {}", size(positions[i]), size(src)));
                }
            }
        }
    }
    // A wrap shorter than the grid step makes every grid point land on the same spot,
    // so the smallest visible distance is the step itself: look again, finer, below it.
    while let Some(m) = r.module.filter(|&m| budget.is_none() && m <= step.max(r.step) && m > MIB) {
        let (fine_step, fine) = grid(2 * m, block as u64, 1024);
        let asc: Vec<usize> = (0..fine.len()).collect();
        let desc: Vec<usize> = asc.iter().rev().copied().collect();
        let fa = pass(dev, &fine, &asc, block, ctx, "Probe: narrowing the wraparound", new_tag(), false)?;
        let fb = pass(dev, &fine, &desc, block, ctx, "Probe: narrowing the wraparound", new_tag(), false)?;
        let smaller = fa.iter().chain(&fb).zip(fine.iter().chain(&fine)).filter_map(|(g, &p)| if let Got::Alias(src) = g { Some(src.abs_diff(p)) } else { None }).filter(|&d| d > 0 && d < m).min();
        match smaller {
            Some(d) => {
                r.examples.push(format!("A finer probe found the drive repeats every {}", size(d)));
                r.module = Some(d);
                r.step = r.step.min(fine_step);
            }
            None => break,
        }
    }
    let bad: Vec<usize> = r.status.char_indices().filter(|(_, c)| *c != 'o').map(|(i, _)| i).collect();
    r.bad = bad.len() as u64;
    r.first_bad = bad.first().map(|&i| positions[i]);
    r.usable = claimed;
    r.finding = Finding::Genuine;
    if let Some(&fb) = bad.first() {
        let after = positions.len() - fb;
        let tail_bad = bad.len() as f64 >= after as f64 * 0.9 && (after >= 4 || r.module.is_some());
        if bad.len() == positions.len() {
            r.finding = Finding::ReadOnly;
            r.usable = 0;
        } else if let Some(m) = r.module {
            // Data turning up at another position proves wraparound: fake capacity,
            // even when an isolated bad block lies below the boundary.
            r.finding = Finding::Fake;
            r.usable = m;
        } else if tail_bad && fb > 0 {
            r.finding = Finding::Fake;
            r.usable = if budget.is_some() { positions[fb] } else { refine(dev, positions[fb - 1], positions[fb], block, ctx)? };
        } else {
            r.finding = Finding::Defective;
            r.usable = positions[fb];
        }
    }
    r.positions = positions;
    r.seconds = t0.elapsed().as_secs_f64();
    Ok(r)
}

/// Narrow down the end of the real capacity between a good and a bad position.
fn refine(dev: &mut dyn Dev, mut lo: u64, mut hi: u64, block: usize, ctx: &Ctx) -> io::Result<u64> {
    let b = block as u64;
    for round in 1..=4 {
        if hi - lo <= MIB {
            break;
        }
        let pts: Vec<u64> = (1..32u64).map(|k| (lo + (hi - lo) * k / 32) / b * b).filter(|&p| p > lo && p < hi && dev.usable(p, b)).collect();
        if pts.is_empty() {
            break;
        }
        let asc: Vec<usize> = (0..pts.len()).collect();
        let desc: Vec<usize> = asc.iter().rev().copied().collect();
        let g = pass(dev, &pts, &desc, block, ctx, &format!("Refining boundary {round}/4"), new_tag(), false)?;
        match g.iter().position(|&x| x != Got::Ok) {
            Some(0) => hi = pts[0],
            Some(i) => {
                lo = pts[i - 1];
                hi = pts[i];
            }
            None => lo = *pts.last().unwrap(),
        }
    }
    Ok((lo + b) / MIB * MIB)
}

/// Free-space mode: files fill the volume's free space; probe positions are
/// disk offsets inside them.
pub struct FreeSpaceDev {
    files: Vec<File>,
    pub paths: Vec<PathBuf>,
    /// (disk offset, length, file index, file offset), sorted by disk offset.
    map: Vec<(u64, u64, usize, u64)>,
    disk_len: u64,
}

impl FreeSpaceDev {
    /// Fill the free space of the volume at `root` with preallocated files under `dir`.
    /// Needs admin: files are allocated without zero-filling them.
    pub fn create(root: &Path, dir: &Path, vol: &platform::VolumeInfo, disk: &platform::DiskInfo, ctx: &Ctx) -> io::Result<FreeSpaceDev> {
        platform::free_space_prepare()?;
        crate::fulltest::make_dir(dir).map_err(|e| ctx_err("create folder", e))?;
        let unit = platform::alloc_unit(root)?.max(4096);
        let want_fs = platform::fs_id_path(dir)?;
        let free = platform::free_space(root)?;
        let mut left = free.saturating_sub(64 * MIB) / unit * unit;
        let mut d = FreeSpaceDev { files: vec![], paths: vec![], map: vec![], disk_len: disk.size };
        ctx.phase("Allocating free space", left);
        let total = left;
        while left >= unit {
            // FAT caps files at 4 GiB; elsewhere big files keep the number of open handles low.
            let fat = vol.fs.to_ascii_lowercase().contains("fat") && !vol.fs.eq_ignore_ascii_case("exfat");
            let len = left.min(if fat { GIB } else { 32 * GIB });
            let p = dir.join(format!("probe{:05}.ftp", d.files.len() + 1));
            crate::dev::no_link(&p)?;
            let _ = std::fs::remove_file(&p);
            let f = direct(OpenOptions::new().read(true).write(true).create_new(true), true).open(&p).map_err(|e| ctx_err("create probe file", e))?;
            if platform::fs_id(&f)? != want_fs {
                return Err(io::Error::other(format!("{} is not on the drive under test", p.display())));
            }
            d.paths.push(p);
            for (fo, disk_off, n) in platform::allocate_and_map(&f, len, root, vol).map_err(|e| ctx_err("allocate and map probe file", e))? {
                d.map.push((disk_off, n, d.files.len(), fo));
            }
            d.files.push(f);
            left -= len;
            ctx.progress(total - left);
        }
        d.map.sort_by_key(|m| m.0);
        d.check_mapping(&disk.path)?;
        Ok(d)
    }

    /// Write through the files and read the same spots from the raw disk: proves
    /// the file-offset -> disk-position mapping before any conclusion relies on it.
    fn check_mapping(&mut self, disk_path: &str) -> io::Result<()> {
        if self.map.is_empty() {
            return Err(io::Error::other("no free space could be allocated"));
        }
        let raw = direct(OpenOptions::new().read(true), false).open(disk_path).map_err(|e| ctx_err("open disk for map check", e))?;
        let pat = Pattern::Seeded { tag: new_tag() };
        let mut buf = AlignedBuf::new(BLOCK);
        let n = self.map.len();
        for &i in &[0, n / 2, n - 1] {
            let pos = self.map[i].0;
            pat.fill(pos, &mut buf);
            self.write_at(pos, &buf).map_err(|e| ctx_err("map check write", e))?;
            self.flush()?;
            pread(&raw, &mut buf, pos).map_err(|e| ctx_err("map check read", e))?;
            if !pat.buffer_ok(pos, &buf) {
                return Err(io::Error::other(format!("free-space map check failed at {}: file data is not where the file system says", size(pos))));
            }
        }
        Ok(())
    }

    fn locate(&self, pos: u64, len: u64) -> Option<(usize, u64)> {
        let i = self.map.partition_point(|m| m.0 <= pos).checked_sub(1)?;
        let (d, l, f, fo) = self.map[i];
        (pos + len <= d + l).then_some((f, fo + pos - d))
    }

    /// Grid positions that fall inside the allocated files.
    pub fn positions(&self, block: u64) -> (u64, Vec<u64>) {
        let (step, g) = grid(self.disk_len, block, 1024);
        (step, g.into_iter().filter(|&p| self.locate(p, block).is_some()).collect())
    }

    pub fn remove(self) {
        let paths = self.paths.clone();
        drop(self.files);
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }
}

impl Dev for FreeSpaceDev {
    fn len(&self) -> u64 {
        self.disk_len
    }
    fn usable(&self, off: u64, len: u64) -> bool {
        self.locate(off, len).is_some()
    }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        let (f, fo) = self.locate(off, buf.len() as u64).ok_or_else(|| io::Error::other("position outside free space"))?;
        pwrite(&self.files[f], buf, fo).map(|_| ())
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        let (f, fo) = self.locate(off, buf.len() as u64).ok_or_else(|| io::Error::other("position outside free space"))?;
        pread(&self.files[f], buf, fo)
    }
    fn flush(&mut self) -> io::Result<()> {
        for f in &self.files {
            f.sync_all()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::{MemDev, SimMode};

    fn probe(real: u64, claimed: u64, mode: SimMode) -> ProbeResult {
        let mut d = MemDev::new(real, claimed, mode);
        let (step, pos) = grid(claimed, BLOCK as u64, 256);
        run(&mut d, pos, step, claimed, "raw", &Ctx::new()).unwrap()
    }

    #[test]
    fn genuine_drive() {
        let r = probe(512 * MIB, 512 * MIB, SimMode::Genuine);
        assert_eq!(r.finding, Finding::Genuine);
        assert_eq!(r.usable, 512 * MIB);
    }

    #[test]
    fn wraparound_fake() {
        let r = probe(256 * MIB, 2 * GIB, SimMode::Wrap);
        assert_eq!(r.finding, Finding::Fake);
        assert_eq!(r.module, Some(256 * MIB));
        assert_eq!(r.usable, 256 * MIB);
    }

    #[test]
    fn dropping_fake_is_refined() {
        let real = 300 * MIB + 4096 * 10;
        let r = probe(real, 2 * GIB, SimMode::Drop);
        assert_eq!(r.finding, Finding::Fake);
        assert!(r.usable <= real && real - r.usable <= MIB, "usable {} real {}", r.usable, real);
    }

    #[test]
    fn read_only_drive() {
        let r = probe(512 * MIB, 512 * MIB, SimMode::ReadOnly);
        assert_eq!(r.finding, Finding::ReadOnly);
    }
}

#[cfg(test)]
mod edge_tests {
    use super::*;
    use crate::dev::{MemDev, SimMode};

    #[test]
    fn tiny_device_is_an_error_not_genuine() {
        let (_, pos) = grid(4096, BLOCK as u64, 1024);
        assert!(pos.is_empty());
        let mut d = MemDev::new(4096, 4096, SimMode::Genuine);
        assert!(run(&mut d, pos, 4096, 4096, "raw", &Ctx::new()).is_err());
    }
}

#[cfg(test)]
mod wrap_with_bad_block {
    use super::*;
    use crate::dev::{MemDev, SimMode};

    /// A wrapping fake that also returns garbage for one block below its real capacity.
    struct BadAt(MemDev, u64);
    impl Dev for BadAt {
        fn len(&self) -> u64 {
            self.0.len()
        }
        fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
            self.0.write_at(off, buf)
        }
        fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.0.read_at(off, buf)?;
            if off == self.1 {
                buf.fill(0);
            }
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn module_wins_over_an_isolated_bad_block() {
        let (real, claimed) = (256 * MIB, 2 * GIB);
        let (step, pos) = grid(claimed, BLOCK as u64, 256);
        let mut d = BadAt(MemDev::new(real, claimed, SimMode::Wrap), 2 * step);
        let r = run(&mut d, pos, step, claimed, "raw", &Ctx::new()).unwrap();
        assert_eq!(r.finding, Finding::Fake);
        assert_eq!(r.usable, 256 * MIB);
    }
}

#[cfg(test)]
mod small_wrap {
    use super::*;
    use crate::dev::{MemDev, SimMode};

    #[test]
    fn wrap_smaller_than_grid_step_is_found() {
        // 2 GiB claimed, 64-point grid -> 32 MiB step; the drive wraps every 8 MiB.
        let (real, claimed) = (8 * MIB, 2 * GIB);
        let (step, pos) = grid(claimed, BLOCK as u64, 64);
        assert!(step > real);
        let mut d = MemDev::new(real, claimed, SimMode::Wrap);
        let r = run(&mut d, pos, step, claimed, "raw", &Ctx::new()).unwrap();
        assert_eq!(r.finding, Finding::Fake);
        assert_eq!(r.module, Some(8 * MIB));
        assert_eq!(r.usable, 8 * MIB);
    }
}
