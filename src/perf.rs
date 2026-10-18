//! Performance tests: random 4K IOPS at queue depth 1 and 32, and the
//! many-small-files test (file-system metadata integrity and files/s).

use crate::ctx::Ctx;
use crate::dev::{AlignedBuf, MIB, direct, pread, pwrite};
use crate::pattern::{Pattern, new_tag};
use crate::report::size;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct IopsRow {
    pub kind: String,
    pub qd: u32,
    pub iops: f64,
    pub mb_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct IopsResult {
    /// Region the random offsets fall in.
    pub span: u64,
    pub seconds_each: f64,
    pub rows: Vec<IopsRow>,
}

const IO: usize = 4096;

pub fn iops_duration(secs: f64) -> io::Result<Duration> {
    let duration = Duration::try_from_secs_f64(secs).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "IOPS duration must be finite and positive"))?;
    if duration.is_zero() || Instant::now().checked_add(duration).is_none() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "IOPS duration must be positive and within the clock's range"));
    }
    Ok(duration)
}

/// One random-I/O run: `qd` threads, each with its own handle, for `secs`.
fn run_one(open: &(dyn Fn(bool) -> io::Result<File> + Sync), span: u64, write: bool, qd: u32, secs: f64, ctx: &Ctx) -> io::Result<IopsRow> {
    let duration = iops_duration(secs)?;
    if span < IO as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "IOPS needs at least 4 KiB"));
    }
    let started = Instant::now();
    let deadline = started.checked_add(duration).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "IOPS duration is too large"))?;
    let blocks = span / IO as u64;
    let results: Vec<io::Result<Vec<f32>>> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..qd)
            .map(|_| {
                s.spawn(move || -> io::Result<Vec<f32>> {
                    let f = open(write)?;
                    let mut buf = AlignedBuf::new(IO);
                    Pattern::Seeded { tag: new_tag() }.fill(0, &mut buf);
                    let mut x = new_tag();
                    let mut lat = vec![];
                    while Instant::now() < deadline && !ctx.cancelled() {
                        x = crate::pattern::xorshift(x);
                        let off = (x % blocks) * IO as u64;
                        let t = Instant::now();
                        if write {
                            if pwrite(&f, &buf, off)? != IO {
                                return Err(io::Error::new(io::ErrorKind::WriteZero, "short IOPS write"));
                            }
                        } else {
                            if pread(&f, &mut buf, off)? != IO {
                                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short IOPS read"));
                            }
                        }
                        lat.push(t.elapsed().as_secs_f32() * 1000.0);
                    }
                    Ok(lat)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let elapsed = started.elapsed().as_secs_f64();
    let mut lat = vec![];
    for r in results {
        lat.extend(r?);
    }
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = lat.len().max(1);
    let pct = |p: f64| lat.get(((n as f64 * p) as usize).min(n - 1)).copied().unwrap_or(0.0) as f64;
    let iops = lat.len() as f64 / elapsed.max(1e-9);
    Ok(IopsRow {
        kind: if write { "write" } else { "read" }.into(),
        qd,
        iops,
        mb_s: iops * IO as f64 / 1e6,
        p50_ms: pct(0.5),
        p99_ms: pct(0.99),
        max_ms: lat.last().copied().unwrap_or(0.0) as f64,
    })
}

/// Random 4K reads (and writes if `write`) at QD1 and QD32, `secs` per run.
pub fn run_iops(open: &(dyn Fn(bool) -> io::Result<File> + Sync), span: u64, write: bool, secs: f64, ctx: &Ctx) -> io::Result<IopsResult> {
    let mut r = IopsResult { span, seconds_each: secs, rows: vec![] };
    let kinds: &[bool] = if write { &[false, true] } else { &[false] };
    for &w in kinds {
        for qd in [1, 32] {
            ctx.phase(&format!("Random 4K {} QD{qd}", if w { "write" } else { "read" }), 0);
            let row = run_one(open, span, w, qd, secs, ctx)?;
            ctx.log(format!("4K random {} QD{}: {:.0} IOPS ({:.2} MB/s), p99 {:.1} ms", row.kind, qd, row.iops, row.mb_s, row.p99_ms));
            r.rows.push(row);
            if ctx.cancelled() {
                return Ok(r);
            }
        }
    }
    Ok(r)
}

/// Create (sequentially, unbuffered) a test file of `len` bytes for file-mode IOPS.
pub fn make_iops_file(path: &Path, len: u64, ctx: &Ctx) -> io::Result<()> {
    crate::dev::no_link(path)?;
    // Never write through an existing name: it could be a hard link to another file.
    let _ = std::fs::remove_file(path);
    let want = crate::platform::fs_id_path(path.parent().unwrap_or(path))?;
    let f = direct(OpenOptions::new().write(true).create_new(true), false).open(path)?;
    if crate::platform::fs_id(&f)? != want {
        return Err(io::Error::other(format!("{} is not on the drive under test", path.display())));
    }
    let mut buf = AlignedBuf::new(8 * MIB as usize);
    Pattern::Seeded { tag: new_tag() }.fill(0, &mut buf);
    ctx.phase("Preparing IOPS test file", len);
    let mut off = 0;
    while off < len {
        if ctx.cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let n = (buf.len() as u64).min(len - off) as usize;
        pwrite(&f, &buf[..n], off)?;
        off += n as u64;
        ctx.progress(off);
    }
    f.sync_all()
}

pub fn open_unbuffered(path: &Path, write: bool) -> io::Result<File> {
    direct(OpenOptions::new().read(true).write(write), true).open(path)
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SmallFilesResult {
    pub files: u64,
    pub bytes: u64,
    pub create_per_s: f64,
    pub rename_per_s: f64,
    pub delete_per_s: f64,
    pub verify_per_s: f64,
    pub verified: u64,
    pub bad_content: u64,
    pub missing: u64,
    /// Files that should be gone (renamed away or deleted) but are still listed.
    pub leftovers: u64,
    pub errors: Vec<String>,
}

impl SmallFilesResult {
    pub fn ok(&self) -> bool {
        self.bad_content == 0 && self.missing == 0 && self.leftovers == 0 && self.errors.is_empty()
    }
    pub fn summary_lines(&self) -> Vec<String> {
        let mut v = vec![
            format!("Small files: {} files ({}) created at {:.0} files/s", self.files, size(self.bytes), self.create_per_s),
            format!("Renamed at {:.0} files/s, deleted at {:.0} files/s, verified {} files at {:.0} files/s", self.rename_per_s, self.delete_per_s, self.verified, self.verify_per_s),
        ];
        if self.ok() {
            v.push("File names and contents match. Deleted files and old names are gone.".into());
        } else {
            v.push(format!("Incorrect contents: {} files. Missing: {}. Left after deletion or renaming: {}.", self.bad_content, self.missing, self.leftovers));
            for e in self.errors.iter().take(10) {
                v.push(format!("  {e}"));
            }
        }
        v
    }
}

fn file_size(tag: u64, i: u64) -> usize {
    // 512 B .. 64 KiB, deliberately not sector-aligned
    let x = (tag ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    (512 + (x >> 33) % (64 * 1024 - 512)) as usize
}

fn content(pat: Pattern, i: u64, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len.div_ceil(512) * 512];
    pat.fill(i * 65536, &mut v);
    v.truncate(len);
    v
}

/// Create `count` small files, rename every other one, delete every fourth,
/// then check names and contents with unbuffered reads.
pub fn run_small_files(dir: &Path, count: u64, ctx: &Ctx) -> SmallFilesResult {
    let mut r = SmallFilesResult::default();
    let tag = new_tag();
    let pat = Pattern::Seeded { tag };
    if let Err(e) = crate::fulltest::remove_data_dir(dir).and_then(|_| crate::fulltest::make_dir(dir)) {
        r.errors.push(format!("cannot create {}: {e}", dir.display()));
        return r;
    }
    let name = |i: u64, renamed: bool| dir.join(if renamed { format!("r{i:06}.dat") } else { format!("f{i:06}.dat") });
    let renamed = |i: u64| i.is_multiple_of(2);
    let deleted = |i: u64| i % 4 == 1;

    ctx.phase("Creating small files", count);
    let t = Instant::now();
    for i in 0..count {
        if ctx.cancelled() {
            return r;
        }
        let data = content(pat, i, file_size(tag, i));
        if let Err(e) = std::fs::write(name(i, false), &data) {
            r.errors.push(format!("create f{i:06}: {e}"));
            return r;
        }
        r.files += 1;
        r.bytes += data.len() as u64;
        ctx.progress(i + 1);
    }
    r.create_per_s = count as f64 / t.elapsed().as_secs_f64();

    ctx.phase("Renaming", count / 2);
    let t = Instant::now();
    let mut n = 0;
    for i in (0..count).filter(|&i| renamed(i)) {
        if let Err(e) = std::fs::rename(name(i, false), name(i, true)) {
            r.errors.push(format!("rename f{i:06}: {e}"));
        }
        n += 1;
        ctx.progress(n);
    }
    r.rename_per_s = n as f64 / t.elapsed().as_secs_f64();

    ctx.phase("Deleting", count / 4);
    let t = Instant::now();
    let mut n = 0;
    for i in (0..count).filter(|&i| deleted(i)) {
        if let Err(e) = std::fs::remove_file(name(i, renamed(i))) {
            r.errors.push(format!("delete f{i:06}: {e}"));
        }
        n += 1;
        ctx.progress(n);
    }
    r.delete_per_s = n as f64 / t.elapsed().as_secs_f64();

    ctx.phase("Verifying small files", count);
    let t = Instant::now();
    let mut buf = AlignedBuf::new(64 * 1024 + 4096);
    let listed: std::collections::HashSet<String> = std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect()).unwrap_or_default();
    for i in 0..count {
        let fname = |ren| name(i, ren).file_name().unwrap().to_string_lossy().to_string();
        if deleted(i) {
            r.leftovers += listed.contains(&fname(renamed(i))) as u64;
            continue;
        }
        if renamed(i) && listed.contains(&fname(false)) {
            r.leftovers += 1;
        }
        let p = name(i, renamed(i));
        let want = content(pat, i, file_size(tag, i));
        // Unbuffered read of a rounded-up length returns the real file size.
        // A short count means end of file; a second read would be unaligned.
        let got = direct(OpenOptions::new().read(true), false).open(&p).and_then(|f| {
            let n = want.len().div_ceil(4096) * 4096;
            pread(&f, &mut buf[..n], 0)
        });
        match got {
            Ok(len) if len == want.len() && buf[..len] == want[..] => r.verified += 1,
            Ok(len) => {
                r.bad_content += 1;
                if r.errors.len() < 20 {
                    r.errors.push(format!("{}: wrong content ({} bytes, expected {})", p.display(), len, want.len()));
                }
            }
            Err(e) => {
                r.missing += 1;
                if r.errors.len() < 20 {
                    r.errors.push(format!("{}: {e}", p.display()));
                }
            }
        }
        ctx.progress(i + 1);
    }
    r.verify_per_s = r.verified as f64 / t.elapsed().as_secs_f64();
    r
}

/// Sequential speed only: says nothing about whether the capacity is real.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct BenchResult {
    pub size: u64,
    pub block: u64,
    pub write: Option<crate::engine::IoStats>,
    pub read: crate::engine::IoStats,
}

impl BenchResult {
    pub fn summary_lines(&self) -> Vec<String> {
        let mut v = vec![];
        let line = |k: &str, st: &crate::engine::IoStats| {
            let speeds: Vec<f64> = st.speed_map.iter().map(|p| p.mb_s).collect();
            let lo = speeds.iter().cloned().fold(f64::INFINITY, f64::min);
            let hi = speeds.iter().cloned().fold(0.0, f64::max);
            format!("Sequential {k}: {:.1} MB/s over {} (range {:.0}-{:.0} MB/s, slowest I/O {:.0} ms)", st.mb_s, size(st.bytes), if lo.is_finite() { lo } else { 0.0 }, hi, st.worst_ms)
        };
        if let Some(w) = &self.write {
            v.push(line("write", w));
        }
        v.push(line("read", &self.read));
        v.push(format!("Block size {}. This measures speed only; use test or probe to check the capacity.", size(self.block)));
        v
    }
}

/// Read `[0, len)` sequentially and time it (raw read-only benchmark).
pub fn seq_read(dev: &mut dyn crate::dev::Dev, len: u64, block: usize, ctx: &Ctx) -> io::Result<crate::engine::IoStats> {
    if len == 0 || block == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "read size and block size must be positive"));
    }
    let mut st = crate::engine::IoStats::default();
    let mut buf = AlignedBuf::new(block);
    ctx.phase("Sequential read", len);
    let mut off = 0;
    while off < len && !ctx.cancelled() {
        let n = (block as u64).min(len - off) as usize;
        let t = Instant::now();
        let got = dev.read_at(off, &mut buf[..n])?;
        let secs = t.elapsed().as_secs_f64();
        st.record(off, got, secs);
        ctx.sample(off, got as f64 / 1e6 / secs.max(1e-9));
        if got != n {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("short read at {off}: received {got} of {n} bytes")));
        }
        off += got as u64;
        ctx.progress(off);
    }
    st.finish(100);
    Ok(st)
}
