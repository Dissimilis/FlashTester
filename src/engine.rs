//! Write and verify passes over a test stream, with timing, classification and
//! read-failure diagnosis. Shared by file mode and raw mode.

use crate::ctx::Ctx;
use crate::dev::{AlignedBuf, Dev, MIB, is_disconnect, is_disk_full};
use crate::pattern::{Class, Pattern, SECTOR};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::mpsc::sync_channel;
use std::time::Instant;

/// One monotonic clock for I/O timing, phase deadlines and simulated idle time.
#[derive(Clone)]
pub struct Clock {
    start: Instant,
    completed: std::sync::Arc<std::sync::Mutex<f64>>,
    #[cfg(test)]
    pub ticks: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

impl Default for Clock {
    fn default() -> Self { Self { start: Instant::now(), completed: Default::default(), #[cfg(test)] ticks: None } }
}

impl Clock {
    pub fn now(&self) -> f64 {
        #[cfg(test)]
        if let Some(t) = &self.ticks { return t.load(std::sync::atomic::Ordering::SeqCst) as f64 / 1e9; }
        self.start.elapsed().as_secs_f64()
    }
    pub fn complete(&self, start: f64, at: f64) -> f64 {
        let mut last = self.completed.lock().unwrap();
        let gap = at - last.max(start);
        *last = last.max(at);
        gap
    }
    pub fn sleep(&self, secs: f64) {
        #[cfg(test)]
        if let Some(t) = &self.ticks { t.fetch_add((secs * 1e9).ceil().max(1.0) as u64, std::sync::atomic::Ordering::SeqCst); return; }
        std::thread::sleep(std::time::Duration::from_secs_f64(secs.max(0.0)));
    }
    #[cfg(test)]
    pub fn simulated() -> Self { Self { start: Instant::now(), completed: Default::default(), ticks: Some(Default::default()) } }
}

#[derive(Clone, Debug)]
pub struct Completion {
    pub off: u64,
    pub len: usize,
    pub seconds: f64,
    pub at: f64,
    pub gap: f64,
    pub tally: Option<Tally>,
}

/// Bounded sequential I/O with independent handles. Drain before returning so
/// read pauses and wrap checks are real fences. Reads verify the expected pattern.
#[allow(clippy::too_many_arguments)]
pub fn write_window(dev: &mut dyn Dev, pat: Pattern, start: u64, end: u64, block: usize, qd: usize, deadline: f64, clock: &Clock, ctx: &Ctx) -> (Vec<Completion>, std::io::Result<()>) {
    io_window(dev, pat, start, end, block, qd, deadline, clock, ctx, true)
}

#[allow(clippy::too_many_arguments)]
pub fn read_window(dev: &mut dyn Dev, pat: Pattern, start: u64, end: u64, block: usize, qd: usize, deadline: f64, clock: &Clock, ctx: &Ctx) -> (Vec<Completion>, std::io::Result<()>) {
    io_window(dev, pat, start, end, block, qd, deadline, clock, ctx, false)
}

fn window_io(dev: &mut dyn Dev, pat: Pattern, off: u64, buf: &mut [u8], write: bool, clock: &Clock) -> (Completion, std::io::Result<()>) {
    if write { pat.fill(off, buf); }
    let t = clock.now();
    let mut result = if write { dev.write_at(off, buf) } else {
        dev.read_at(off, buf).and_then(|n| if n == buf.len() { Ok(()) } else { Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short sequential read")) })
    };
    let at = clock.now();
    let mut c = Completion { off, len: if result.is_ok() { buf.len() } else { 0 }, seconds: at - t, at, gap: clock.complete(t, at), tally: None };
    if !write && result.is_ok() {
        let mut tally = Tally::default();
        if !tally.check(pat, off, buf) { result = Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Written data changed at byte {off}."))); }
        c.tally = Some(tally);
    }
    (c, result)
}

#[allow(clippy::too_many_arguments)]
fn io_window(dev: &mut dyn Dev, pat: Pattern, start: u64, end: u64, block: usize, qd: usize, deadline: f64, clock: &Clock, ctx: &Ctx, write: bool) -> (Vec<Completion>, std::io::Result<()>) {
    let mut completed = vec![];
    let mut result = Ok(());
    if qd == 1 {
        let mut buf = AlignedBuf::new(block);
        let mut off = start;
        while off < end && clock.now() < deadline && !ctx.cancelled() {
            let n = (end - off).min(block as u64) as usize;
            let (c, r) = window_io(dev, pat, off, &mut buf[..n], write, clock);
            result = r;
            completed.push(c);
            if result.is_err() || completed.last().unwrap().gap >= 10.0 { break; }
            off += n as u64;
        }
        return (completed, result);
    }
    let handles: std::io::Result<Vec<_>> = (0..qd).map(|_| dev.fork()).collect();
    let handles = match handles { Ok(h) => h, Err(e) => return (completed, Err(e)) };
    std::thread::scope(|scope| {
        let (tx, rx) = sync_channel(qd);
        let mut queues = vec![];
        for (id, mut handle) in handles.into_iter().enumerate() {
            let (work, jobs) = sync_channel::<(u64, usize)>(1);
            queues.push(work);
            let tx = tx.clone();
            scope.spawn(move || {
                let mut buf = AlignedBuf::new(block);
                while let Ok((off, n)) = jobs.recv() {
                    let (c, r) = window_io(&mut *handle, pat, off, &mut buf[..n], write, clock);
                    if tx.send((id, c, r)).is_err() { break; }
                }
            });
        }
        drop(tx);
        let mut next = start;
        let mut pending = 0;
        for q in &queues {
            if next >= end || clock.now() >= deadline || ctx.cancelled() { break; }
            let n = (end - next).min(block as u64) as usize;
            q.send((next, n)).unwrap();
            next += n as u64;
            pending += 1;
        }
        let mut stalled = false;
        while pending > 0 {
            let (id, c, r) = rx.recv().unwrap();
            pending -= 1;
            stalled |= c.gap >= 10.0;
            completed.push(c);
            if result.is_ok() { result = r; }
            if result.is_ok() && !stalled && next < end && clock.now() < deadline && !ctx.cancelled() {
                let n = (end - next).min(block as u64) as usize;
                queues[id].send((next, n)).unwrap();
                next += n as u64;
                pending += 1;
            }
        }
        drop(queues);
    });
    completed.sort_by_key(|c| c.off);
    (completed, result)
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MapPoint {
    /// Start of the region, bytes.
    pub pos: u64,
    pub mb_s: f64,
    pub worst_ms: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct IoStats {
    pub bytes: u64,
    pub seconds: f64,
    pub mb_s: f64,
    /// Slowest single I/O.
    pub worst_ms: f64,
    pub worst_at: u64,
    pub stalls_over_1s: u64,
    pub speed_map: Vec<MapPoint>,
    #[serde(skip)]
    samples: Vec<(u64, u32, f32)>,
}

impl IoStats {
    pub fn record(&mut self, off: u64, len: usize, secs: f64) {
        self.bytes += len as u64;
        self.seconds += secs;
        let ms = secs * 1000.0;
        if ms > self.worst_ms {
            self.worst_ms = ms;
            self.worst_at = off;
        }
        if secs > 1.0 {
            self.stalls_over_1s += 1;
        }
        self.samples.push((off, len as u32, secs as f32));
    }

    /// Compute averages and a speed map of up to `buckets` regions by position.
    pub fn finish(&mut self, buckets: usize) {
        self.mb_s = if self.seconds > 0.0 { self.bytes as f64 / 1e6 / self.seconds } else { 0.0 };
        if self.samples.is_empty() {
            return;
        }
        let lo = self.samples.iter().map(|s| s.0).min().unwrap();
        let hi = self.samples.iter().map(|s| s.0 + s.1 as u64).max().unwrap();
        let width = ((hi - lo) / buckets as u64).max(1);
        let mut acc = vec![(0u64, 0f64, 0f64); buckets + 1];
        for &(off, len, secs) in &self.samples {
            let b = (((off - lo) / width) as usize).min(buckets);
            acc[b].0 += len as u64;
            acc[b].1 += secs as f64;
            acc[b].2 = acc[b].2.max(secs as f64 * 1000.0);
        }
        self.speed_map = acc
            .iter()
            .enumerate()
            .filter(|(_, a)| a.0 > 0)
            .map(|(i, a)| MapPoint { pos: lo + i as u64 * width, mb_s: a.0 as f64 / 1e6 / a.1.max(1e-9), worst_ms: a.2 })
            .collect();
    }

    /// Regions running below 1/3 of the median speed.
    pub fn slow_regions(&self) -> Vec<&MapPoint> {
        let mut v: Vec<f64> = self.speed_map.iter().map(|m| m.mb_s).collect();
        if v.len() < 5 {
            return vec![];
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let med = v[v.len() / 2];
        self.speed_map.iter().filter(|m| m.mb_s < med / 3.0).collect()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FirstError {
    pub offset: u64,
    pub expected: u64,
    pub found: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BadRange {
    pub start: u64,
    pub end: u64,
    pub kind: String,
}

/// Sector counts by class.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Tally {
    pub ok: u64,
    pub changed: u64,
    pub corrupted: u64,
    pub aliased: u64,
    pub stale: u64,
    pub unreadable: u64,
    pub first_error: Option<FirstError>,
    /// Distinct offsets whose data turned up elsewhere.
    pub alias_sources: u64,
    /// Smallest distance between a sector and the offset its data belongs to:
    /// for a wraparound fake this is the real capacity.
    pub alias_period: Option<u64>,
    pub bad_ranges: Vec<BadRange>,
    pub bad_ranges_truncated: bool,
    #[serde(skip)]
    alias_set: HashSet<u64>,
}

impl Tally {
    pub fn lost(&self) -> u64 {
        self.changed + self.corrupted + self.aliased + self.stale + self.unreadable
    }

    fn bad(&mut self, off: u64, len: u64, kind: &str) {
        if !push_range(&mut self.bad_ranges, off, len, kind) {
            self.bad_ranges_truncated = true;
        }
    }

    pub fn add(&mut self, off: u64, class: Class, first: Option<(u64, u64)>) {
        let s = SECTOR as u64;
        let kind = match class {
            Class::Ok => {
                self.ok += 1;
                return;
            }
            Class::Changed => {
                self.changed += 1;
                "changed"
            }
            Class::Corrupted => {
                self.corrupted += 1;
                "corrupted"
            }
            Class::Stale => {
                self.stale += 1;
                "stale"
            }
            Class::Aliased(src) => {
                self.aliased += 1;
                if self.alias_set.len() < 1_000_000 && self.alias_set.insert(src) {
                    self.alias_sources += 1;
                }
                let d = src.abs_diff(off);
                if d > 0 && self.alias_period.is_none_or(|p| d < p) {
                    self.alias_period = Some(d);
                }
                "aliased"
            }
        };
        if self.first_error.is_none() {
            let (e, f) = first.unwrap_or((0, 0));
            self.first_error = Some(FirstError { offset: off, expected: e, found: f });
        }
        self.bad(off, s, kind);
    }

    pub fn add_unreadable(&mut self, off: u64, len: u64) {
        self.unreadable += len / SECTOR as u64;
        if self.first_error.is_none() {
            self.first_error = Some(FirstError { offset: off, expected: 0, found: 0 });
        }
        self.bad(off, len, "unreadable");
    }

    pub fn merge(&mut self, o: &Tally) {
        self.ok += o.ok;
        self.changed += o.changed;
        self.corrupted += o.corrupted;
        self.aliased += o.aliased;
        self.stale += o.stale;
        self.unreadable += o.unreadable;
        if self.first_error.is_none() {
            self.first_error = o.first_error.clone();
        }
        self.bad_ranges_truncated |= o.bad_ranges_truncated;
        // Distinct per pass; across passes the largest is the honest lower bound.
        self.alias_sources = self.alias_sources.max(o.alias_sources);
        if let Some(p) = o.alias_period
            && self.alias_period.is_none_or(|q| p < q) {
                self.alias_period = Some(p);
            }
        for r in &o.bad_ranges {
            self.bad(r.start, r.end - r.start, &r.kind.clone());
        }
    }

    /// Classify a buffer read from stream offset `start`.
    pub fn check(&mut self, pat: Pattern, start: u64, data: &[u8]) -> bool {
        if pat.buffer_ok(start, data) {
            self.ok += (data.len() / SECTOR) as u64;
            return true;
        }
        let classes: Vec<_> = data
            .par_chunks(SECTOR)
            .enumerate()
            .map(|(i, sec)| pat.classify(start + (i * SECTOR) as u64, sec))
            .collect();
        for (i, (c, f)) in classes.into_iter().enumerate() {
            self.add(start + (i * SECTOR) as u64, c, f);
        }
        false
    }
}

/// A notable incident during a pass.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Event {
    pub time: String,
    pub offset: u64,
    pub len: u64,
    /// media | unstable | io-error | transient-io | disconnect | missing | write-error
    pub kind: String,
    pub detail: String,
}

pub fn now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

impl Event {
    pub fn new(offset: u64, len: u64, kind: &str, detail: impl Into<String>) -> Self {
        Event { time: now(), offset, len, kind: kind.into(), detail: detail.into() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Stop {
    Done,
    DiskFull,
    Cancelled,
    Disconnected,
    Error(String),
    EarlyAbort(String),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WriteResult {
    pub stats: IoStats,
    /// Stream offset the data reaches.
    pub end: u64,
    pub stop: Stop,
    pub events: Vec<Event>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct VerifyResult {
    pub stats: IoStats,
    pub tally: Tally,
    pub stop: Stop,
    pub events: Vec<Event>,
}

/// Periodic re-read of already written data during a write pass.
pub struct EarlyCheck {
    pub every: u64,
    pub samples: usize,
}

fn mb(x: u64) -> String {
    crate::report::size(x)
}

/// Write the pattern over `[start, end)` in `block`-sized chunks.
pub fn write_pass(dev: &mut dyn Dev, pat: Pattern, start: u64, end: u64, block: usize, ctx: &Ctx, early: Option<&EarlyCheck>) -> WriteResult {
    ctx.phase("Writing", end - start);
    let mut stats = IoStats::default();
    let mut events = vec![];
    let mut stop = Stop::Done;
    let mut pos = start;
    let mut next_check = early.map(|e| start + e.every);
    // What the first block held before writing: if it comes back unchanged, writes are ignored.
    let before = {
        let n = MIB.min(end - start) as usize;
        let mut b = AlignedBuf::new(n);
        match dev.read_at(start, &mut b[..n]) {
            Ok(r) if r == n => Some(hash(&b[..n])),
            _ => None,
        }
    };
    std::thread::scope(|s| {
        let (tx_full, rx_full) = sync_channel::<(u64, AlignedBuf, usize)>(2);
        let (tx_empty, rx_empty) = sync_channel::<AlignedBuf>(4);
        for _ in 0..3 {
            tx_empty.send(AlignedBuf::new(block)).unwrap();
        }
        s.spawn(move || {
            let mut off = start;
            while off < end {
                let Ok(mut b) = rx_empty.recv() else { break };
                let n = (block as u64).min(end - off) as usize;
                pat.fill(off, &mut b[..n]);
                if tx_full.send((off, b, n)).is_err() {
                    break;
                }
                off += n as u64;
            }
        });
        while let Ok((off, buf, n)) = rx_full.recv() {
            if ctx.cancelled() {
                stop = Stop::Cancelled;
                break;
            }
            let t = Instant::now();
            match dev.write_at(off, &buf[..n]) {
                Ok(()) => {
                    let secs = t.elapsed().as_secs_f64();
                    stats.record(off, n, secs);
                    pos = off + n as u64;
                    ctx.progress(pos - start);
                    ctx.sample(off, n as f64 / 1e6 / secs.max(1e-9));
                    if off == start
                        && let Some(msg) = readback(dev, pat, off, n.min(MIB as usize), before) {
                            ctx.log(&msg);
                            stop = Stop::EarlyAbort(msg);
                            break;
                        }
                }
                Err(e) if is_disk_full(&e) => {
                    // Fill the rest in 1 MiB steps, then stop.
                    let step = (MIB as usize).max(dev.align());
                    let mut o = 0;
                    while o + step <= n && dev.write_at(off + o as u64, &buf[o..o + step]).is_ok() {
                        o += step;
                    }
                    stats.record(off, o, t.elapsed().as_secs_f64());
                    pos = off + o as u64;
                    stop = Stop::DiskFull;
                    break;
                }
                Err(e) => {
                    let disc = is_disconnect(&e);
                    events.push(Event::new(off, n as u64, if disc { "disconnect" } else { "write-error" }, e.to_string()));
                    stop = if disc { Stop::Disconnected } else { Stop::Error(format!("write failed at {}: {e}", mb(off))) };
                    break;
                }
            }
            if let (Some(nc), Some(ec)) = (next_check, early)
                && pos >= nc {
                    next_check = Some(pos + ec.every);
                    if let Some(msg) = spot_check(dev, pat, start, pos, ec.samples, ctx) {
                        stop = Stop::EarlyAbort(msg);
                        break;
                    }
                }
            let _ = tx_empty.send(buf);
        }
        drop(rx_full);
        drop(tx_empty);
    });
    if let Err(e) = dev.flush()
        && matches!(stop, Stop::Done | Stop::DiskFull) {
            let disc = is_disconnect(&e);
            events.push(Event::new(pos, 0, if disc { "disconnect" } else { "write-error" }, format!("flush: {e}")));
            stop = if disc { Stop::Disconnected } else { Stop::Error(format!("flush failed: {e}")) };
        }
    stats.finish(100);
    WriteResult { stats, end: pos, stop, events }
}

/// Adds `[off, off + len)` to `ranges`, extending the last range when it joins
/// on; false when the list is full (1000 ranges) and the range was dropped.
pub fn push_range(ranges: &mut Vec<BadRange>, off: u64, len: u64, kind: &str) -> bool {
    if let Some(r) = ranges.last_mut()
        && r.end == off && r.kind == kind {
            r.end += len;
            return true;
        }
    if ranges.len() >= 1000 {
        return false;
    }
    ranges.push(BadRange { start: off, end: off + len, kind: kind.to_string() });
    true
}

/// How a read-only early abort starts; `fulltest::judge` keys on it.
pub const IGNORES_WRITES: &str = "The drive ignores writes";

/// Flush and read back the first block just written: catches drives that
/// silently ignore writes before hours are spent filling them.
fn readback(dev: &mut dyn Dev, pat: Pattern, off: u64, n: usize, before: Option<u64>) -> Option<String> {
    let mut b = AlignedBuf::new(n);
    let ok = dev.flush().is_ok() && matches!(dev.read_at(off, &mut b[..n]), Ok(r) if r == n);
    let mut t = Tally::default();
    if ok && t.check(pat, off, &b[..n]) {
        return None;
    }
    if !ok {
        return Some("The first block written could not be read back.".into());
    }
    if t.stale > 0 || before == Some(hash(&b[..n])) {
        let what = if t.stale > 0 { "data from an earlier run" } else { "exactly what was there before" };
        return Some(format!("{IGNORES_WRITES}: the first block written read back as {what}. It is read-only or broken."));
    }
    Some(format!("The first block written came back wrong ({} of {} sectors bad).", t.lost(), n / SECTOR))
}

/// Re-read `n` spread-out 4 KiB blocks of `[start, written)`. Returns an abort
/// message if data written earlier has changed.
pub fn spot_check(dev: &mut dyn Dev, pat: Pattern, start: u64, written: u64, n: usize, ctx: &Ctx) -> Option<String> {
    let blk = 4096u64.max(dev.align() as u64);
    let span = (written - start) / blk;
    if span == 0 {
        return None;
    }
    let mut buf = AlignedBuf::new(blk as usize);
    let mut t = Tally::default();
    let mut rng = crate::pattern::new_tag();
    for i in 0..n {
        // Stratified: one random block in each of n equal slices.
        rng = crate::pattern::xorshift(rng);
        let slice = span / n as u64;
        let b = if slice > 0 { i as u64 * slice + rng % slice } else { rng % span };
        let off = start + b * blk;
        match dev.read_at(off, &mut buf) {
            Ok(r) if r == blk as usize => {
                t.check(pat, off, &buf);
            }
            _ => t.add_unreadable(off, blk),
        }
    }
    if t.lost() == 0 {
        ctx.log(format!("Spot check at {}: earlier data intact", mb(written)));
        return None;
    }
    let first = t.first_error.as_ref().map(|f| f.offset).unwrap_or(0);
    let msg = match t.alias_period {
        Some(p) => format!(
            "Data written earlier was overwritten by later writes (wraparound). The drive is fake: real capacity is about {} (claimed space was being filled at {}).",
            mb(p),
            mb(written)
        ),
        None => format!(
            "Data written earlier at {} changed or was lost after writing {}. The drive is likely fake or failing; real capacity is probably below {}.",
            mb(first),
            mb(written),
            mb(written)
        ),
    };
    ctx.log(&msg);
    Some(msg)
}

fn hash(data: &[u8]) -> u64 {
    data.chunks(8).fold(0xcbf2_9ce4_8422_2325u64, |h, c| {
        let mut w = [0u8; 8];
        w[..c.len()].copy_from_slice(c);
        (h ^ u64::from_le_bytes(w)).wrapping_mul(0x100_0000_01b3)
    })
}

/// Read `[start, end)` back and classify every sector.
pub fn verify_pass(dev: &mut dyn Dev, pat: Pattern, start: u64, end: u64, block: usize, ctx: &Ctx) -> VerifyResult {
    ctx.phase_at("Verifying", start, end - start);
    let mut stats = IoStats::default();
    let mut tally = Tally::default();
    let mut events: Vec<Event> = vec![];
    let mut stop = Stop::Done;
    // (offset, len, hash of first read) of chunks with bad sectors, re-read afterwards.
    let mut suspects: Vec<(u64, usize, u64)> = vec![];

    enum Msg {
        Data(u64, AlignedBuf, usize, f64),
        /// Region that could not be read; carries the buffer back if the reader gave it up.
        Unreadable(u64, usize, Event, Option<AlignedBuf>),
        Stop(Stop, Option<Event>),
    }

    std::thread::scope(|s| {
        let (tx, rx) = sync_channel::<Msg>(2);
        let (tx_empty, rx_empty) = sync_channel::<AlignedBuf>(4);
        for _ in 0..3 {
            tx_empty.send(AlignedBuf::new(block)).unwrap();
        }
        let dev = &mut *dev;
        let ctx2 = ctx.clone();
        s.spawn(move || {
            let mut off = start;
            while off < end {
                if ctx2.cancelled() {
                    let _ = tx.send(Msg::Stop(Stop::Cancelled, None));
                    return;
                }
                let Ok(mut b) = rx_empty.recv() else { return };
                let n = (block as u64).min(end - off) as usize;
                let t = Instant::now();
                let mut res = dev.read_at(off, &mut b[..n]);
                if let Err(e) = &res {
                    if is_disconnect(e) {
                        let ev = Event::new(off, n as u64, "disconnect", e.to_string());
                        let _ = tx.send(Msg::Stop(Stop::Disconnected, Some(ev)));
                        return;
                    }
                    // Retry; a success later doesn't erase the failure.
                    let first = e.to_string();
                    let mut recovered = false;
                    for _ in 0..2 {
                        if let Ok(r) = dev.read_at(off, &mut b[..n]) {
                            res = Ok(r);
                            recovered = true;
                            break;
                        }
                    }
                    let kind = if recovered { "transient-io" } else { "io-error" };
                    let detail = if recovered { format!("read failed ({first}), retry succeeded") } else { format!("read failed 3 times: {first}") };
                    let ev = Event::new(off, n as u64, kind, detail);
                    if !recovered {
                        let _ = tx.send(Msg::Unreadable(off, n, ev, Some(b)));
                        off += n as u64;
                        continue;
                    }
                    let _ = tx.send(Msg::Unreadable(off, 0, ev, None));
                }
                let r = res.unwrap();
                let secs = t.elapsed().as_secs_f64();
                // Data first: the tally's first error must be the earliest offset.
                if tx.send(Msg::Data(off, b, r, secs)).is_err() {
                    return;
                }
                if r < n {
                    let ev = Event::new(off + r as u64, (n - r) as u64, "missing", "data missing (file absent or short)");
                    let _ = tx.send(Msg::Unreadable(off + r as u64, n - r, ev, None));
                }
                off += n as u64;
            }
        });
        while let Ok(m) = rx.recv() {
            match m {
                Msg::Data(off, buf, n, secs) => {
                    // A short read can end mid-sector: classify whole sectors only.
                    let whole = n / SECTOR * SECTOR;
                    if whole < n {
                        tally.add_unreadable(off + whole as u64, SECTOR as u64);
                    }
                    let n = whole;
                    if n > 0 {
                        stats.record(off, n, secs);
                        ctx.sample(off, n as f64 / 1e6 / secs.max(1e-9));
                        let seen = tally.bad_ranges.len();
                        if !tally.check(pat, off, &buf[..n]) {
                            for r in &tally.bad_ranges[seen.saturating_sub(1)..] {
                                ctx.mark(r.start.max(off), r.end, &r.kind);
                            }
                            if suspects.len() < 64 {
                                suspects.push((off, n, hash(&buf[..n])));
                            }
                        }
                    }
                    ctx.progress(off + n as u64 - start);
                    let _ = tx_empty.send(buf);
                }
                Msg::Unreadable(off, n, ev, buf) => {
                    if n > 0 {
                        tally.add_unreadable(off, n as u64);
                        ctx.mark(off, off + n as u64, "unreadable");
                    }
                    if let Some(b) = buf {
                        let _ = tx_empty.send(b);
                    }
                    // One event per run of missing data, not one per chunk.
                    match events.last_mut() {
                        Some(l) if l.kind == ev.kind && l.offset + l.len == ev.offset => l.len += ev.len,
                        _ => {
                            ctx.log(format!("{} at {}: {}", ev.kind, mb(ev.offset), ev.detail));
                            events.push(ev);
                        }
                    }
                }
                Msg::Stop(st, ev) => {
                    if let Some(ev) = ev {
                        ctx.log(format!("{} at {}: {}", ev.kind, mb(ev.offset), ev.detail));
                        events.push(ev);
                    }
                    stop = st;
                    break;
                }
            }
        }
        drop(rx);
        drop(tx_empty);
    });

    // Re-read chunks with bad data: same wrong bytes = media, different = unstable reads.
    if stop != Stop::Disconnected && stop != Stop::Cancelled {
        'suspects: for (off, n, h0) in suspects {
            if ctx.cancelled() {
                stop = Stop::Cancelled;
                break;
            }
            let mut b = AlignedBuf::new(n);
            let mut hashes = vec![];
            let mut err = None;
            for _ in 0..2 {
                if ctx.cancelled() {
                    stop = Stop::Cancelled;
                    break 'suspects;
                }
                match dev.read_at(off, &mut b[..n]) {
                    Ok(got) if got == n => hashes.push(hash(&b[..n])),
                    Ok(got) => err = Some(format!("short read: {got} of {n} bytes")),
                    Err(e) => err = Some(e.to_string()),
                }
            }
            let (kind, detail) = if let Some(e) = err {
                ("io-error", format!("re-read of a bad chunk failed: {e}"))
            } else if hashes.iter().all(|&h| h == h0) {
                ("media", "same wrong data on every re-read (stored data is bad)".to_string())
            } else {
                ("unstable", "data changes between re-reads (unstable reads: link, reader or failing flash)".to_string())
            };
            events.push(Event::new(off, n as u64, kind, detail));
        }
    }
    stats.finish(100);
    VerifyResult { stats, tally, stop, events }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::{FileSet, MemDev, SimMode};
    use std::io::{Seek, SeekFrom, Write};

    const BLK: usize = 1 << 20;

    #[test]
    fn wraparound_aborts_early_and_aliases() {
        let ctx = Ctx::new();
        let pat = Pattern::Seeded { tag: 7 };
        let mut d = MemDev::new(64 * MIB, 512 * MIB, SimMode::Wrap);
        let w = write_pass(&mut d, pat, 0, 512 * MIB, BLK, &ctx, Some(&EarlyCheck { every: 32 * MIB, samples: 64 }));
        assert!(matches!(w.stop, Stop::EarlyAbort(ref m) if m.contains("wraparound")), "{:?}", w.stop);
        assert!(w.end < 200 * MIB);
        let v = verify_pass(&mut d, pat, 0, w.end, BLK, &ctx);
        assert!(v.tally.aliased > 0);
        assert_eq!(v.tally.alias_period, Some(64 * MIB));
    }

    #[test]
    fn dropped_writes_are_lost_tail() {
        let ctx = Ctx::new();
        let pat = Pattern::Seeded { tag: 9 };
        let mut d = MemDev::new(64 * MIB, 128 * MIB, SimMode::Drop);
        let w = write_pass(&mut d, pat, 0, 128 * MIB, BLK, &ctx, None);
        assert_eq!(w.stop, Stop::Done);
        let v = verify_pass(&mut d, pat, 0, 128 * MIB, BLK, &ctx);
        assert_eq!(v.tally.ok, 64 * MIB / 512);
        assert_eq!(v.tally.corrupted, 64 * MIB / 512);
        assert_eq!(v.tally.first_error.unwrap().offset, 64 * MIB);
    }

    #[test]
    fn read_only_is_caught_on_first_block() {
        let ctx = Ctx::new();
        let mut d = MemDev::new(64 * MIB, 64 * MIB, SimMode::ReadOnly);
        // pretend an earlier run left its data there
        Pattern::Seeded { tag: 1 }.fill(0, &mut d.data[..BLK]);
        let w = write_pass(&mut d, Pattern::Seeded { tag: 2 }, 0, 64 * MIB, BLK, &ctx, None);
        assert!(matches!(w.stop, Stop::EarlyAbort(ref m) if m.contains("ignores writes") && m.contains("earlier run")), "{:?}", w.stop);
    }

    #[test]
    fn file_set_roundtrip_and_corruption() {
        let dir = std::env::temp_dir().join(format!("ft-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = Ctx::new();
        let pat = Pattern::Seeded { tag: 11 };
        let len = 24 * MIB;
        let mut fs = FileSet::new(&dir, len);
        let w = write_pass(&mut fs, pat, 0, len, 4 * BLK, &ctx, Some(&EarlyCheck { every: 8 * MIB, samples: 16 }));
        assert_eq!(w.stop, Stop::Done);
        fs.close().unwrap();
        let v = verify_pass(&mut fs, pat, 0, len, 4 * BLK, &ctx);
        assert_eq!(v.tally.lost(), 0);
        assert_eq!(v.tally.ok, len / 512);
        fs.close().unwrap();
        // flip one bit in the middle of the file
        let mut f = std::fs::OpenOptions::new().read(true).write(true).open(fs.path(0)).unwrap();
        f.seek(SeekFrom::Start(5 * MIB + 100)).unwrap();
        f.write_all(&[0x5a ^ 0xff]).unwrap();
        drop(f);
        let v = verify_pass(&mut fs, pat, 0, len, 4 * BLK, &ctx);
        assert_eq!(v.tally.lost(), 1);
        assert_eq!(v.tally.first_error.as_ref().unwrap().offset, 5 * MIB);
        assert!(v.events.iter().any(|e| e.kind == "media"));
        fs.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod speed {
    use super::*;
    use crate::dev::{MemDev, SimMode};

    /// Engine ceiling without a disk: `cargo test --release engine_ceiling -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn engine_ceiling() {
        let ctx = Ctx::new();
        let pat = Pattern::Seeded { tag: 3 };
        let len = 2u64 << 30;
        let mut d = MemDev::new(len, len, SimMode::Genuine);
        let t = Instant::now();
        let w = write_pass(&mut d, pat, 0, len, 8 << 20, &ctx, None);
        println!("write pass wall clock {:.0} MB/s", len as f64 / 1e6 / t.elapsed().as_secs_f64());
        let t = Instant::now();
        let v = verify_pass(&mut d, pat, 0, len, 8 << 20, &ctx);
        let vs = t.elapsed().as_secs_f64();
        println!("write pipeline {:.0} MB/s (memcpy-bound), verify pipeline {:.0} MB/s", w.stats.mb_s, len as f64 / 1e6 / vs);
        assert_eq!(v.tally.lost(), 0);
    }
}
