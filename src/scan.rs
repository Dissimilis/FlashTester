//! Read-only surface scan: read every sector, time it, and map bad and slow regions.

use crate::ctx::Ctx;
use crate::dev::{AlignedBuf, Dev, is_disconnect};
use crate::engine::{BadRange, Event, IoStats, MapPoint, Stop, push_range};
use crate::report::size;
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ScanResult {
    pub stats: IoStats,
    pub size: u64,
    /// End of the fully scanned region, including unreadable areas.
    #[serde(default)]
    pub scanned: u64,
    /// Bytes in blocks that could not be read.
    pub bad_bytes: u64,
    pub unit: u64,
    pub bad_ranges: Vec<BadRange>,
    pub slow: Vec<MapPoint>,
    pub stop: Stop,
    pub events: Vec<Event>,
}

impl ScanResult {
    pub fn summary_lines(&self) -> Vec<String> {
        let mut v = vec![format!(
            "Surface scan: read {} of {} at {:.1} MB/s; slowest read {:.0} ms at {}; {} reads over 1 s.",
            size(self.stats.bytes),
            size(self.size),
            self.stats.mb_s,
            self.stats.worst_ms,
            size(self.stats.worst_at),
            self.stats.stalls_over_1s
        )];
        if self.bad_bytes == 0 {
            v.push("No unreadable sectors.".into());
        } else {
            v.push(format!("{} unreadable in {} region(s):", size(self.bad_bytes), self.bad_ranges.len()));
            for r in self.bad_ranges.iter().take(10) {
                v.push(format!("  {} - {} (0x{:x})", size(r.start), size(r.end), r.start));
            }
        }
        for s in self.slow.iter().take(10) {
            v.push(format!("Slow region at {}: {:.1} MB/s (slowest read {:.0} ms)", size(s.pos), s.mb_s, s.worst_ms));
        }
        if self.stop == Stop::Disconnected {
            v.push("The drive disconnected during the scan.".into());
        }
        if self.events.iter().any(|e| e.kind == "io-error") {
            v.push("Some reads failed and were retried in smaller blocks. The original errors remain part of this result.".into());
        }
        v
    }
}

pub fn run_scan(dev: &mut dyn Dev, limit: Option<u64>, block: usize, ctx: &Ctx) -> ScanResult {
    let len = limit.unwrap_or(u64::MAX).min(dev.len()) / 4096 * 4096;
    let unit = dev.align() as u64;
    let mut r = ScanResult { stats: IoStats::default(), size: len, scanned: 0, bad_bytes: 0, unit, bad_ranges: vec![], slow: vec![], stop: Stop::Done, events: vec![] };
    if len == 0 {
        r.stop = Stop::Error("The scan size is smaller than one 4096-byte block.".into());
        return r;
    }
    let mut buf = AlignedBuf::new(block);
    let mut small = AlignedBuf::new(unit as usize);
    let mut off = 0u64;
    ctx.phase("Scanning (read-only)", len);
    let bad = |r: &mut ScanResult, o: u64, n: u64| {
        r.bad_bytes += n;
        ctx.mark(o, o + n, "unreadable");
        push_range(&mut r.bad_ranges, o, n, "unreadable");
    };
    while off < len {
        if ctx.cancelled() {
            r.stop = Stop::Cancelled;
            break;
        }
        let n = (block as u64).min(len - off) as usize;
        let t = Instant::now();
        match dev.read_at(off, &mut buf[..n]) {
            Ok(got) => {
                let secs = t.elapsed().as_secs_f64();
                if got < n {
                    bad(&mut r, off + got as u64, (n - got) as u64);
                }
                r.stats.record(off, got, secs);
                ctx.sample(off, got as f64 / 1e6 / secs.max(1e-9));
            }
            Err(e) if is_disconnect(&e) => {
                r.stats.record(off, 0, t.elapsed().as_secs_f64());
                r.events.push(Event::new(off, n as u64, "disconnect", e.to_string()));
                r.stop = Stop::Disconnected;
                break;
            }
            Err(e) => {
                // Find the bad blocks inside this chunk.
                r.stats.record(off, 0, t.elapsed().as_secs_f64());
                r.events.push(Event::new(off, n as u64, "io-error", e.to_string()));
                ctx.log(format!("Read error at {}: {e}; locating bad blocks", size(off)));
                let mut o = off;
                while o < off + n as u64 {
                    if ctx.cancelled() {
                        r.stop = Stop::Cancelled;
                        break;
                    }
                    let count = unit.min(off + n as u64 - o) as usize;
                    let t = Instant::now();
                    match dev.read_at(o, &mut small[..count]) {
                        Ok(got) => {
                            let secs = t.elapsed().as_secs_f64();
                            r.stats.record(o, got, secs);
                            ctx.sample(o, got as f64 / 1e6 / secs.max(1e-9));
                            if got < count {
                                bad(&mut r, o + got as u64, (count - got) as u64);
                            }
                        }
                        Err(e) if is_disconnect(&e) => {
                            r.stats.record(o, 0, t.elapsed().as_secs_f64());
                            r.events.push(Event::new(o, count as u64, "disconnect", e.to_string()));
                            r.stop = Stop::Disconnected;
                            break;
                        }
                        Err(_) => {
                            r.stats.record(o, 0, t.elapsed().as_secs_f64());
                            bad(&mut r, o, count as u64);
                        }
                    }
                    o += count as u64;
                    r.scanned = o;
                    ctx.progress(o);
                }
                if matches!(r.stop, Stop::Disconnected | Stop::Cancelled) {
                    break;
                }
            }
        }
        off += n as u64;
        r.scanned = off;
        ctx.progress(off);
    }
    r.stats.finish(200);
    r.slow = r.stats.slow_regions().into_iter().cloned().collect();
    r
}
