//! Bounded whole-disk diagnostics. Sampling can find a fault, never prove every byte.
use crate::ctx::Ctx;
use crate::dev::{AlignedBuf, Dev, MIB};
use crate::engine::{self, Clock, Tally};
use crate::pattern::{Pattern, new_tag};
use crate::platform::DiskInfo;
use crate::probe::{self, Finding, ProbeResult};
use crate::report::Status;
use crate::smart::{Counter, Smart, SmartRun, Temp};
use serde::{Deserialize, Serialize};
use std::io;

const GB: u64 = 1_000_000_000;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DriveClass { #[default] Usb, Usb2, Sata, Nvme }

impl DriveClass {
    fn budgets(self) -> [f64; 7] {
        match self {
            Self::Usb => [15., 45., 60., 210., 75., 120., 75.],
            Self::Usb2 => [10., 10., 25., 30., 45., 0., 60.],
            Self::Sata => [15., 45., 20., 210., 120., 120., 70.],
            Self::Nvme => [15., 45., 20., 450., 150., 150., 70.],
        }
    }
    fn ssd(self) -> bool { matches!(self, Self::Sata | Self::Nvme) }
    fn qd(self) -> usize { if self.ssd() { 4 } else { 1 } }
    fn post_cap(self) -> u64 { match self { Self::Nvme => 32 * GB, Self::Sata => 8 * GB, _ => GB } }
}

pub fn classify(d: &DiskInfo, smart: Option<&Smart>) -> DriveClass {
    if d.usb.as_ref().is_some_and(|u| u.slow_link) { return DriveClass::Usb2; }
    if d.bus == "USB" {
        return if d.ssd_hint == Some(true) || smart.is_some_and(|s| s.via.contains("NVMe") || s.via.contains("SAT")) { DriveClass::Sata } else { DriveClass::Usb };
    }
    match d.bus.as_str() { "SD" | "MMC" => DriveClass::Usb, "NVMe" => DriveClass::Nvme, _ => DriveClass::Sata }
}

pub fn failing(s: &Smart) -> bool {
    !s.alarms.is_empty() || [Counter::Pending, Counter::OfflineUncorrectable].iter().any(|&c| s.get(c).unwrap_or(0) > 0)
}

#[derive(Clone)]
pub struct Limits {
    pub phases: [f64; 7],
    pub total: f64,
    pub seq_cap: u64,
    pub write_cap: u64,
    pub block: usize,
    pub check_every: u64,
    pub mark_every: u64,
}

impl Limits {
    pub fn new(class: DriveClass, size: u64, align: u64, max_time: Option<f64>) -> Self {
        let mut phases = class.budgets();
        let default = if class == DriveClass::Usb2 { 180. } else if class == DriveClass::Nvme { 900. } else { 600. };
        let total = max_time.unwrap_or(default);
        // Extra time belongs to sequential writes. Final verification is reserved first.
        phases[3] += (total - default).max(0.0);
        Self { phases, total, seq_cap: (size / 5 * 2) / align * align, write_cap: (size / 2) / align * align,
            block: (4 * MIB) as usize, check_every: GB / align * align, mark_every: 256 * MIB }
    }
    fn end(&self, phase: usize, now: f64) -> f64 {
        let reserve = if phase == 6 { 0. } else { self.phases[6].min(self.total) };
        (now + self.phases[phase]).min((self.total - reserve).max(0.0))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Stall {
    pub phase: String,
    pub offset: Option<u64>,
    pub at: f64,
    pub seconds: f64,
    pub write: bool,
    pub write_path: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Phase {
    pub name: String,
    pub seconds: f64,
    pub bytes: u64,
    pub median_ms: f64,
    pub p99_ms: f64,
    pub worst_ms: f64,
    pub operations: u64,
    #[serde(skip)]
    latency: Vec<f64>,
}

impl Phase {
    fn finish(&mut self) {
        self.latency.sort_by(f64::total_cmp);
        let n = self.latency.len();
        if n > 0 {
            self.median_ms = self.latency[n / 2] * 1000.;
            self.p99_ms = self.latency[(n * 99 / 100).min(n - 1)] * 1000.;
            self.worst_ms = self.worst_ms.max(self.latency[n - 1] * 1000.);
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Recovery {
    pub idle_seconds: f64,
    pub fast_bytes: u64,
    pub mb_s: f64,
    pub lower_bound: bool,
    pub full: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Zone {
    pub name: String,
    pub start: u64,
    pub end: u64,
    pub verified: u64,
    pub read_4k_ms: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Sample { pub seconds: f64, pub offset: u64, pub bytes: u64, pub mb_s: f64 }

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Diagnostics {
    pub class: DriveClass,
    pub status: Status,
    pub reason: String,
    pub seconds: f64,
    pub budget_seconds: f64,
    pub trim: String,
    pub written: u64,
    pub distinct_upper_bound: u64,
    /// Sustained speed before a drop, otherwise sample-median burst speed. Legacy JSON name.
    pub peak_mb_s: f64,
    #[serde(default)]
    pub burst_mb_s: Option<f64>,
    #[serde(default)]
    pub sustained_mb_s: Option<f64>,
    #[serde(default)]
    pub write_pattern: String,
    #[serde(default)]
    pub cache_bytes: Option<u64>,
    pub drop_at: Option<u64>,
    pub drop_confirmed_at: Option<u64>,
    pub post_mb_s: Option<f64>,
    pub baseline_read_mb_s: Option<f64>,
    pub drop_read_mb_s: Option<f64>,
    pub heat_suspected: bool,
    pub link_limited: bool,
    pub samples: Vec<Sample>,
    pub phases: Vec<Phase>,
    pub stalls: Vec<Stall>,
    pub warnings: Vec<String>,
    pub tally: Tally,
    pub probe: Option<ProbeResult>,
    pub read_mb_s: Option<f64>,
    pub mixed: Option<Phase>,
    pub mixed_verified: bool,
    pub iops: crate::perf::IopsResult,
    pub idle_read_ms: Option<f64>,
    pub loaded_reads: Option<Phase>,
    pub reads_freeze: bool,
    pub recovery: Vec<Recovery>,
    pub thermal_recovery: bool,
    pub zones: Vec<Zone>,
    pub final_complete: bool,
    pub stall_interpretation: String,
    pub smart: Option<SmartRun>,
    pub identity_changes: Vec<String>,
    pub triage_failing: bool,

}

pub fn check_time(seconds: f64) -> Result<(), String> {
    let valid = std::time::Duration::try_from_secs_f64(seconds).ok()
        .filter(|d| !d.is_zero()).and_then(|d| std::time::Instant::now().checked_add(d));
    if valid.is_none() { return Err("Diagnostic duration must be finite, positive and within the clock's range.".into()); }
    Ok(())
}

pub fn report(disk: &DiskInfo, d: Diagnostics) -> crate::report::Report {
    let mut r = crate::report::Report::new("diagnose", crate::fulltest::Where::Disk(disk.clone()).target());
    r.set(d.status, d.reason.clone());
    r.seconds = d.seconds;
    r.finished = engine::now();
    r.warnings.extend(d.warnings.clone());
    r.probe = d.probe.clone();
    r.smart = d.smart.clone();
    r.identity_changes = d.identity_changes.clone();
    if !d.iops.rows.is_empty() { r.iops = Some(d.iops.clone()); }
    r.tested = d.zones.iter().map(|z| z.verified).sum();
    r.notes.push("Quick diagnostics samples the disk. A full write-and-verify test is needed to check every byte. Discard may erase the whole disk even though subsequent writes cover at most half its capacity.".into());
    r.notes.push("Latency percentiles use bounded samples; worst latency and all stalls are retained. Zone read latency is a single 4K sample, not proof of weak flash.".into());
    r.notes.push("Data I/O pauses of 10 s or more are warnings; 60 s without a completion stops the run. Slow successful discard requests do not indicate failure. Limits are checked between calls, so a blocked call can overrun a deadline.".into());
    r.diagnostics = Some(d);
    r
}

impl Diagnostics {
    fn check_drop(&mut self) {
        if self.drop_at.is_some() && self.post_mb_s.is_some_and(|s| s >= self.peak_mb_s * 0.9) {
            self.drop_at = None;
            self.drop_confirmed_at = None;
            self.cache_bytes = None;
            self.post_mb_s = None;
            self.peak_mb_s = self.burst_mb_s.or(self.sustained_mb_s).unwrap_or(self.peak_mb_s);
        }
    }
    fn cache_caveat(&self) -> bool { !self.trim.starts_with("effective") && (self.drop_at.is_some() || !self.recovery.is_empty()) }
    pub fn summary_lines(&self) -> Vec<String> {
        use crate::report::{size, duration};
        let mut lines = vec![];
        let speed = if self.peak_mb_s > 0. { format!("{:.1} MB/s before the drop", self.peak_mb_s) }
            else { "burst speed not measured".into() };
        if self.written == 0 { lines.push("Sequential write: not measured.".into()); }
        else if let Some(pos) = self.drop_at {
            let after = self.post_mb_s.map(|v| format!("{v:.1} MB/s")).unwrap_or_else(|| "not measured".into());
            lines.push(format!("Sequential write: {speed}; slowdown near {}, then {after}.", size(pos)));
            lines.push(if self.heat_suspected { "Heat, power or controller/garbage-collection slowdown is suspected; this is not a cache-size measurement.".into() }
                else if self.baseline_read_mb_s.is_some() { format!("Estimated fast stretch: {}. Reads stayed fast: the write cache is a likely cause, not proven.", size(self.cache_bytes.unwrap_or(pos))) }
                else { "No pre-drop read baseline was available; the cause of the write slowdown is unconfirmed.".into() });
        } else {
            lines.push(format!("Sequential write: no drop in the first {}.", size(self.written)));
            if let Some(sustained) = self.sustained_mb_s {
                lines.push(if self.write_pattern == "bursts with regular pauses" {
                    format!("Writes in bursts at {:.1} MB/s with regular pauses; sustained {sustained:.1} MB/s.", self.burst_mb_s.unwrap_or(self.peak_mb_s))
                } else { format!("Sequential writes sustained {sustained:.1} MB/s.") });
            } else if self.peak_mb_s > 0. { lines.push(format!("Write speed: {:.1} MB/s.", self.peak_mb_s)); }
            if self.write_pattern == "bursts with regular pauses" && !self.trim.starts_with("effective") {
                lines.push("No distinct write-cache stretch was seen; the write cache may not have recovered since earlier writes.".into());
            }
        }
        if self.cache_caveat() { lines.push("All cache figures may be understated: the drive was not trimmed.".into()); }
        if self.drop_at.is_none() { lines.push("Cache recovery: not measured because no drop was seen.".into()); }
        else if self.heat_suspected { lines.push(if self.thermal_recovery { "Thermal recovery: read speed returned after idle; heat is a hypothesis, not proof." } else { "Thermal recovery: read speed did not recover within the available budget; no recovery writes were made." }.into()); }
        else if self.recovery.is_empty() { lines.push("Cache recovery: insufficient time or unused space within the write cap.".into()); }
        for r in &self.recovery {
            lines.push(if r.full { format!("Fast writes fully recover within {:.0} s idle ({} at {:.1} MB/s).", r.idle_seconds, size(r.fast_bytes), r.mb_s) }
                else { format!("After {:.0} s idle, {}{} was fast again at {:.1} MB/s{}.", r.idle_seconds, if r.lower_bound { "at least " } else { "" }, size(r.fast_bytes), r.mb_s, if r.lower_bound { " (attempt capped)" } else { "" }) });
        }
        lines.push(format!("Sequential read: {}.", self.read_mb_s.map(|v| format!("{v:.1} MB/s, verified")).unwrap_or_else(|| "not measured".into())));
        if let Some(m) = &self.mixed {
            lines.push(format!("Mixed small writes: {:.1} MB/s; {}. Cache state: {}.", m.bytes as f64 / 1e6 / m.seconds.max(1e-9), if self.mixed_verified { "read back and verified" } else { "read-back incomplete" }, if self.drop_at.is_some() && !self.heat_suspected && self.baseline_read_mb_s.is_some() { "likely full" } else { "not confirmed full" }));
            if m.p99_ms > m.median_ms.max(0.001) * 50. {
                lines.push(format!("The worst small write took {:.2} s: copying a folder of small files may feel like it hangs (median {:.2} ms, p99 {:.2} ms).", m.worst_ms / 1000., m.median_ms, m.p99_ms));
            } else if m.operations > 0 { lines.push(format!("Mixed small writes {} (median {:.2} ms, p99 {:.2} ms).", if m.p99_ms < 100. { "were snappy" } else { "were consistently slow" }, m.median_ms, m.p99_ms)); }
        } else { lines.push("Mixed small writes: not measured.".into()); }
        for kind in ["read", "write"] {
            if let Some(row) = self.iops.rows.iter().filter(|r| r.kind == kind).max_by(|a,b| a.iops.total_cmp(&b.iops)) {
                lines.push(format!("Random 4K {kind}: {:.0} IOPS ({:.1} MB/s), best measured QD{}.", row.iops, row.mb_s, row.qd));
            }
        }
        lines.push(match &self.loaded_reads {
            Some(r) => format!("Reads during writes: {} (idle median {:.2} ms; loaded p99 {:.2} ms, worst {:.2} ms).", if self.reads_freeze { "bulk storage only: it freezes reads while writing" } else { "fine as a system/app drive in this sample" }, self.idle_read_ms.unwrap_or(0.), r.p99_ms, r.worst_ms),
            None => "Reads during writes: not measured.".into(),
        });
        lines.push(format!("TRIM: {}. Run time: {} of {} allowed.", if self.trim.is_empty() { "not attempted" } else { &self.trim }, duration(self.seconds), duration(self.budget_seconds)));
        lines
    }
}

/// Sample counts describe burst speeds; bytes / active time includes controller pauses.
#[derive(Default)]
struct DropWatch {
    samples: Vec<Sample>, reference: f64, peak: f64, burst: f64, sustained: f64, sawtooth: bool,
    // Before the onset [start, end), then the start of the sustained measurement.
    confirmed: Option<(usize, usize, usize)>,
}
fn median(mut values: Vec<f64>) -> f64 {
    if values.is_empty() { return 0.; }
    values.sort_by(f64::total_cmp);
    (values[(values.len() - 1) / 2] + values[values.len() / 2]) / 2.
}
impl DropWatch {
    fn time(&self, i: usize) -> f64 { if i == 0 { 0. } else { self.samples[i - 1].seconds } }
    fn bytes(&self, a: usize, b: usize) -> u64 { self.samples[a..b].iter().map(|s| s.bytes).sum() }
    fn speed(&self, a: usize, b: usize) -> f64 { self.bytes(a, b) as f64 / 1e6 / (self.time(b) - self.time(a)).max(1e-9) }
    fn window_speed(&self, start: usize, end: usize) -> Option<f64> {
        let since = self.time(end) - 5.;
        if since < self.time(start) { return None; }
        let mut bytes = 0.;
        for i in (start..end).rev() {
            let to = self.time(i + 1);
            if to <= since { break; }
            let duration = to - self.time(i);
            bytes += self.samples[i].bytes as f64 * (to - self.time(i).max(since)) / duration.max(1e-9);
        }
        Some(bytes / 5e6)
    }
    fn pauses(&self, start: usize, burst: f64) -> Vec<(usize, usize)> {
        let fast_time = median((start..self.samples.len()).filter(|&i| self.samples[i].mb_s >= burst * 0.85)
            .map(|i| self.time(i + 1) - self.time(i)).collect());
        let mut pauses = vec![];
        let mut i = start;
        while i < self.samples.len() {
            if self.samples[i].mb_s >= burst * 0.5 { i += 1; continue; }
            let a = i;
            while i < self.samples.len() && self.samples[i].mb_s < burst * 0.5 { i += 1; }
            // A lower plateau has many ordinary-duration samples. A pause must
            // contain a slow completion, including adjacent transition samples.
            if (a..i).any(|j| self.time(j + 1) - self.time(j) >= fast_time * 3.) { pauses.push((a, i)); }
        }
        pauses
    }
    fn after(&self) -> Option<f64> { self.confirmed.map(|(_, _, post)| self.speed(post, self.samples.len())) }
    fn cache_bytes(&self) -> Option<u64> { self.confirmed.map(|(a, b, _)| self.bytes(a, b)) }
    fn update(&self, r: &mut Diagnostics) {
        r.peak_mb_s = self.peak;
        r.burst_mb_s = Some(self.burst);
        r.sustained_mb_s = Some(self.sustained);
        r.cache_bytes = self.cache_bytes();
        r.write_pattern = if self.sawtooth { "bursts with regular pauses" } else { "continuous writes" }.into();
    }
    fn push(&mut self, s: Sample) -> Option<u64> {
        self.samples.push(s);
        let n = self.samples.len();
        // Five consecutive samples resist isolated spikes without weighting a
        // long wake-up or controller pause more heavily than a normal completion.
        if n >= 5 && (self.time(n) <= 10. || self.reference == 0.) {
            self.reference = self.reference.max(median(self.samples[n - 5..].iter().map(|s| s.mb_s).collect()));
        }
        let reference = if self.reference > 0. { self.reference } else { median(self.samples.iter().map(|s| s.mb_s).collect()) };
        let start = self.samples.iter().position(|s| s.mb_s >= reference * 0.5).unwrap_or(0);
        self.burst = median(self.samples[start..].iter().map(|s| s.mb_s).collect());
        self.sustained = self.speed(start, n);
        self.sawtooth = self.pauses(start, self.burst).len() >= 2;
        if let Some((a, b, _)) = self.confirmed { self.peak = self.speed(a, b); return Some(self.samples[b].offset); }
        self.peak = self.burst;
        if n < 5 { return None; }
        let pauses = self.pauses(start, reference);
        let mut end = pauses.first().map_or(n, |p| p.0);
        let mut i = start;
        while i < end {
            if self.samples[i].mb_s >= reference * 0.85 { i += 1; continue; }
            let a = i;
            while i < n && self.samples[i].mb_s < reference * 0.85 { i += 1; }
            // Short boundary writes and isolated dips must not move a later cliff
            // to an earlier address. A lower level needs two seconds of evidence.
            if self.time(i) - self.time(a) >= 2. { end = a; break; }
        }
        let first = self.time(end) - self.time(start);
        if end == n || first < 1. { return None; }
        let before = self.speed(start, end);
        if pauses.len() >= 3 && pauses[0].0 == end {
            let typical = median(pauses.windows(2).map(|p| self.time(p[1].0) - self.time(p[0].1)).collect());
            let last = pauses.last().unwrap().1;
            let post = pauses[0].1; // Complete later cycles, without the initial transition pause.
            if first > 2. * typical && self.time(last) - self.time(end) >= 5. && self.speed(post, last) <= before * 0.75 {
                self.confirmed = Some((start, end, post));
            }
        }
        // A lasting lower level also counts. Require several samples so one
        // long pause cannot masquerade as a lower plateau; bursts reset the hold.
        let low = (end..n).rfind(|&i| self.samples[i].mb_s > before * 0.85).map_or(end, |i| i + 1);
        if self.confirmed.is_none() && n - low >= 5 {
            for (fraction, hold) in [(0.75, 10.), (0.85, 20.)] {
                let mut since = None;
                for j in low + 1..=n {
                    if self.window_speed(start, j).is_some_and(|s| s <= before * fraction) {
                        let t = *since.get_or_insert(self.time(j));
                        // Use the low region that survived the hold, not an earlier
                        // dip. Only the sawtooth rule uses the first stretch boundary.
                        if self.time(j) - t >= hold { self.confirmed = Some((start, low, low)); }
                    } else { since = None; }
                }
            }
        }
        if let Some((a, b, _)) = self.confirmed { self.peak = self.speed(a, b); Some(self.samples[b].offset) } else { None }
    }
}

use crate::probe::Budget;

struct Run<'a> {
    clock: Clock,
    ctx: &'a Ctx,
    limits: Limits,
    r: Diagnostics,
    pat: Pattern,
    end: f64,
    phase_start: f64,
    seq_end: u64,
    overhead: u64,
    mixed: (u64, u64),
    recovery: (u64, u64),
    before: Option<Smart>,
    observe: &'a mut (dyn FnMut() -> io::Result<Option<Smart>> + Send),
    replayed: bool,
    reads: std::collections::VecDeque<(f64, Pattern, u64, usize)>,
}

impl Run<'_> {
    fn phase(&mut self, n: usize, name: &str) {
        self.finish_phase();
        self.replayed = false;
        self.reads.clear();
        self.phase_start = self.clock.now();
        self.end = self.limits.end(n, self.phase_start);
        self.r.phases.push(Phase { name: name.into(), ..Default::default() });
        self.ctx.phase(name, 0);
    }
    fn finish_phase(&mut self) {
        if let Some(p) = self.r.phases.last_mut() { p.seconds = self.clock.now() - self.phase_start; p.finish(); }
    }
    fn ready(&self) -> io::Result<()> {
        if self.ctx.cancelled() { return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")); }
        if self.clock.now() >= self.end { return Err(io::Error::new(io::ErrorKind::TimedOut, Budget)); }
        Ok(())
    }
    fn record(&mut self, off: u64, n: usize, start: f64, write: bool) -> io::Result<()> {
        let elapsed = self.clock.now() - start;
        self.record_at(off, n, elapsed, self.clock.now(), write);
        if self.clock.complete(start, self.clock.now()) >= 60.0 { return Err(io::Error::other(format!("No I/O completed for {elapsed:.1} s; a blocked call overran the limit."))); }
        Ok(())
    }
    fn record_at(&mut self, off: u64, n: usize, elapsed: f64, at: f64, write: bool) {
        let p = self.r.phases.last_mut().unwrap();
        p.bytes += n as u64;
        p.operations += 1;
        p.worst_ms = p.worst_ms.max(elapsed * 1000.);
        if p.latency.len() < 65536 { p.latency.push(elapsed); }
        else { let slot = crate::pattern::xorshift(p.operations) % p.operations; if slot < p.latency.len() as u64 { p.latency[slot as usize] = elapsed; } }
        if elapsed >= 2.0 { self.r.stalls.push(Stall { phase: p.name.clone(), offset: (n > 0).then_some(off), at, seconds: elapsed, write, write_path: false }); }
    }
    fn read(&mut self, dev: &mut dyn Dev, off: u64, buf: &mut [u8]) -> io::Result<()> {
        self.ready()?;
        let t = self.clock.now();
        let result = dev.read_at(off, buf);
        self.record(off, buf.len(), t, false)?;
        if result? != buf.len() { return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read")); }
        Ok(())
    }
    fn write(&mut self, dev: &mut dyn Dev, off: u64, buf: &[u8]) -> io::Result<()> {
        self.ready()?;
        let t = self.clock.now();
        let result = dev.write_at(off, buf);
        self.record(off, buf.len(), t, true)?;
        result
    }
    fn verify(&mut self, dev: &mut dyn Dev, pat: Pattern, off: u64, n: usize, wrap: bool) -> io::Result<()> {
        let mut buf = AlignedBuf::new(n);
        let started = self.clock.now();
        self.read(dev, off, &mut buf[..n])?;
        let elapsed = self.clock.now() - started;
        let mut t = Tally::default();
        if !t.check(pat, off, &buf[..n]) {
            self.r.tally.merge(&t);
            // Retry once, but keep the original mismatch as a finding even if it clears.
            self.read(dev, off, &mut buf[..n])?;
            self.r.status = if pat.buffer_ok(off, &buf[..n]) { Status::Unreliable } else if wrap || t.aliased > 0 { Status::Fake } else { Status::Defective };
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Written data changed at byte {off}.")));
        }
        self.r.tally.merge(&t);
        self.replay_reads(dev, pat, off, n, started, elapsed)
    }
    fn replay_reads(&mut self, dev: &mut dyn Dev, pat: Pattern, off: u64, n: usize, started: f64, elapsed: f64) -> io::Result<()> {
        if !self.replayed {
            self.reads.push_back((started, pat, off, n));
            while self.reads.len() > 1 && self.reads[1].0 < self.clock.now() - 10. { self.reads.pop_front(); }
            if elapsed >= 2. {
                self.replayed = true;
                self.r.warnings.push("Repeating up to the last 10 s of verification reads once after a stall.".into());
                let until = (self.clock.now() + 10.).min(self.end);
                for (_, pattern, pos, len) in self.reads.clone() {
                    if self.clock.now() >= until { break; }
                    self.verify(dev, pattern, pos, len, false)?;
                }
            }
        }
        Ok(())
    }
    fn idle(&mut self, secs: f64) -> io::Result<()> {
        let end = (self.clock.now() + secs).min(self.end);
        while self.clock.now() < end { self.ready()?; self.clock.sleep((end - self.clock.now()).min(0.1)); }
        Ok(())
    }
    fn discard(&mut self, dev: &mut dyn Dev, off: u64, len: u64) -> io::Result<()> {
        self.ready()?;
        let t = self.clock.now();
        let result = dev.discard(off, len);
        self.record_at(0, 0, self.clock.now() - t, self.clock.now(), true);
        // A slow successful discard is not evidence of failing media.
        self.clock.complete(t, self.clock.now());
        result
    }
    fn trim(&mut self, dev: &mut dyn Dev) -> io::Result<()> {
        self.phase(1, "TRIM and idle");
        let align = dev.sector_size() as u64;
        let size = dev.len() / align * align;
        let chunk = dev.discard_max_bytes().unwrap_or(8 * crate::dev::GIB).min(8 * crate::dev::GIB) / align * align;
        if chunk == 0 { self.r.trim = "unsupported: OS discard limit is zero".into(); return Ok(()); }
        self.ctx.phase("TRIM", size);
        let mut off = 0;
        self.r.trim = format!("partial: 0 of {size} bytes trimmed");
        while off < size && self.clock.now() < self.end {
            let n = chunk.min(size - off);
            if let Err(e) = self.discard(dev, off, n) {
                if e.kind() != io::ErrorKind::Unsupported { return Err(e); }
                self.r.trim = format!("unsupported after {off} of {size} bytes: {e}");
                return Ok(());
            }
            off += n;
            self.ctx.progress(off);
            self.r.trim = format!("partial: {off} of {size} bytes trimmed");
        }
        if off < size { return Ok(()); }
        self.r.trim = "whole disk trimmed; effectiveness check incomplete".into();
        if self.clock.now() >= self.end { return Ok(()); }
        let n = (4 * MIB).min(self.limits.seq_cap) as usize;
        let mut buf = AlignedBuf::new(n);
        self.pat.fill(0, &mut buf[..n]);
        self.write(dev, 0, &buf[..n])?;
        self.overhead += n as u64;
        self.flush(dev)?;
        self.verify(dev, self.pat, 0, n, false)?;
        let mut off = 0;
        while off < n as u64 {
            let len = chunk.min(n as u64 - off);
            if let Err(e) = self.discard(dev, off, len) {
                self.r.trim = format!("sample discard failed: {e}");
                if e.kind() == io::ErrorKind::Unsupported { return Ok(()); }
                return Err(e);
            }
            off += len;
        }
        self.read(dev, 0, &mut buf[..n])?;
        if buf[..n].as_chunks::<512>().0.iter().enumerate().any(|(i, b)| self.pat.buffer_ok(i as u64 * 512, b)) {
            self.r.trim = "probably not effective (tagged data remains; legal on some drives)".into();
        } else {
            self.r.trim = "effective in the sample".into();
            self.idle(30.)?;
            self.snapshot()?;
        }
        Ok(())
    }
    fn probe(&mut self, dev: &mut dyn Dev) -> io::Result<()> {
        self.phase(2, "Capacity probe");
        let (mut step, mut points) = probe::grid(dev.len(), dev.align() as u64, 1024);
        let max = (self.limits.write_cap.saturating_sub(self.overhead) / dev.align() as u64).min(1024) as usize;
        if max < 2 { return Err(io::Error::other("device too small for diagnostics")); }
        while points.len() > max {
            let last = *points.last().unwrap();
            step *= 2;
            points.retain(|p| p.is_multiple_of(step) || *p == last);
        }
        self.overhead += points.len() as u64 * dev.align() as u64;
        let budget = (self.end - self.clock.now()).max(0.0);
        let ctx = self.ctx.clone();
        let clock = self.clock.clone();
        let result = probe::run_limited(&mut Measured { dev, run: self }, points, step, budget, &clock, &ctx)?;
        self.r.status = match result.finding { Finding::Fake => Status::Fake, Finding::Defective => Status::Defective, Finding::ReadOnly => Status::ReadOnly, Finding::Genuine => Status::Info };
        self.r.probe = Some(result);
        Ok(())
    }
    fn wrap_check(&mut self, dev: &mut dyn Dev, written: u64, rotation: u64) -> io::Result<()> {
        let align = dev.align() as u64;
        self.verify(dev, self.pat, 0, MIB.min(written) as usize, true)?;
        let marks = written.saturating_sub(align) / self.limits.mark_every;
        for i in 0..marks.min(64) {
            let off = (1 + (rotation * 64 + i) % marks) * self.limits.mark_every;
            self.verify(dev, self.pat, off, align as usize, true)?;
        }
        let off = crate::pattern::xorshift(written ^ rotation ^ 0x12345678) % (written / align) * align;
        self.verify(dev, self.pat, off, align as usize, true)
    }
    fn read_speed(&mut self, dev: &mut dyn Dev, len: u64, seconds: f64) -> io::Result<Option<f64>> {
        let len = len / dev.align() as u64 * dev.align() as u64;
        if len < dev.align() as u64 { return Ok(None); }
        let start = self.clock.now();
        let end = (start + seconds).min(self.end);
        let mut bytes = 0;
        let qd = self.r.class.qd();
        let block = self.limits.block.min((len / qd as u64 / dev.align() as u64).max(1) as usize * dev.align());
        while self.clock.now() < end {
            self.ready()?;
            let (mut done, result) = engine::read_window(dev, self.pat, 0, len, block, qd, end, &self.clock, self.ctx);
            if done.is_empty() { result?; break; }
            done.sort_by(|a,b| a.at.total_cmp(&b.at));
            for c in &done {
                self.record_at(c.off, c.len, c.seconds, c.at, false);
                bytes += c.len as u64;
                if let Some(t) = &c.tally {
                    self.r.tally.merge(t);
                    if t.lost() > 0 {
                        self.r.status = if t.aliased > 0 { Status::Fake } else { Status::Defective };
                        // Retry once, retaining the original fault even if it clears.
                        let mut buf = AlignedBuf::new(c.len);
                        self.read(dev, c.off, &mut buf)?;
                        if self.pat.buffer_ok(c.off, &buf) { self.r.status = Status::Unreliable; }
                        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Written data changed at byte {}.", c.off)));
                    }
                }
            }
            result?;
            if done.iter().any(|c| c.gap >= 60.) { return Err(io::Error::other("No I/O completed for 60 s; a blocked read overran the limit.")); }
            for c in done { self.replay_reads(dev, self.pat, c.off, c.len, c.at - c.seconds, c.seconds)?; }
        }
        Ok((bytes > 0).then(|| bytes as f64 / 1e6 / (self.clock.now() - start).max(1e-9)))
    }
    fn sequential(&mut self, dev: &mut dyn Dev, short: bool) -> io::Result<()> {
        self.phase(3, "Sequential write");
        if !short { self.snapshot()?; }
        let cap = self.limits.seq_cap.min(self.limits.write_cap.saturating_sub(self.overhead));
        let cap = if short { cap.min(16 * MIB) } else { cap } / dev.align() as u64 * dev.align() as u64;
        let mut watch = DropWatch::default();
        let mut active = 0.;
        let mut next_check = self.limits.check_every;
        let mut post_start = None;
        let mut post_time = 0.;
        while self.seq_end < cap && self.clock.now() < self.end {
            self.ready()?;
            let start = self.clock.now();
            let off = self.seq_end;
            let mut end = cap.min(next_check);
            if self.r.baseline_read_mb_s.is_none() && post_start.is_none() { end = end.min(256 * MIB); }
            if let Some((_, pos)) = post_start { end = end.min(pos + self.r.class.post_cap() / dev.align() as u64 * dev.align() as u64); }
            if end <= off { break; }
            let deadline = (start + 0.2).min(self.end);
            let (done, result) = engine::write_window(dev, self.pat, off, end, self.limits.block, self.r.class.qd(), deadline, &self.clock, self.ctx);
            let elapsed = self.clock.now() - start;
            for c in &done {
                self.record_at(c.off, c.len, c.seconds, c.at, true);
                self.seq_end = self.seq_end.max(c.off + c.len as u64);
            }
            self.r.written = self.seq_end;
            result?;
            if done.iter().any(|c| c.gap >= 60.) { return Err(io::Error::other("No I/O completed for 60 s; a blocked write overran the limit.")); }
            for c in &done { self.check_stall(dev, c.off, c.seconds)?; }
            if self.seq_end == off { break; }
            active += elapsed;
            let sample = Sample { seconds: active, offset: off, bytes: self.seq_end - off, mb_s: (self.seq_end - off) as f64 / 1e6 / elapsed.max(1e-9) };
            self.ctx.progress(self.seq_end);
            self.ctx.sample(off, sample.mb_s);
            self.r.samples.push(sample.clone());
            // Baseline before the detector can confirm a drop, including small drives.
            if self.r.baseline_read_mb_s.is_none() && post_start.is_none() && self.seq_end >= 4 * MIB
                && (self.seq_end >= 256 * MIB || active >= 2. || self.seq_end == cap) {
                self.r.baseline_read_mb_s = self.read_speed(dev, self.seq_end.min(256 * MIB), 2.)?;
            }
            let drop = watch.push(sample);
            watch.update(&mut self.r);
            if off == 0 { self.verify(dev, self.pat, 0, MIB.min(self.seq_end) as usize, false)?; }
            if self.seq_end >= next_check {
                self.wrap_check(dev, self.seq_end, next_check / self.limits.check_every - 1)?;
                next_check += self.limits.check_every;
            }
            if let Some((_, pos)) = post_start {
                post_time += elapsed;
                self.r.post_mb_s = watch.after();
                if post_time >= 15. || self.seq_end - pos >= self.r.class.post_cap() { break; }
            } else if let Some(pos) = drop {
                self.r.drop_at = Some(pos);
                self.r.drop_confirmed_at = Some(self.seq_end);
                self.r.drop_read_mb_s = self.read_speed(dev, self.seq_end.min(GB), 3.)?;
                self.snapshot()?;
                self.r.heat_suspected |= self.r.baseline_read_mb_s.zip(self.r.drop_read_mb_s).is_some_and(|(a,b)| b < a * 0.75);
                post_start = Some((active, self.seq_end));
            }
            if self.r.class == DriveClass::Usb2 && active >= 10. && (35.0..=42.0).contains(&watch.peak) && (35.0..=42.0).contains(&watch.sustained) {
                self.r.link_limited = true;
                break;
            }
        }
        if self.seq_end > 0 { self.wrap_check(dev, self.seq_end, next_check / self.limits.check_every)?; }
        self.flush(dev)?;
        if let Some(pos) = self.r.drop_at && self.r.post_mb_s.is_none() {
            self.r.warnings.push(format!("Slowdown seen near {}, but the write limit ended before the speed after it could be measured.", crate::report::size(pos)));
        }
        Ok(())
    }
}

// Pick among the eligible 4K blocks except the last one, avoiding a cache hit
// from repeatedly polling one address. Callers require at least two blocks.
fn random_read_offset(rng: &mut u64, previous: &mut Option<u64>, len: u64) -> u64 {
    *rng = crate::pattern::xorshift(*rng);
    let blocks = len / 4096;
    let block = if let Some(last) = *previous {
        let next = *rng % (blocks - 1);
        if next >= last / 4096 { next + 1 } else { next }
    } else { *rng % blocks };
    let off = block * 4096;
    *previous = Some(off);
    off
}

fn mixed_read(dev: &mut dyn Dev, pat: Pattern, rng: &mut u64, previous: &mut Option<u64>, len: u64, buf: &mut [u8]) -> (u64, io::Result<()>) {
    let off = random_read_offset(rng, previous, len);
    let result = dev.read_at(off, buf).and_then(|n| if n != buf.len() { Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short overlapping read")) }
        else if !pat.buffer_ok(off, buf) { Err(io::Error::new(io::ErrorKind::InvalidData, "Old data changed during mixed writes")) } else { Ok(()) });
    (off, result)
}

/// No attribution from one or two pauses; position repetition needs distinct phases.
pub fn interpret_stalls(stalls: &[Stall]) -> String {
    if stalls.len() < 3 { return if stalls.is_empty() { "No stalls of 2 s or longer." } else { "Too few stalls to attribute a cause." }.into(); }
    for s in stalls {
        let Some(off) = s.offset else { continue; };
        let same: Vec<_> = stalls.iter().filter(|x| x.offset.is_some_and(|p| p / MIB == off / MIB)).collect();
        if same.len() >= 3 && same.iter().any(|x| x.phase != s.phase) {
            return format!("Bad regions near {}: stalls repeated here across phases.", crate::report::size(off / MIB * MIB));
        }
    }
    let mut times: Vec<_> = stalls.iter().collect();
    times.sort_by(|a,b| a.at.total_cmp(&b.at));
    if times.windows(3).any(|w| w[2].at - w[0].at <= 60. && w[0].seconds < w[1].seconds && w[1].seconds < w[2].seconds) {
        "Throttling or power problem: speed may degrade in long use.".into()
    } else { "Random hiccups, likely harmless; no repeatable region or worsening time pattern.".into() }
}

fn next_idle(previous: f64, bytes: u64, original: u64, speed: f64, peak: f64, capped: bool) -> Option<f64> {
    if bytes as f64 >= original as f64 * 0.9 && speed >= peak * 0.9 { return None; }
    if previous >= 30. { return None; }
    if previous == 3. && bytes as f64 <= original as f64 * 0.1 && !capped { Some(60.) }
    else if previous == 3. { Some(15.) } else { Some(30.) }
}

impl Run<'_> {
    fn soft(&mut self, result: io::Result<()>) -> io::Result<()> {
        // Device timeouts are hard errors; only our phase deadline is a soft stop.
        match result {
            Err(e) if e.get_ref().is_some_and(|cause| cause.is::<Budget>()) => {
                let phase = &self.r.phases.last().unwrap().name;
                if phase == "Sequential write" {
                    if let Some(pos) = self.r.drop_at && self.r.post_mb_s.is_none() {
                        self.r.warnings.push(format!("Slowdown seen near {}, but the budget ended before the speed after it could be measured.", crate::report::size(pos)));
                    }
                } else { self.r.warnings.push(format!("{phase} reached its budget; unfinished measurements are omitted.")); }
                Ok(())
            }
            r => r,
        }
    }
    fn snapshot(&mut self) -> io::Result<()> {
        self.ready()?;
        let t = self.clock.now();
        let observed = (self.observe)();
        self.record(0, 0, t, false)?;
        let current = match observed {
            Err(e) => { self.r.identity_changes.push(e.to_string()); return Err(e); }
            Ok(s) => s,
        };
        if current.is_none() && self.before.is_some() {
            self.r.warnings.push("SMART could not be read at this checkpoint; temperature and counter changes are unavailable.".into());
        }
        if let Some(s) = current {
            let run = self.r.smart.get_or_insert_with(SmartRun::default);
            run.via = s.via.clone();
            run.warn_temp = s.warn_temp;
            if let Some(c) = s.get(Counter::Temperature) {
                run.temps.push(Temp { secs: self.clock.now(), pos: self.seq_end, c });
                self.ctx.temp(self.seq_end, c);
                if let Some(first) = self.before.as_ref().and_then(|b| b.get(Counter::Temperature)) {
                    self.r.heat_suspected |= self.r.drop_at.is_some() && c >= first + 10;
                }
            }
            if let Some(before) = &self.before { run.changes = crate::smart::changes(before, &s); }
            if failing(&s) || run.changes.iter().any(|c| c.what.bad() && c.after > c.before) {
                self.r.status = Status::Unreliable;
                return Err(io::Error::other("The drive reports an alarm or rising SMART error counters."));
            }
        }
        Ok(())
    }
    fn flush(&mut self, dev: &mut dyn Dev) -> io::Result<()> {
        self.ready()?;
        let t = self.clock.now();
        let result = dev.flush();
        self.record(0, 0, t, true)?;
        result
    }
    fn random(&mut self, dev: &mut dyn Dev, start: u64, span: u64, write: bool, qd: u32, seconds: f64) -> io::Result<crate::perf::IopsRow> {
        self.ready()?;
        let handles = (0..qd).map(|_| dev.fork()).collect::<io::Result<Vec<_>>>()?;
        let result = crate::perf::random_io(handles, crate::perf::RandomOpts { start, span, write, pattern: Some(self.pat), deadline: (self.clock.now() + seconds).min(self.end) }, &self.clock, self.ctx);
        let p = self.r.phases.last_mut().unwrap();
        p.worst_ms = p.worst_ms.max(result.row.max_ms);
        p.operations += result.operations;
        p.bytes += result.operations * 4096;
        p.latency.extend(result.latencies.iter().take(65536usize.saturating_sub(p.latency.len())).map(|ms| *ms as f64 / 1000.));
        self.r.tally.merge(&result.tally);
        for c in &result.stalls { self.r.stalls.push(Stall { phase: p.name.clone(), offset: Some(c.off), at: c.at, seconds: c.seconds, write, write_path: false }); }
        self.r.iops.rows.push(result.row.clone());
        if let Some(e) = result.error {
            if e.kind() == io::ErrorKind::InvalidData { self.r.status = if result.tally.aliased > 0 { Status::Fake } else { Status::Defective }; }
            return Err(e);
        }
        if self.ctx.cancelled() { return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")); }
        for c in &result.stalls {
            if write {
                let t = self.clock.now();
                self.verify(dev, self.pat, c.off, 4096, false)?;
                if let Some(s) = self.r.stalls.iter_mut().rev().find(|s| s.offset == Some(c.off) && s.write) { s.write_path = self.clock.now() - t < 0.1; }
            }
        }
        if !result.stalls.is_empty() && !self.replayed && self.clock.now() < self.end {
            self.replayed = true;
            self.r.warnings.push("Repeating the last random-I/O measurement for up to 10 s after a stall.".into());
            self.random(dev, start, span, write, qd, 10.)?;
        }
        Ok(result.row)
    }
    fn small(&mut self, dev: &mut dyn Dev) -> io::Result<()> {
        self.phase(4, "Mixed small writes and random 4K");
        if self.seq_end < 4096 { return Ok(()); }
        let align = dev.sector_size() as u64;
        let free = self.limits.write_cap.saturating_sub(self.overhead + self.seq_end);
        let span = free.min(if self.r.class.ssd() { 256 * MIB } else { 32 * MIB }) / dev.align() as u64 * dev.align() as u64;
        if span < 128 * 1024 { self.r.warnings.push("Small-write region did not fit within the write cap.".into()); return Ok(()); }
        self.mixed = (self.seq_end, self.seq_end);
        // Initialise only what fits in a sixth of the phase, leaving time to measure
        // and verify even a slow stick. Random reads need known contents throughout.
        let init_end = self.clock.now() + (self.end - self.clock.now()).max(0.) / 6.;
        self.ctx.log(format!("Small writes: preparing {} at {}.", crate::report::size(span), crate::report::size(self.seq_end)));
        let mut buf = AlignedBuf::new(self.limits.block.max(128 * 1024));
        while self.mixed.1 - self.mixed.0 < span && self.clock.now() < init_end {
            let n = (span - (self.mixed.1 - self.mixed.0)).min(self.limits.block as u64) as usize;
            self.pat.fill(self.mixed.1, &mut buf[..n]);
            self.write(dev, self.mixed.1, &buf[..n])?;
            self.mixed.1 += n as u64;
        }
        let span = self.mixed.1 - self.mixed.0;
        if span < 128 * 1024 { self.r.warnings.push("Too little time to initialise the small-write region.".into()); return Ok(()); }
        let seconds: f64 = if self.r.class == DriveClass::Nvme { 15. } else { 10. };
        let read_len = self.r.drop_at.unwrap_or(self.seq_end).min(self.seq_end) / 4096 * 4096;
        if read_len < 8192 { self.r.warnings.push("Too little pre-drop data for varying reads during writes.".into()); return Ok(()); }
        self.ctx.log(format!("Small writes: idle 4K reads over the first {}.", crate::report::size(read_len)));
        let idle = self.random(dev, 0, read_len, false, 1, seconds.min((self.end - self.clock.now()).max(0.) / 5.))?;
        self.r.idle_read_ms = Some(idle.p50_ms);
        let third = (self.end - self.clock.now()).max(0.) / 3.;
        let mixed_end = self.clock.now() + third * 2.;
        let write_end = self.clock.now() + third;
        let started = self.clock.now();
        let mut mixed = Phase { name: "Mixed small writes".into(), ..Default::default() };
        self.ctx.log("Small writes: mixed writes with overlapping reads.".to_string());
        let mut reader = dev.fork()?;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let stop = std::sync::atomic::AtomicBool::new(false);
        let pat = self.pat;
        let clock = self.clock.clone();
        let ctx = self.ctx.clone();
        let result = std::thread::scope(|scope| {
            let stop_ref = &stop;
            let read_thread = scope.spawn(move || {
                let mut samples = vec![];
                let mut buf = AlignedBuf::new(4096);
                let (mut rng, mut previous) = (new_tag(), None);
                loop {
                    match rx.recv_timeout(std::time::Duration::from_secs(1)) {
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        _ if stop_ref.load(std::sync::atomic::Ordering::SeqCst) || ctx.cancelled() || clock.now() >= write_end => break,
                        _ => {}
                    }
                    let start = clock.now();
                    let (off, r) = mixed_read(&mut *reader, pat, &mut rng, &mut previous, read_len, &mut buf);
                    let gap = clock.complete(start, clock.now());
                    samples.push((off, start, clock.now() - start, r, gap));
                    if samples.last().unwrap().3.is_err() { stop_ref.store(true, std::sync::atomic::Ordering::SeqCst); break; }
                }
                samples
            });
            let writes = (|| {
                let (mut rng, mut pos, mut count) = (new_tag(), 0, 0u64);
                let mut history = std::collections::VecDeque::new();
                let mut resume = None;
                while self.clock.now() < write_end && !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    let now = self.clock.now();
                    if let Some((until, saved_rng, saved_pos, saved_count)) = resume && now >= until {
                        (rng, pos, count) = (saved_rng, saved_pos, saved_count); resume = None;
                    }
                    if resume.is_none() && history.back().is_none_or(|&(t, _, _, _)| now - t >= 0.2) {
                        history.push_back((now, rng, pos, count));
                        while history.len() > 1 && history[1].0 < now - 10. { history.pop_front(); }
                    }
                    if count.is_multiple_of(16) { let _ = tx.try_send(()); }
                    rng = crate::pattern::xorshift(rng);
                    // Mostly sector/4K writes, occasional 16K, 64K and 128K; nearby runs.
                    let size = match rng % 16 { 0 => 128 * 1024, 1 => 64 * 1024, 2..=4 => 16 * 1024, 5..=9 => 4096, _ => align };
                    let n = size.div_ceil(align) * align;
                    if count.is_multiple_of(16) { pos = (rng % (span / align)) * align; }
                    if pos + n > span { pos = 0; }
                    let off = self.mixed.0 + pos;
                    self.pat.fill(off, &mut buf[..n as usize]);
                    let t = self.clock.now();
                    self.write(dev, off, &buf[..n as usize])?;
                    let elapsed = self.clock.now() - t;
                    mixed.bytes += n;
                    mixed.operations += 1;
                    mixed.worst_ms = mixed.worst_ms.max(elapsed * 1000.);
                    if mixed.latency.len() < 65536 { mixed.latency.push(elapsed); }
                    self.check_stall(dev, off, elapsed)?;
                    pos += n;
                    count += 1;
                    if elapsed >= 2. && !self.replayed {
                        self.replayed = true;
                        resume = Some(((self.clock.now() + 10.).min(write_end), rng, pos, count));
                        if let Some(&(_, saved_rng, saved_pos, saved_count)) = history.front() { (rng, pos, count) = (saved_rng, saved_pos, saved_count); }
                        self.r.warnings.push("Repeating up to the last 10 s of mixed writes once after a stall.".into());
                    }
                }
                Ok::<_, io::Error>(())
            })();
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            drop(tx);
            let reads = read_thread.join().unwrap();
            let mut loaded = Phase { name: "Reads during writes".into(), ..Default::default() };
            for (off, start, elapsed, result, gap) in reads {
                self.record_at(off, 4096, elapsed, start + elapsed, false);
                loaded.latency.push(elapsed); loaded.operations += 1;
                loaded.worst_ms = loaded.worst_ms.max(elapsed * 1000.);
                if result.is_err() { self.r.status = Status::Unreliable; }
                result?;
                if gap >= 60. { return Err(io::Error::other(format!("No I/O completed for {gap:.1} s during an overlapping read."))); }
            }
            loaded.finish();
            self.r.reads_freeze = loaded.p99_ms >= 100. && loaded.p99_ms >= idle.p50_ms.max(0.001) * 50.;
            if loaded.operations > 0 { self.r.loaded_reads = Some(loaded); }
            writes
        });
        mixed.seconds = self.clock.now() - started;
        mixed.finish();
        self.r.mixed = Some(mixed);
        result?;
        self.flush(dev)?;
        let saved = self.end;
        self.end = mixed_end;
        let verify = (|| {
            let mut off = self.mixed.0;
            while off < self.mixed.1 {
                let n = (self.mixed.1 - off).min(self.limits.block as u64) as usize;
                self.verify(dev, self.pat, off, n, false)?;
                off += n as u64;
            }
            Ok(())
        })();
        self.end = saved;
        // An unfinished read-back is not a successful mixed-write measurement.
        if verify.is_err() { self.r.warnings.push("Mixed-write read-back was incomplete.".into()); }
        self.r.mixed_verified = verify.is_ok();
        self.soft(verify)?;
        for write in [false, true] {
            let mut previous = if write { self.random(dev, self.mixed.0, span, true, 1, seconds)?.iops } else { idle.iops };
            for qd in [4, 32] {
                if self.end - self.clock.now() < seconds { break; }
                let row = self.random(dev, if write { self.mixed.0 } else { 0 }, if write { span } else { self.seq_end }, write, qd, seconds)?;
                if row.iops < previous * 1.15 { break; }
                previous = row.iops;
            }
        }
        self.r.iops.span = span;
        self.r.iops.seconds_each = seconds;
        Ok(())
    }
    fn check_stall(&mut self, dev: &mut dyn Dev, off: u64, elapsed: f64) -> io::Result<()> {
        if elapsed < 2. { return Ok(()); }
        let start = self.clock.now();
        let alignment = dev.sector_size();
        self.verify(dev, self.pat, off, alignment, false)?;
        let fast = self.clock.now() - start < 0.1;
        if let Some(stall) = self.r.stalls.iter_mut().rev().find(|s| s.offset == Some(off) && s.write) { stall.write_path = fast; }
        if !self.replayed && !self.r.phases.last().unwrap().name.starts_with("Mixed") {
            self.replayed = true;
            self.r.warnings.push(format!("{}: repeating up to the last 10 s of writes once after a stall.", self.r.phases.last().unwrap().name));
            let bytes = self.r.samples.iter().rev().take_while(|s| self.r.samples.last().unwrap().seconds - s.seconds <= 10.).map(|s| s.bytes).sum::<u64>().max(dev.align() as u64);
            let start = off.saturating_sub(bytes) / dev.align() as u64 * dev.align() as u64;
            let end = off + dev.align() as u64;
            let until = (self.clock.now() + 10.).min(self.end);
            while self.clock.now() < until {
                let (done, result) = engine::write_window(dev, self.pat, start, end, self.limits.block, self.r.class.qd(), until, &self.clock, self.ctx);
                for c in &done { self.record_at(c.off, c.len, c.seconds, c.at, true); }
                result?;
                if done.iter().any(|c| c.gap >= 60.) { return Err(io::Error::other("No I/O completed for 60 s during stall replay.")); }
                if done.is_empty() { break; }
            }
        }
        Ok(())
    }
    fn recover(&mut self, dev: &mut dyn Dev) -> io::Result<()> {
        if self.r.drop_at.is_none() || self.r.class == DriveClass::Usb2 { return Ok(()); }
        self.phase(5, "Recovery");
        let original = self.r.cache_bytes.unwrap_or(self.r.drop_at.unwrap());
        if self.r.heat_suspected {
            while self.end - self.clock.now() >= 4. {
                self.idle(3.)?;
                self.snapshot()?;
                if self.read_speed(dev, self.seq_end.min(GB), 1.)?.zip(self.r.baseline_read_mb_s).is_some_and(|(now, before)| now >= before * 0.9) { self.r.thermal_recovery = true; break; }
            }
            return Ok(());
        }
        let cap = self.limits.write_cap.saturating_sub(self.overhead);
        self.recovery = (self.mixed.1.max(self.seq_end), self.mixed.1.max(self.seq_end));
        let mut idle = 3.;
        while self.recovery.1 < cap && self.end - self.clock.now() > idle {
            self.idle(idle)?;
            self.snapshot()?;
            let start = self.recovery.1;
            let t = self.clock.now();
            let until = (t + 20.).min(self.end);
            let (mut fast_bytes, mut fast_time, mut dropped) = (0u64, 0., false);
            while self.clock.now() < until && self.recovery.1 < cap {
                let off = self.recovery.1;
                let before = self.clock.now();
                let (done, result) = engine::write_window(dev, self.pat, off, cap, self.limits.block, self.r.class.qd(), (before + 0.2).min(until), &self.clock, self.ctx);
                for c in &done { self.record_at(c.off, c.len, c.seconds, c.at, true); self.recovery.1 = self.recovery.1.max(c.off + c.len as u64); }
                result?;
                if done.iter().any(|c| c.gap >= 60.) { return Err(io::Error::other("No I/O completed for 60 s during recovery.")); }
                if self.recovery.1 == off { break; }
                let elapsed = self.clock.now() - before;
                for c in &done { self.check_stall(dev, c.off, c.seconds)?; }
                let speed = (self.recovery.1 - off) as f64 / 1e6 / elapsed.max(1e-9);
                if speed < self.r.peak_mb_s * 0.85 { dropped = true; break; }
                fast_bytes += self.recovery.1 - off;
                fast_time += elapsed;
            }
            let speed = fast_bytes as f64 / 1e6 / fast_time.max(1e-9);
            let full = original > 0 && fast_bytes as f64 >= original as f64 * 0.9 && speed >= self.r.peak_mb_s * 0.9;
            self.r.recovery.push(Recovery { idle_seconds: idle, fast_bytes, mb_s: speed, lower_bound: !dropped, full });
            if self.recovery.1 > start { let alignment = dev.align(); self.verify(dev, self.pat, start, alignment, false)?; }
            self.wrap_check(dev, self.seq_end, self.r.recovery.len() as u64)?;
            let Some(next) = next_idle(idle, fast_bytes, original, speed, self.r.peak_mb_s, !dropped) else { break; };
            idle = next;
        }
        Ok(())
    }
    fn final_checks(&mut self, dev: &mut dyn Dev) -> io::Result<()> {
        self.phase(6, "Final verification");
        self.flush(dev)?;
        if let Some(p) = self.r.probe.clone() {
            for (i, &off) in p.positions.iter().enumerate() {
                let tag = if p.point_tags.is_empty() { p.tag } else { p.point_tags[i] };
                let Some(tag) = tag else { continue; };
                // Probe points touched by a later phase now carry the main run tag.
                let pat = if off < self.seq_end || (self.mixed.0..self.mixed.1).contains(&off) || (self.recovery.0..self.recovery.1).contains(&off) { self.pat } else { Pattern::Seeded { tag } };
                self.verify(dev, pat, off, p.block as usize, false)?;
            }
        }
        let drop = self.r.drop_at.unwrap_or(self.seq_end).min(self.seq_end);
        let spans = [("Before drop", 0, drop), ("After drop", drop, self.seq_end), ("Recovery", self.recovery.0, self.recovery.1)];
        let count = spans.iter().filter(|(_,a,b)| b > a).count().max(1);
        let sample = if self.r.class.ssd() { 256 * MIB } else { 16 * MIB } / count as u64;
        for (name, start, end) in spans {
            if end <= start { continue; }
            let align = dev.align() as u64;
            let wanted = sample.min(end - start) / align * align;
            let mut zone = Zone { name: name.into(), start, end, ..Default::default() };
            let block = self.limits.block.min(wanted as usize).max(align as usize);
            let chunks = wanted.div_ceil(block as u64);
            for i in 0..chunks {
                let n = (wanted - zone.verified).min(block as u64);
                let off = start + ((end - start - n) * i / chunks.max(1)) / align * align;
                self.verify(dev, self.pat, off, n as usize, false)?;
                zone.verified += n;
            }
            let t = self.clock.now();
            self.verify(dev, self.pat, start, align as usize, false)?;
            zone.read_4k_ms = Some((self.clock.now() - t) * 1000.);
            self.r.zones.push(zone);
        }
        if self.mixed.1 > self.mixed.0 {
            let mut off = self.mixed.0;
            while off < self.mixed.1 {
                let n = (self.mixed.1 - off).min(self.limits.block as u64) as usize;
                self.verify(dev, self.pat, off, n, false)?;
                off += n as u64;
            }
        }
        self.r.read_mb_s = self.read_speed(dev, drop, 15f64.min((self.end - self.clock.now() - 2.).max(0.)))?;
        self.snapshot()?;
        self.r.final_complete = self.seq_end > 0 && self.r.probe.is_some();
        Ok(())
    }
}

/// Deadlines also cover the probe's speed calibration and all its passes.
struct Measured<'a, 'b> { dev: &'a mut dyn Dev, run: &'a mut Run<'b> }
impl Dev for Measured<'_, '_> {
    fn len(&self) -> u64 { self.dev.len() }
    fn align(&self) -> usize { self.dev.align() }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> { self.run.write(self.dev, off, buf) }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> { self.run.read(self.dev, off, buf).map(|_| buf.len()) }
    fn flush(&mut self) -> io::Result<()> { self.run.ready()?; let t = self.run.clock.now(); let r = self.dev.flush(); self.run.record(0, 0, t, true)?; r }
}

pub fn run_device(dev: &mut dyn Dev, disk: &DiskInfo, smart: Option<&Smart>, max_time: Option<f64>, ctx: &Ctx) -> Diagnostics {
    let class = classify(disk, smart);
    let limits = Limits::new(class, disk.size, dev.align() as u64, max_time);
    run_core(dev, class, smart.cloned(), limits, Clock::default(), &mut || Ok(None), ctx)
}

/// The raw owner remains alive for the entire run, including all worker handles.
pub fn run_disk(dev: &mut dyn Dev, disk: &DiskInfo, max_time: Option<f64>, ctx: &Ctx) -> Diagnostics {
    let clock = Clock::default();
    ctx.phase("Triage: identity and SMART", 0);
    let before = crate::smart::read(disk).ok();
    let class = classify(disk, before.as_ref());
    let limits = Limits::new(class, dev.len(), dev.align() as u64, max_time);
    let has_smart = before.is_some();
    let mut observe = || {
        let now = crate::platform::disk(&disk.id)?;
        let changes = crate::trust::identity_changes(disk, &now);
        if disk.check_identity(&now).is_err() || !changes.is_empty() {
            return Err(io::Error::other(format!("Drive identity changed: {}", changes.join("; "))));
        }
        Ok(if has_smart { crate::smart::read(disk).ok() } else { None })
    };
    run_core(dev, class, before, limits, clock, &mut observe, ctx)
}

#[cfg(test)]
fn run_with(dev: &mut dyn Dev, class: DriveClass, short: bool, limits: Limits, clock: Clock, ctx: &Ctx) -> Diagnostics {
    let before = short.then(|| Smart { alarms: vec!["Test alarm".into()], ..Default::default() });
    run_core(dev, class, before, limits, clock, &mut || Ok(None), ctx)
}

#[allow(clippy::too_many_arguments)]
fn run_core(dev: &mut dyn Dev, class: DriveClass, before: Option<Smart>, limits: Limits, clock: Clock, observe: &mut (dyn FnMut() -> io::Result<Option<Smart>> + Send), ctx: &Ctx) -> Diagnostics {
    let short = before.as_ref().is_some_and(failing);
    let mut r = Diagnostics { class, budget_seconds: limits.total, triage_failing: short, ..Default::default() };
    if let Some(s) = &before {
        r.smart = Some(SmartRun { via: s.via.clone(), warn_temp: s.warn_temp, temps: s.get(Counter::Temperature).map(|c| vec![Temp { secs: clock.now(), pos: 0, c }]).unwrap_or_default(), ..Default::default() });
        r.warnings.extend(s.alarms.clone());
        if s.get(Counter::Reallocated).unwrap_or(0) > 0 { r.warnings.push("The drive reports remapped sectors; an increase during this run is a problem.".into()); }
    }
    let mut run = Run { clock, ctx, r, limits, pat: Pattern::Seeded { tag: new_tag() }, end: 0., phase_start: 0., seq_end: 0, overhead: 0,
        mixed: (0,0), recovery: (0,0), before, observe, replayed: false, reads: Default::default() };
    let result = (|| {
        run.phase(0, "Triage");
        run.phase_start = 0.;
        if run.clock.now() > 0. { run.record(0, 0, 0., false)?; }
        if !short { let result = run.trim(dev); run.soft(result)?; }
        let result = run.probe(dev); run.soft(result)?;
        if run.r.status.exit_code() == 1 { return Ok(()); }
        let result = run.sequential(dev, short);
        run.r.check_drop(); // A rebound invalidates cache figures before recovery testing.
        run.soft(result)?;
        if !short {
            let result = run.small(dev); run.soft(result)?;
            let result = run.recover(dev); run.soft(result)?;
        }
        let result = run.final_checks(dev); run.soft(result)?;
        Ok::<_, io::Error>(())
    })();
    if let Err(e) = result {
        run.r.reason = e.to_string();
        if run.ctx.cancelled() { run.r.status = Status::Cancelled; }
        else if run.r.status.exit_code() != 1 { run.r.status = Status::Unreliable; }
    }
    run.r.distinct_upper_bound = run.overhead + run.seq_end + (run.mixed.1 - run.mixed.0) + (run.recovery.1 - run.recovery.0);
    run.finish_phase();
    run.r.seconds = run.clock.now();
    if run.r.seconds > run.limits.total { run.r.warnings.push(format!("The last kernel call returned {:.1} s after the overall deadline; limits can only be checked between I/Os.", run.r.seconds - run.limits.total)); }
    run.r.stall_interpretation = interpret_stalls(&run.r.stalls);
    finish_verdict(&mut run.r);
    run.r
}

fn finish_verdict(r: &mut Diagnostics) {
    r.check_drop();
    if r.status == Status::Cancelled { return; }
    if r.status.exit_code() != 1 {
        r.status = if r.triage_failing || !r.identity_changes.is_empty() || r.smart.as_ref().is_some_and(|s| s.changes.iter().any(|c| c.what.bad() && c.after > c.before)) { Status::Unreliable }
            else if r.tally.lost() > 0 { if r.tally.aliased > 0 { Status::Fake } else { Status::Defective } }
            else if r.final_complete && r.probe.is_some() && r.written > 0 { Status::Ok } else { Status::Info };
    }
    if r.reason.is_empty() { r.reason = match r.status {
        Status::Ok => "Genuine in the samples tested. No data loss or failure was found; a full test is needed to check every byte.",
        Status::Fake => "Fake capacity: later writes replaced earlier data or the capacity probe found a failed tail.",
        Status::Defective | Status::ReadOnly => "Failing: the drive did not retain the written data.",
        Status::Unreliable if r.triage_failing => "Failing: the drive reports a SMART alarm or pending bad sectors.",
        Status::Unreliable => "Problems found: the drive or its connection reported errors.",
        _ => "Incomplete: the budget ended before all diagnostic checks finished.",
    }.into(); }
    if let Some(p) = &r.probe && p.partial { r.warnings.push(p.partial_summary()); }
    if r.triage_failing { r.warnings.push("SMART was already reporting failure; only the probe and a small verified sample were run.".into()); }
    if r.reads_freeze { r.warnings.push("Bulk storage only: it freezes reads while writing.".into()); }
    if r.link_limited { r.warnings.push("The measured speed is at the USB 2 ceiling (about 35-40 MB/s); this port limits speed.".into()); }
    if r.post_mb_s.or(r.sustained_mb_s).is_some_and(|s| s > 0. && s < 5.) { r.warnings.push("Sequential writes are very slow.".into()); }
    if r.cache_caveat() { r.warnings.push("Cache figures may be understated: the drive was not trimmed.".into()); }
    if let Some(s) = r.stalls.iter().filter(|s| s.offset.is_some() && s.seconds >= 10.).max_by(|a,b| a.seconds.total_cmp(&b.seconds)) {
        r.warnings.push(format!("The drive stopped responding for {:.1} s; long pauses like this make copies hang. Only 60 s without an I/O completion aborts the run.", s.seconds));
    }
    if !r.stalls.is_empty() { r.warnings.push(r.stall_interpretation.clone()); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::{CacheDev, MemDev, SimMode};

    fn limits(class: DriveClass, size: u64) -> Limits {
        Limits { block: 64 * 1024, check_every: MIB, mark_every: 128 * 1024, ..Limits::new(class, size, 4096, None) }
    }

    #[test]
    fn class_and_short_run_rules() {
        let mut d = DiskInfo { bus: "USB".into(), ..Default::default() };
        assert_eq!(classify(&d, None), DriveClass::Usb);
        d.ssd_hint = Some(true);
        assert_eq!(classify(&d, None), DriveClass::Sata);
        d.ssd_hint = Some(false);
        assert_eq!(classify(&d, None), DriveClass::Usb);
        d.ssd_hint = None;
        let mut s = Smart { via: "SAT over USB".into(), ..Default::default() };
        assert_eq!(classify(&d, Some(&s)), DriveClass::Sata);
        d.usb = Some(crate::platform::UsbInfo { slow_link: true, ..Default::default() });
        assert_eq!(classify(&d, Some(&s)), DriveClass::Usb2);
        s.key.push(crate::smart::Key { what: Counter::Reallocated, value: 42 });
        assert!(!failing(&s));
        s.key.push(crate::smart::Key { what: Counter::Pending, value: 1 });
        assert!(failing(&s));
    }

    fn hardware_trace(name: &str, speed: (f64, f64), sustained: (f64, f64), cache_mb: Option<(f64, f64)>) {
        let block = include_str!("../tests/data/diagnose-traces.txt").split("# ").find(|b| b.starts_with(&format!("{name}:"))).unwrap();
        let (expectation, samples) = block.split_once('\n').unwrap();
        let expected_drop = expectation.contains("report a drop") && !expectation.contains("must not report a drop");
        assert_eq!(expected_drop, cache_mb.is_some(), "{expectation}");
        let mut watch = DropWatch::default();
        let mut found = None;
        let mut confirmed = 0.;
        let mut before = 0.;
        for token in samples.split_whitespace() {
            let fields: Vec<_> = token.split(':').collect();
            let sample = Sample { seconds: fields[0].parse().unwrap(), offset: fields[1].parse().unwrap(),
                bytes: fields[2].parse().unwrap(), mb_s: fields[3].parse().unwrap() };
            let now = sample.seconds;
            if let Some(pos) = watch.push(sample) {
                assert!(expected_drop, "{expectation}: false drop at {now}");
                if found.is_none() { found = Some(pos); confirmed = now; before = watch.peak; }
                assert_eq!(found, Some(pos));
                assert_eq!(watch.peak, before);
            }
        }
        assert_eq!(found.is_some(), expected_drop, "{expectation}");
        assert!((speed.0..=speed.1).contains(&watch.peak), "{name}: {}", watch.peak);
        let measured = watch.after().unwrap_or(watch.sustained);
        assert!((sustained.0..=sustained.1).contains(&measured), "{name}: {measured}");
        if let Some((lo, hi)) = cache_mb { assert!((lo..=hi).contains(&(watch.cache_bytes().unwrap() as f64 / 1e6))); }
        if name == "wd_ssd_fedora_30m" {
            assert!((554. * GB as f64..555. * GB as f64).contains(&(found.unwrap() as f64)));
            assert!((675. ..680.).contains(&confirmed), "confirmation moved: {confirmed}");
        }
        if name.starts_with("kingston") || name.starts_with("verbatim") {
            assert!(confirmed < Limits::new(DriveClass::Usb2, 64 * GB, 4096, None).phases[3] - 5., "confirmation must leave time for read checks");
        }
        let last = watch.samples.last().unwrap();
        let mut d = Diagnostics { written: last.offset + last.bytes, drop_at: found, post_mb_s: watch.after(),
            trim: "unsupported".into(), baseline_read_mb_s: Some(100.), final_complete: true, probe: Some(Default::default()), ..Default::default() };
        watch.update(&mut d);
        finish_verdict(&mut d);
        assert_eq!(d.drop_at, found); // The final consistency guard must retain genuine drops.
        assert!(!d.warnings.iter().any(|s| s.contains("very slow")));
        if expectation.contains("sawtooth") && !expected_drop {
            assert_eq!(d.write_pattern, "bursts with regular pauses");
            assert!(d.summary_lines().iter().any(|s| s.contains("cache may not have recovered")));
        }
        let report = report(&DiskInfo::default(), d);
        let line = if expected_drop { format!("{:.1} MB/s before the drop", watch.peak) }
            else if watch.sawtooth { format!("Writes in bursts at {:.1} MB/s", watch.burst) }
            else { format!("sustained {:.1} MB/s", watch.sustained) };
        assert!(report.text().contains(&line));
        assert!(report.html().contains(&line));
        let mut json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["diagnostics"]["peak_mb_s"], watch.peak);
        assert_eq!(json["diagnostics"]["sustained_mb_s"], watch.sustained);
        for field in ["burst_mb_s", "sustained_mb_s", "write_pattern", "cache_bytes"] { json["diagnostics"].as_object_mut().unwrap().remove(field); }
        let old: crate::report::Report = serde_json::from_value(json).unwrap();
        assert!(old.diagnostics.unwrap().sustained_mb_s.is_none());
        println!("{name}: {} {:.2} MB/s, sustained/after {measured:.2} MB/s, onset {:?} MB, cache {:?} MB, confirmed {confirmed:.2} s",
            if expected_drop { "before" } else { "burst" }, watch.peak, found.map(|n| n as f64 / 1e6), watch.cache_bytes().map(|n| n as f64 / 1e6));
    }

    #[test]
    fn trace_transcend_drop_falsely_reported() { hardware_trace("transcend_drop_falsely_reported", (23., 26.), (17., 20.), None); }
    #[test]
    fn trace_transcend_p4() { hardware_trace("transcend_p4", (21., 25.), (14., 19.), None); }
    #[test]
    fn trace_kingston_r3() { hardware_trace("kingston_r3", (23., 27.), (10., 14.), Some((60., 90.))); }
    #[test]
    fn trace_kingston_p4() { hardware_trace("kingston_p4", (23., 27.), (10., 14.), Some((40., 65.))); }
    #[test]
    fn trace_verbatim_p4() { hardware_trace("verbatim_p4", (95., 105.), (18., 25.), Some((150., 210.))); }
    #[test]
    fn trace_smi_r3() { hardware_trace("smi_r3", (75., 85.), (20., 32.), Some((650., 730.))); }
    #[test]
    fn trace_smi_p4() { hardware_trace("smi_p4", (75., 85.), (26., 32.), None); }
    #[test]
    fn trace_lexar_r3() { hardware_trace("lexar_r3", (24., 30.), (35., 45.), None); }
    #[test]
    fn trace_wd_ssd_p4() { hardware_trace("wd_ssd_p4", (340., 360.), (340., 360.), None); }

    #[test]
    fn trace_wd_ssd_fedora_30m() { hardware_trace("wd_ssd_fedora_30m", (825., 845.), (360., 420.), Some((554_000., 555_000.))); }
    #[test]
    fn trace_wd_ssd_fedora_default() { hardware_trace("wd_ssd_fedora_default", (840., 860.), (825., 850.), None); }

    #[test]
    fn early_dips_do_not_move_a_later_sustained_cliff() {
        let mut d = CacheDev::new(4 * MIB, 4 * MIB, SimMode::Genuine, 4 * MIB, 40960., 16384.);
        let mut watch = DropWatch::default();
        let mut found = None;
        for i in 0..300 {
            // A multi-second early dip, followed by a long fast region and a real cliff.
            let speed = if i >= 200 { 16384. } else if (20..32).contains(&i) { 28672. } else { 40960. };
            { let mut s = d.state.lock().unwrap(); s.fast = speed; s.slow = speed; }
            let start = d.clock.now();
            d.write_at(i * 8192, &[0;8192]).unwrap();
            found = watch.push(Sample { seconds: d.clock.now(), offset: i * 8192, bytes: 8192, mb_s: 8192. / 1e6 / (d.clock.now() - start) });
            if i < 220 { assert!(found.is_none()); }
        }
        assert_eq!(found, Some(200 * 8192));
        assert_eq!(watch.cache_bytes(), found);
        assert!((0.039..0.041).contains(&watch.peak));
        assert!((watch.after().unwrap() - 0.016384).abs() < 1e-9);
    }

    #[test]
    fn a_rebound_discards_drop_figures_but_keeps_other_results() {
        let mut d = CacheDev::new(2 * MIB, 2 * MIB, SimMode::Genuine, 20 * 8192, 40960., 16384.);
        let mut watch = DropWatch::default();
        let mut found = None;
        for i in 0..150 {
            if i == 50 {
                assert!(found.is_some());
                d.state.lock().unwrap().slow = 81920.;
            }
            let start = d.clock.now();
            d.write_at(i * 8192, &[0;8192]).unwrap();
            found = watch.push(Sample { seconds: d.clock.now(), offset: i * 8192, bytes: 8192, mb_s: 8192. / 1e6 / (d.clock.now() - start) });
        }
        assert!(watch.after().unwrap() > watch.peak);
        let mut measured = Diagnostics { written: 150 * 8192, drop_at: found, drop_confirmed_at: Some(50 * 8192), post_mb_s: watch.after(),
            final_complete: true, probe: Some(Default::default()), read_mb_s: Some(77.), trim: "unsupported".into(), ..Default::default() };
        watch.update(&mut measured);
        measured.tally.ok = 42;
        for status in [Status::Ok, Status::Unreliable, Status::Cancelled] {
            let mut r = measured.clone();
            r.status = status;
            finish_verdict(&mut r);
            assert_eq!(r.status, status);
            assert_eq!(r.read_mb_s, Some(77.));
            assert_eq!(r.tally.ok, 42);
            assert!(r.probe.is_some());
            assert!(r.drop_at.is_none() && r.drop_confirmed_at.is_none() && r.cache_bytes.is_none() && r.post_mb_s.is_none());
            assert_eq!(r.peak_mb_s, watch.burst);
            assert!(!r.warnings.iter().any(|s| s.contains("Cache figures")));
            let report = report(&DiskInfo::default(), r);
            assert!(report.text().contains("no drop in the first"));
            assert!(report.html().contains("Sequential writes sustained"));
            assert!(!report.text().contains("before the drop"));
            let json = serde_json::to_value(&report).unwrap();
            assert!(json["diagnostics"]["drop_at"].is_null());
            assert!(json["diagnostics"]["post_mb_s"].is_null());
        }
        for (ratio, keep) in [(0.899, true), (0.9, false), (1.1, false)] {
            let mut r = measured.clone();
            r.post_mb_s = Some(r.peak_mb_s * ratio);
            r.check_drop();
            assert_eq!(r.drop_at.is_some(), keep);
        }
    }

    #[test]
    fn short_boundary_dips_do_not_move_a_later_cliff() {
        let mut watch = DropWatch::default();
        let mut off = 0;
        let mut boundary = 0;
        let mut found = None;
        for i in 0..200 {
            let speed = if i >= 100 { 40. } else if i == 20 { 70. } else { 100. };
            if i == 100 { boundary = off; }
            let bytes = (speed * 1e6 * 0.2) as u64;
            found = watch.push(Sample { seconds: (i + 1) as f64 * 0.2, offset: off, bytes, mb_s: speed });
            off += bytes;
            if found.is_some() { break; }
        }
        assert_eq!(found, Some(boundary));
        assert!((99.0..100.).contains(&watch.peak));
    }

    #[test]
    fn isolated_pause_is_not_a_plateau_and_slow_warning_uses_sustained_speed() {
        let mut watch = DropWatch::default();
        let mut seconds = 0.;
        let mut off = 0;
        for i in 0..150 {
            let elapsed = if i == 50 { 20. } else { 0.2 };
            seconds += elapsed;
            assert!(watch.push(Sample { seconds, offset: off, bytes: MIB, mb_s: MIB as f64 / 1e6 / elapsed }).is_none());
            off += MIB;
        }
        for (peak, sustained, warn) in [(1.8, 29., false), (100., 3., true)] {
            let mut d = Diagnostics { peak_mb_s: peak, sustained_mb_s: Some(sustained), ..Default::default() };
            finish_verdict(&mut d);
            assert_eq!(d.warnings.iter().any(|s| s.contains("very slow")), warn);
        }
    }

    #[test]
    fn drop_requires_sustained_slowdown() {
        for (slow, expected_time) in [(70., 24.), (82., 34.)] {
            let mut watch = DropWatch::default();
            let mut found = None;
            let mut off = 0;
            for i in 1..=250 {
                let speed = if i <= 50 { 100. } else { slow };
                let bytes = (speed * 1e6 * 0.2) as u64;
                let sample = Sample { seconds: i as f64 * 0.2, offset: off, bytes, mb_s: speed };
                off += bytes;
                if watch.push(sample).is_some() {
                    found = Some(i as f64 * 0.2); break;
                }
            }
            assert!((watch.peak - 100.).abs() < 1e-6);
            assert!(found.unwrap() >= expected_time && found.unwrap() <= expected_time + 3., "{found:?}");
        }
        let mut watch = DropWatch::default();
        for i in 1..=200 { assert!(watch.push(Sample { seconds: i as f64 * 0.2, offset: i, bytes: 1, mb_s: if (65..70).contains(&i) { 10. } else { 100. } }).is_none()); }
    }

    #[test]
    fn sawtooth_writes_separate_burst_speed_from_sustained_speed() {
        struct Sawtooth { disk: crate::dev::MemDev, clock: Clock, duration: f64 }
        impl Dev for Sawtooth {
            fn len(&self) -> u64 { self.disk.len() }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
            fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> { self.clock.sleep(self.duration); self.disk.write_at(off, buf) }
            fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> { self.disk.read_at(off, buf) }
        }
        let trace = [(3.6,1.), (5.,3.), (5.3,25.), (5.6,25.), (6.,24.), (6.3,24.), (6.7,24.), (7.,24.),
            (7.4,25.), (7.7,23.), (9.3,6.), (9.6,24.), (10.,24.), (10.3,24.), (10.6,25.), (11.,24.),
            (11.4,24.), (11.7,23.), (12.1,24.), (13.6,5.), (13.9,24.), (14.3,24.), (14.6,25.), (15.,24.),
            (15.3,24.), (15.7,23.), (16.,24.)];
        let mut intervals = vec![];
        let mut previous = 0.;
        for (seconds, speed) in trace { intervals.push((seconds - previous, speed)); previous = seconds; }
        // Repeat a full steady cycle long enough to exercise both hold thresholds.
        let cycle = intervals[11..20].to_vec();
        for _ in 0..20 { intervals.extend_from_slice(&cycle); }
        let mut d = Sawtooth { disk: crate::dev::MemDev::new(4 * MIB, 4 * MIB, SimMode::Genuine), clock: Clock::simulated(), duration: 0. };
        let mut watch = DropWatch::default();
        let mut off = 0;
        for (duration, speed) in intervals {
            d.duration = duration;
            // Scale bytes and rates together; preserve the observed pauses exactly.
            let bytes = (duration * speed * 1000.).round() as usize;
            d.write_at(off, &vec![0; bytes]).unwrap();
            assert!(watch.push(Sample { seconds: d.clock.now(), offset: off, bytes: bytes as u64, mb_s: bytes as f64 / 1e6 / duration }).is_none());
            off += bytes as u64;
        }
        assert!((0.023..0.026).contains(&watch.peak), "{}", watch.peak);
        assert!((0.017..0.022).contains(&watch.sustained));
        assert!(watch.sawtooth);
        let r = Diagnostics { written: off, peak_mb_s: watch.peak, drop_at: Some(MIB), ..Default::default() };
        assert!(r.summary_lines().iter().any(|s| s.contains("MB/s before the drop")));
        assert_eq!(serde_json::to_value(&r).unwrap()["peak_mb_s"], watch.peak);
    }

    #[test]
    fn burst_reference_still_detects_early_real_cliffs() {
        for (fast, slow, cache) in [(25_000., 12_000., 100_000), (100_000., 16_000., 110_000), (80_000., 26_000., 690_000)] {
            let mut d = CacheDev::new(4 * MIB, 4 * MIB, SimMode::Genuine, cache, fast, slow);
            let mut watch = DropWatch::default();
            let mut found = None;
            for off in (0..4 * MIB).step_by(4096) {
                let start = d.clock.now();
                d.write_at(off, &[0;4096]).unwrap();
                found = watch.push(Sample { seconds: d.clock.now(), offset: off, bytes: 4096, mb_s: 4096. / 1e6 / (d.clock.now() - start) });
                if found.is_some() { break; }
            }
            assert!(found.is_some(), "{fast} -> {slow}, reference {}", watch.peak);
            assert!(found.unwrap().abs_diff(cache) < 8192);
        }
    }

    #[test]
    fn cache_drive_cliff_and_idle_are_simulated() {
        let mut d = CacheDev::new(MIB, MIB, SimMode::Genuine, 8192, 8192., 1024.);
        let b = vec![1; 8192];
        d.write_at(0, &b).unwrap();
        assert_eq!(d.clock.now(), 1.);
        d.write_at(8192, &b).unwrap();
        assert_eq!(d.clock.now(), 9.);
        d.clock.sleep(3.);
        d.write_at(16384, &b).unwrap();
        assert_eq!(d.clock.now(), 13.);
    }

    #[test]
    fn observed_cache_cliff_triggers_the_detector() {
        let mut d = CacheDev::new(2 * MIB, 2 * MIB, SimMode::Genuine, 409600, 40960., 8192.);
        let mut watch = DropWatch::default();
        let mut detected = None;
        for i in 0..180 {
            let start = d.clock.now();
            d.write_at(i * 4096, &[0; 4096]).unwrap();
            let duration = d.clock.now() - start;
            detected = watch.push(Sample { seconds: d.clock.now(), offset: i * 4096, bytes: 4096, mb_s: 4096. / 1e6 / duration });
            if detected.is_some() { break; }
        }
        assert!(detected.is_some());
        assert!((watch.peak - 0.04096).abs() < 1e-6);
    }

    #[test]
    fn budgets_reserve_final_checks_and_keep_capacity_caps() {
        let a = Limits::new(DriveClass::Nvme, 100 * GB, 4096, Some(1800.));
        assert_eq!(a.phases[3], 1350.);
        assert_eq!(a.end(3, 1700.), 1730.);
        assert_eq!(a.end(6, 1730.), 1800.);
        assert!(a.seq_cap <= 40 * GB && a.write_cap <= 50 * GB);
        let b = Limits::new(DriveClass::Usb, 100 * GB, 4096, Some(30.));
        assert_eq!(b.end(3, 0.), 0.);
        assert_eq!(b.end(6, 0.), 30.);
    }

    #[test]
    fn unusual_wrap_and_protected_prefix_are_caught() {
        for mode in [SimMode::Wrap, SimMode::Ring { prefix: MIB }] {
            let mut d = CacheDev::new(3 * MIB + MIB / 2, 32 * MIB, mode, 64 * MIB, 100e6, 20e6);
            let clock = d.clock.clone();
            let ctx = Ctx::new();
            let mut run = Run { clock, ctx: &ctx, limits: limits(DriveClass::Usb, 32 * MIB), r: Diagnostics::default(), pat: Pattern::Seeded { tag: 9 }, end: 500., phase_start: 0., seq_end: 0, overhead: 0, mixed: (0, 0), recovery: (0, 0), before: None, observe: &mut || Ok(None), replayed: false, reads: Default::default() };
            assert!(run.sequential(&mut d, false).is_err());
            assert_eq!(run.r.status, Status::Fake);
            assert!(run.seq_end <= 5 * MIB);
        }
    }

    #[test]
    fn fake_stops_before_sequential_writes() {
        let mut d = CacheDev::new(8 * MIB, 64 * MIB, SimMode::Wrap, 64 * MIB, 100e6, 20e6);
        let clock = d.clock.clone();
        let r = run_with(&mut d, DriveClass::Usb, false, limits(DriveClass::Usb, 64 * MIB), clock, &Ctx::new());
        assert_eq!(r.status, Status::Fake, "{}", r.reason);
        assert_eq!(r.written, 0);
        assert!(!r.phases.iter().any(|p| p.name == "Sequential write"));
    }

    #[test]
    fn several_writes_use_independent_handles_and_respect_end() {
        let mut d = CacheDev::new(2 * MIB, 2 * MIB, SimMode::Genuine, 2 * MIB, 100e6, 20e6);
        let clock = d.clock.clone();
        let pat = Pattern::Seeded { tag: 19 };
        let (done, r) = engine::write_window(&mut d, pat, 0, MIB + 4096, 64 * 1024, 4, 10., &clock, &Ctx::new());
        r.unwrap();
        assert_eq!(done.iter().map(|c| c.len as u64).sum::<u64>(), MIB + 4096);
        let s = d.state.lock().unwrap();
        assert!(pat.buffer_ok(0, &s.disk.data[..MIB as usize + 4096]));
        assert!(s.disk.data[MIB as usize + 4096..].iter().all(|b| *b == 0));
    }

    #[test]
    fn ssd_writer_has_four_writes_outstanding() {
        #[derive(Clone)]
        struct Gate { disk: CacheDev, entered: std::sync::Arc<(std::sync::Mutex<usize>, std::sync::Condvar)> }
        impl Dev for Gate {
            fn len(&self) -> u64 { self.disk.len() }
            fn fork(&mut self) -> io::Result<Box<dyn Dev>> { Ok(Box::new(self.clone())) }
            fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> { self.disk.read_at(off, buf) }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
            fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
                let (lock, ready) = &*self.entered;
                let mut n = lock.lock().unwrap();
                *n += 1;
                ready.notify_all();
                let (n, _) = ready.wait_timeout_while(n, std::time::Duration::from_millis(250), |n| *n < 4).unwrap();
                if *n < 4 { return Err(io::Error::other("writes did not overlap")); }
                drop(n);
                self.disk.write_at(off, buf)
            }
        }
        let mut dev = Gate { disk: CacheDev::new(MIB, MIB, SimMode::Genuine, MIB, 100e6, 20e6), entered: Default::default() };
        let clock = dev.disk.clock.clone();
        let (done, result) = engine::write_window(&mut dev, Pattern::Seeded { tag: 7 }, 0, 4 * 4096, 4096, 4, 10., &clock, &Ctx::new());
        result.unwrap();
        assert_eq!(done.len(), 4);
    }

    #[test]
    fn blocked_triage_returns_a_problem_before_writing() {
        let mut d = CacheDev::new(MIB, MIB, SimMode::Genuine, MIB, 100e6, 20e6);
        let clock = d.clock.clone();
        clock.sleep(62.);
        let r = run_with(&mut d, DriveClass::Usb, false, limits(DriveClass::Usb, MIB), clock, &Ctx::new());
        assert_eq!(r.status, Status::Unreliable);
        assert_eq!(d.state.lock().unwrap().written, 0);
        assert_eq!(r.phases.len(), 1);
        assert_eq!(r.phases[0].worst_ms, 62000.);
    }

    #[test]
    fn genuine_run_obeys_distinct_byte_cap() {
        let mut d = CacheDev::new(32 * MIB, 32 * MIB, SimMode::Genuine, 32 * MIB, 100e6, 20e6);
        let clock = d.clock.clone();
        let r = run_with(&mut d, DriveClass::Usb, false, limits(DriveClass::Usb, 32 * MIB), clock, &Ctx::new());
        assert_eq!(r.status, Status::Ok, "{}", r.reason);
        assert!(r.written <= 32 * MIB * 4 / 10);
        assert!(r.distinct_upper_bound <= 16 * MIB);
        assert!(r.drop_at.is_none());
    }

    #[test]
    fn simulated_runs_render_and_round_trip_complete_partial_and_failed_reports() {
        for case in 0..5 {
            let fake = case == 1;
            let mut d = CacheDev::new(if fake { 8 * MIB } else { 32 * MIB }, 32 * MIB, if fake { SimMode::Wrap } else { SimMode::Genuine }, 32 * MIB, 100e6, 20e6);
            let mut l = limits(DriveClass::Usb, d.len());
            let ctx = Ctx::new();
            if case == 2 { ctx.cancel(); }
            if case == 3 { l.total = 0.1; }
            let clock = d.clock.clone();
            let result = run_with(&mut d, DriveClass::Usb, case == 4, l, clock, &ctx);
            let disk = DiskInfo { size: d.len(), vendor: "Simulated".into(), product: "<drive & test>".into(), ..Default::default() };
            let report = report(&disk, result);
            assert_eq!(report.exit_code(), match case { 1 | 4 => 1, 2 => 130, _ => 0 });
            let json = serde_json::to_string(&report).unwrap();
            let saved: crate::report::Report = serde_json::from_str(&json).unwrap();
            assert!(saved.diagnostics.is_some());
            assert_eq!(saved.exit_code(), report.exit_code());
            assert!(saved.text().contains("Quick diagnostics:"));
            let html = saved.html();
            assert!(html.contains("Diagnostic phases"));
            assert!(html.contains("&lt;drive &amp; test&gt;"));
            assert!(saved.evidence_html().contains("full write-and-verify test"));
            assert!(saved.notes.iter().any(|n| n.contains("every byte")));
            let r = saved.diagnostics.unwrap();
            assert!(r.distinct_upper_bound <= 16 * MIB);
            if fake { assert_eq!(r.written, 0); assert!(r.mixed.is_none() && r.recovery.is_empty()); }
            if case == 4 { assert!(r.triage_failing && r.mixed.is_none()); assert!(!r.phases.iter().any(|p| p.name == "TRIM and idle")); }
            if case == 3 { assert!(!r.final_complete); }
        }
    }

    #[test]
    fn cache_cliff_run_reports_the_drop_and_keeps_recovery_bounded() {
        let size = 128 * MIB;
        let mut d = CacheDev::new(size, size, SimMode::Genuine, 4 * MIB, 1e6, 1e5);
        let clock = d.clock.clone();
        let mut l = limits(DriveClass::Usb, size);
        l.check_every = 8 * MIB;
        let r = run_with(&mut d, DriveClass::Usb, false, l, clock, &Ctx::new());
        assert_eq!(r.status, Status::Ok, "{} {:?}", r.reason, r.warnings);
        assert!(r.drop_at.is_some(), "peak {} written {}", r.peak_mb_s, r.written);
        assert!(r.post_mb_s.unwrap() < r.peak_mb_s * 0.75);
        assert!(!r.recovery.is_empty());
        assert!(r.mixed.as_ref().is_some_and(|m| m.operations > 0));
        assert!(r.mixed_verified);
        assert!(r.distinct_upper_bound <= size / 2);
        let report = report(&DiskInfo { size, ..Default::default() }, r);
        assert!(report.html().contains("stroke-dasharray"));
        assert!(report.text().contains("slowdown near"));
    }

    #[test]
    fn first_io_error_keeps_a_partial_report_and_stops_writing() {
        struct Broken { writes: usize, kind: io::ErrorKind }
        impl Dev for Broken {
            fn len(&self) -> u64 { 32 * MIB }
            fn write_at(&mut self, _: u64, _: &[u8]) -> io::Result<()> { self.writes += 1; Err(io::Error::new(self.kind, "simulated write failure")) }
            fn read_at(&mut self, _: u64, _: &mut [u8]) -> io::Result<usize> { panic!("read after failed write") }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
        }
        for kind in [io::ErrorKind::Other, io::ErrorKind::TimedOut, io::ErrorKind::Interrupted] {
            let mut d = Broken { writes: 0, kind };
            let r = run_with(&mut d, DriveClass::Usb, false, limits(DriveClass::Usb, 32 * MIB), Clock::simulated(), &Ctx::new());
            assert_eq!(d.writes, 1);
            assert_eq!(r.status, Status::Unreliable);
            assert!(!r.final_complete && r.mixed.is_none());
            let r = report(&DiskInfo::default(), r);
            assert_eq!(r.exit_code(), 1);
            assert!(r.text().contains("simulated write failure"));
            assert!(r.html().contains("Diagnostic phases"));
        }
    }

    #[test]
    fn diagnostic_report_qualifies_cache_and_link_claims_and_reads_old_json() {
        let disk = DiskInfo { usb: Some(crate::platform::UsbInfo { slow_link: true, link: "USB 2".into(), ..Default::default() }), ..Default::default() };
        let d = Diagnostics { trim: "unsupported".into(), written: 2 * MIB, peak_mb_s: 5., drop_at: Some(MIB), heat_suspected: true, ..Default::default() };
        let report = report(&disk, d);
        assert!(!report.warnings.iter().any(|w| w.contains("link limits")));
        assert!(report.text().contains("not a cache-size measurement"));
        assert!(report.text().contains("may be understated"));
        let mut json = serde_json::to_value(&report).unwrap();
        json.as_object_mut().unwrap().remove("diagnostics");
        let old: crate::report::Report = serde_json::from_value(json).unwrap();
        assert!(old.diagnostics.is_none());
    }

    #[test]
    fn probe_deadline_covers_speed_calibration() {
        let mut d = CacheDev::new(8 * MIB, 128 * MIB, SimMode::Drop, 0, 4096., 4096.);
        let clock = d.clock.clone();
        let ctx = Ctx::new();
        let mut l = limits(DriveClass::Usb, 128 * MIB);
        l.phases[2] = 0.5;
        let mut run = Run { clock, ctx: &ctx, limits: l, r: Diagnostics::default(), pat: Pattern::Seeded { tag: 9 }, end: 500., phase_start: 0., seq_end: 0, overhead: 0, mixed: (0,0), recovery: (0,0), before: None, observe: &mut || Ok(None), replayed: false, reads: Default::default() };
        run.probe(&mut d).unwrap();
        let p = run.r.probe.as_ref().unwrap();
        assert!(p.partial);
        assert_eq!(p.point_tags.iter().filter(|t| t.is_some()).count(), 1);
        assert_eq!(p.status.bytes().filter(|&s| s == b'o').count(), 0);
        assert!(run.clock.now() <= 1.1);
        assert_eq!(run.r.written, 0);
    }

    #[test]
    fn unsupported_trim_never_idles() {
        let mut d = MemDev::new(8 * MIB, 8 * MIB, SimMode::Genuine);
        let clock = Clock::simulated();
        let ctx = Ctx::new();
        let mut run = Run { clock, ctx: &ctx, limits: limits(DriveClass::Usb, 8 * MIB), r: Diagnostics::default(), pat: Pattern::Seeded { tag: 9 }, end: 500., phase_start: 0., seq_end: 0, overhead: 0, mixed: (0, 0), recovery: (0, 0), before: None, observe: &mut || Ok(None), replayed: false, reads: Default::default() };
        run.trim(&mut d).unwrap();
        run.finish_phase();
        assert!(run.r.trim.starts_with("unsupported"));
        assert_eq!(run.r.phases[0].seconds, 0.);
    }
}

#[cfg(test)]
mod later_tests {
    use super::*;
    use crate::dev::{CacheDev, SimMode};

    fn state<'a>(d: &CacheDev, ctx: &'a Ctx, observe: &'a mut (dyn FnMut() -> io::Result<Option<Smart>> + Send)) -> Run<'a> {
        Run { clock: d.clock.clone(), ctx, limits: Limits { block: 4096, check_every: MIB, mark_every: 128 * 1024, ..Limits::new(DriveClass::Usb, d.len(), 4096, None) },
            r: Diagnostics::default(), pat: Pattern::Seeded { tag: 9 }, end: 500., phase_start: 0., seq_end: 0, overhead: 0, mixed: (0,0), recovery: (0,0), before: None, observe, replayed: false, reads: Default::default() }
    }
    fn fill(d: &mut CacheDev, pat: Pattern, start: u64, end: u64) {
        let mut buf = AlignedBuf::new((end - start) as usize);
        pat.fill(start, &mut buf);
        d.write_at(start, &buf).unwrap();
    }

    #[derive(Clone)]
    struct Pausing {
        disk: CacheDev,
        read_pause: std::sync::Arc<std::sync::Mutex<f64>>,
        write_pause: std::sync::Arc<std::sync::Mutex<f64>>,
        random_delay: f64,
        wrap_after: Option<u64>,
    }
    impl Pausing {
        fn new(size: u64) -> Self {
            Self { disk: CacheDev::new(size, size, SimMode::Genuine, size, 100e6, 20e6), read_pause: Default::default(), write_pause: Default::default(), random_delay: 0., wrap_after: None }
        }
        fn pause(&self, value: &std::sync::Mutex<f64>) {
            let seconds = std::mem::take(&mut *value.lock().unwrap());
            if seconds > 0. { self.disk.clock.sleep(seconds); }
        }
    }
    impl Dev for Pausing {
        fn len(&self) -> u64 { self.disk.len() }
        fn fork(&mut self) -> io::Result<Box<dyn Dev>> { Ok(Box::new(self.clone())) }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
        fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
            if let Some(count) = self.wrap_after {
                let mut s = self.disk.state.lock().unwrap();
                if s.written / 4096 == count { s.disk.data.truncate(8 * MIB as usize); s.disk.mode = SimMode::Wrap; }
            }
            self.pause(&self.write_pause);
            if buf.len() == 4096 && self.random_delay > 0. { self.disk.clock.sleep(self.random_delay); }
            self.disk.write_at(off, buf)
        }
        fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
            self.pause(&self.read_pause);
            self.disk.read_at(off, buf)
        }
    }

    struct Discarding { disk: CacheDev, limit: Option<u64>, delay: f64, error: Option<io::ErrorKind>, calls: Vec<(u64, u64)> }
    impl Dev for Discarding {
        fn len(&self) -> u64 { self.disk.len() }
        fn sector_size(&self) -> usize { 512 }
        fn discard_max_bytes(&self) -> Option<u64> { self.limit }
        fn discard(&mut self, off: u64, len: u64) -> io::Result<()> {
            self.calls.push((off, len));
            self.disk.clock.sleep(self.delay);
            if let Some(kind) = self.error { return Err(io::Error::new(kind, "discard fixture")); }
            self.disk.discard(off, len)
        }
        fn flush(&mut self) -> io::Result<()> { self.disk.clock.sleep(2.); Ok(()) }
        fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> { self.disk.write_at(off, buf) }
        fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> { self.disk.read_at(off, buf) }
    }

    #[test]
    fn discard_chunks_obey_limits_budgets_and_keep_slow_successes() {
        for (limit, delay, claimed, expected) in [
            (Some(MIB + 1), 15., 32 * MIB, vec![(0, MIB), (MIB, MIB), (2 * MIB, MIB)]),
            (None, 74.8, 20 * crate::dev::GIB, vec![(0, 8 * crate::dev::GIB)]),
            (None, 1., 20 * crate::dev::GIB, vec![(0, 8 * crate::dev::GIB), (8 * crate::dev::GIB, 8 * crate::dev::GIB), (16 * crate::dev::GIB, 4 * crate::dev::GIB), (0, 4 * MIB)]),
        ] {
            let disk = CacheDev::new(32 * MIB, claimed, SimMode::Drop, 32 * MIB, 100e6, 20e6);
            let ctx = Ctx::new();
            let mut observe = || Ok(None);
            let mut run = state(&disk, &ctx, &mut observe);
            let mut d = Discarding { disk, limit, delay, error: None, calls: vec![] };
            run.trim(&mut d).unwrap();
            assert_eq!(d.calls, expected);
            assert_eq!(run.r.status.exit_code(), 0);
            assert!(run.r.stalls.iter().all(|s| s.offset.is_none()));
            if delay >= 15. {
                let trimmed = expected.iter().map(|(_, n)| n).sum::<u64>();
                assert_eq!(run.r.trim, format!("partial: {trimmed} of {claimed} bytes trimmed"));
                assert_eq!(ctx.status().done, trimmed);
                assert_eq!(ctx.status().total, claimed);
                // A new phase can proceed after a long discard returned successfully.
                run.phase(3, "Sequential write");
                run.write(&mut d, 0, &[0; 4096]).unwrap();
            } else { assert!(run.r.trim.starts_with("effective")); }
        }
        for kind in [io::ErrorKind::Unsupported, io::ErrorKind::InvalidInput, io::ErrorKind::Other] {
            let disk = CacheDev::new(MIB, MIB, SimMode::Genuine, MIB, 100e6, 20e6);
            let ctx = Ctx::new();
            let mut observe = || Ok(None);
            let mut run = state(&disk, &ctx, &mut observe);
            let mut d = Discarding { disk, limit: None, delay: 75., error: Some(kind), calls: vec![] };
            assert_eq!(run.trim(&mut d).is_ok(), kind == io::ErrorKind::Unsupported);
            assert_eq!(d.calls.len(), 1);
        }
    }

    #[test]
    fn pauses_below_sixty_seconds_warn_but_do_not_fail() {
        for write in [false, true] {
            for delay in [10., 30., 59., 60.] {
                let mut d = Pausing::new(32 * MIB);
                *if write { &d.write_pause } else { &d.read_pause }.lock().unwrap() = delay;
                let clock = d.disk.clock.clone();
                let limits = Limits { block: 64 * 1024, ..Limits::new(DriveClass::Usb, d.len(), 4096, None) };
                let r = run_with(&mut d, DriveClass::Usb, false, limits, clock, &Ctx::new());
                assert_eq!(r.status, if delay < 60. { Status::Ok } else { Status::Unreliable }, "{write} {delay}: {}", r.reason);
                assert!(r.stalls.iter().any(|s| s.offset.is_some() && s.seconds >= delay));
                assert!(r.warnings.iter().any(|w| w.contains("make copies hang")));
                if delay >= 60. { assert!(r.reason.contains("No I/O completed")); }
            }
        }
    }

    #[test]
    fn queued_and_random_io_keep_going_after_ten_second_pauses() {
        for write in [false, true] {
            for delay in [12., 60.] {
                let mut d = Pausing::new(MIB);
                let pat = Pattern::Seeded { tag: 12 };
                fill(&mut d.disk, pat, 0, 8192);
                *if write { &d.write_pause } else { &d.read_pause }.lock().unwrap() = delay;
                let clock = d.disk.clock.clone();
                let ctx = Ctx::new();
                let (done, result) = if write { engine::write_window(&mut d, pat, 0, 8192, 4096, 1, clock.now() + 100., &clock, &ctx) }
                    else { engine::read_window(&mut d, pat, 0, 8192, 4096, 1, clock.now() + 100., &clock, &ctx) };
                result.unwrap();
                assert_eq!(done.len(), if delay < 60. { 2 } else { 1 });
                *if write { &d.write_pause } else { &d.read_pause }.lock().unwrap() = delay;
                let r = crate::perf::random_io(vec![d.fork().unwrap()], crate::perf::RandomOpts { start: 0, span: 8192, write, pattern: Some(pat), deadline: clock.now() + 15. }, &clock, &ctx);
                assert_eq!(r.error.is_some(), delay >= 60.);
                assert!(!r.stalls.is_empty());
            }
        }
    }

    #[test]
    fn nonpositional_stalls_never_implicate_lba_zero() {
        let disk = CacheDev::new(MIB, MIB, SimMode::Genuine, MIB, 100e6, 20e6);
        let clock = disk.clock.clone();
        let ctx = Ctx::new();
        let mut observe = || { clock.sleep(3.); Ok(None) };
        let mut run = state(&disk, &ctx, &mut observe);
        let mut d = Discarding { disk, limit: None, delay: 4., error: None, calls: vec![] };
        run.phase(0, "Flush"); run.flush(&mut d).unwrap();
        run.phase(0, "Identity"); run.snapshot().unwrap();
        run.phase(1, "TRIM"); run.discard(&mut d, 0, 4096).unwrap();
        assert_eq!(run.r.stalls.len(), 3);
        assert!(run.r.stalls.iter().all(|s| s.offset.is_none()));
        assert!(!interpret_stalls(&run.r.stalls).contains("Bad regions"));
        assert!(interpret_stalls(&run.r.stalls).contains("Throttling"));
        let mut r = report(&DiskInfo::default(), run.r);
        assert!(r.text().contains("no disk position"));
        assert!(r.html().contains("no disk position"));
        let json = serde_json::to_value(&r).unwrap();
        assert!(json["diagnostics"]["stalls"][0]["offset"].is_null());
        let d = r.diagnostics.as_mut().unwrap();
        for s in &mut d.stalls { s.offset = Some(0); }
        assert!(interpret_stalls(&d.stalls).contains("Bad regions near 0 B"));
    }

    #[test]
    fn slow_scattered_writes_thin_the_probe_and_preserve_partial_tags() {
        for budget in [60., 35., 3.] {
            let mut d = Pausing::new(64 * MIB);
            d.random_delay = 1.;
            let clock = d.disk.clock.clone();
            let (step, points) = probe::grid(d.len(), 4096, 1024);
            let p = probe::run_limited(&mut d, points, step, budget, &clock, &Ctx::new()).unwrap();
            assert_eq!(p.partial, budget < 60.);
            assert!(p.seconds < budget + 1.01);
            assert!(p.step.is_power_of_two());
            assert!(p.positions[..p.positions.len() - 1].iter().all(|p0| p0.is_multiple_of(p.step)));
            if budget >= 35. { assert!((16..=32).contains(&p.positions.len())); }
            if budget == 35. { assert!(p.point_tags.iter().flatten().collect::<std::collections::BTreeSet<_>>().len() > 1); }
            let mut buf = AlignedBuf::new(4096);
            for (&off, &tag) in p.positions.iter().zip(&p.point_tags) {
                if let Some(tag) = tag {
                    d.read_at(off, &mut buf).unwrap();
                    assert!(Pattern::Seeded { tag }.buffer_ok(off, &buf));
                }
            }
            if budget < 60. {
                assert!(p.summary_lines()[0].contains("Capacity probe partial:"));
                assert!(!p.summary_lines().join(" ").contains("2 passes"));
                assert!(p.map_html().contains("p-unknown"));
            }
        }
    }

    #[test]
    fn unfinished_or_coarse_fake_probes_do_not_overestimate_usable_capacity() {
        for (real, budget, expected, switch) in [(8 * MIB, 25.011, 0, None), (8 * MIB, 60., 8 * MIB, None), (MIB, 60., 0, None), (64 * MIB, 42.0342, 0, Some(25))] {
            let mut d = Pausing::new(real);
            d.disk = CacheDev::new(real, 64 * MIB, SimMode::Wrap, 64 * MIB, 100e6, 20e6);
            d.random_delay = 1.;
            d.wrap_after = switch;
            let clock = d.disk.clock.clone();
            let (step, points) = probe::grid(d.len(), 4096, 1024);
            let p = probe::run_limited(&mut d, points, step, budget, &clock, &Ctx::new()).unwrap();
            assert_eq!(p.finding, Finding::Fake);
            assert_eq!(p.usable, expected);
            if expected == 0 {
                assert!(p.module.is_none());
                assert!(p.summary_lines().iter().any(|s| s.contains("estimate was not established")));
            }
        }
    }

    #[test]
    fn partial_probe_can_finish_ok_and_fake_probe_still_stops() {
        for fake in [false, true] {
            let mut d = Pausing::new(64 * MIB);
            if fake { d.disk = CacheDev::new(8 * MIB, 64 * MIB, SimMode::Wrap, 64 * MIB, 100e6, 20e6); }
            d.random_delay = 1.;
            let clock = d.disk.clock.clone();
            let mut limits = Limits::new(DriveClass::Usb, d.len(), 4096, None);
            limits.block = 64 * 1024;
            limits.phases[2] = if fake { 60. } else { 35. };
            let r = run_with(&mut d, DriveClass::Usb, false, limits, clock, &Ctx::new());
            assert_eq!(r.status, if fake { Status::Fake } else { Status::Ok }, "{}", r.reason);
            if fake { assert_eq!(r.written, 0); }
            else {
                assert!(r.final_complete && r.probe.as_ref().unwrap().partial);
                assert!(r.warnings.iter().any(|w| w.contains("Capacity probe partial:")));
                let r = report(&DiskInfo::default(), r);
                assert!(r.text().contains("Capacity probe partial:"));
                assert!(r.html().contains("Capacity probe partial:"));
                let saved: crate::report::Report = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
                assert_eq!(saved.exit_code(), 0);
                assert_eq!(saved.probe.unwrap().point_tags, r.probe.unwrap().point_tags);
            }
        }
    }

    #[test]
    fn cache_caveat_requires_a_reported_drop_or_recovery() {
        let mut d = Pausing::new(32 * MIB); // No discard support.
        let clock = d.disk.clock.clone();
        let limits = Limits { block: 64 * 1024, ..Limits::new(DriveClass::Usb, d.len(), 4096, None) };
        let mut r = run_with(&mut d, DriveClass::Usb, false, limits, clock, &Ctx::new());
        assert_eq!(r.status, Status::Ok);
        assert!(r.drop_at.is_none() && r.recovery.is_empty());
        assert!(!r.summary_lines().iter().chain(&r.warnings).any(|l| l.contains("may be understated")));
        r.drop_at = Some(MIB);
        assert!(r.summary_lines().iter().any(|l| l.contains("may be understated")));
        r.drop_at = None;
        r.recovery.push(Recovery { fast_bytes: MIB, ..Default::default() });
        assert!(r.summary_lines().iter().any(|l| l.contains("may be understated")));
        r.trim = "effective in the sample".into();
        assert!(!r.summary_lines().iter().any(|l| l.contains("may be understated")));
    }

    #[test]
    fn mixed_reads_vary_offsets_and_check_the_pattern_there() {
        let mut d = CacheDev::new(8 * MIB, 8 * MIB, SimMode::Genuine, 8 * MIB, 100e6, 20e6);
        let pat = Pattern::Seeded { tag: 19 };
        fill(&mut d, pat, 0, 4 * MIB);
        for len in [MIB, 4 * MIB] { // Before a drop, or the entire sequential region.
            let (mut rng, mut previous) = (12345, None);
            let mut offsets = std::collections::BTreeSet::new();
            let mut buf = AlignedBuf::new(4096);
            for _ in 0..128 {
                let last = previous;
                let (off, result) = mixed_read(&mut d, pat, &mut rng, &mut previous, len, &mut buf);
                result.unwrap();
                assert!(off + 4096 <= len && off.is_multiple_of(4096));
                assert_ne!(Some(off), last);
                offsets.insert(off);
            }
            assert!(offsets.len() > 90);
            assert!(*offsets.last().unwrap() > len * 3 / 4);
        }
        // LBA 0 still reads perfectly; all other sectors have changed.
        d.state.lock().unwrap().disk.data[4096..4 * MIB as usize].fill(0);
        let (_, result) = mixed_read(&mut d, pat, &mut 12345, &mut Some(0), 4 * MIB, &mut AlignedBuf::new(4096));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn sequential_reader_uses_class_depth_and_stops_on_corruption() {
        #[derive(Clone)]
        struct Gate {
            disk: CacheDev,
            qd: usize,
            entered: std::sync::Arc<(std::sync::Mutex<usize>, std::sync::Condvar)>,
            forks: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }
        impl Dev for Gate {
            fn len(&self) -> u64 { self.disk.len() }
            fn fork(&mut self) -> io::Result<Box<dyn Dev>> {
                self.forks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Box::new(self.clone()))
            }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
            fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> { self.disk.write_at(off, buf) }
            fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
                let (lock, ready) = &*self.entered;
                let mut n = lock.lock().unwrap();
                *n += 1;
                ready.notify_all();
                let (n, _) = ready.wait_timeout_while(n, std::time::Duration::from_millis(250), |n| *n < self.qd).unwrap();
                if *n < self.qd { return Err(io::Error::other("reads did not overlap")); }
                drop(n);
                self.disk.read_at(off, buf)
            }
        }
        for hint in [None, Some(false), Some(true)] {
            let disk = DiskInfo { bus: "USB".into(), ssd_hint: hint, ..Default::default() };
            let class = classify(&disk, None); // No SMART from the bridge.
            let qd = if hint == Some(true) { 4 } else { 1 };
            assert_eq!(class.qd(), qd);
            assert_eq!(class.budgets()[4], if qd == 4 { 120. } else { 75. });
            let mut d = CacheDev::new(MIB, MIB, SimMode::Genuine, MIB, 100e6, 20e6);
            let ctx = Ctx::new();
            let mut observe = || Ok(None);
            let mut run = state(&d, &ctx, &mut observe);
            run.r.class = class;
            fill(&mut d, run.pat, 0, MIB);
            let mut gate = Gate { disk: d, qd, entered: Default::default(), forks: Default::default() };
            run.phase(6, "Read");
            assert!(run.read_speed(&mut gate, MIB, 0.0001).unwrap().unwrap() > 0.);
            assert_eq!(*gate.entered.0.lock().unwrap(), qd);
            assert_eq!(gate.forks.load(std::sync::atomic::Ordering::SeqCst), if qd == 1 { 0 } else { 4 });
            assert_eq!(run.r.tally.ok, qd as u64 * 8);
            gate.disk.state.lock().unwrap().disk.data[..4096].fill(0);
            assert!(run.read_speed(&mut gate, MIB, 1.).is_err());
            assert_eq!(run.r.status, Status::Defective);
        }
    }

    #[test]
    fn sequential_reader_keeps_handle_errors_and_cancellation() {
        let d = CacheDev::new(MIB, MIB, SimMode::Genuine, MIB, 100e6, 20e6);
        let ctx = Ctx::new();
        let mut observe = || Ok(None);
        let mut run = state(&d, &ctx, &mut observe);
        run.r.class = DriveClass::Sata;
        run.phase(6, "Read");
        let mut no_fork = crate::dev::MemDev::new(MIB, MIB, SimMode::Genuine);
        assert_eq!(run.read_speed(&mut no_fork, MIB, 1.).unwrap_err().kind(), io::ErrorKind::Unsupported);
        ctx.cancel();
        assert_eq!(run.read_speed(&mut no_fork, MIB, 1.).unwrap_err().kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn sequential_limits_warn_only_when_a_drop_measurement_is_missing() {
        for budget in [1., 100.] {
            let mut d = CacheDev::new(32 * MIB, 32 * MIB, SimMode::Genuine, 32 * MIB, 100e6, 20e6);
            let ctx = Ctx::new();
            let mut observe = || Ok(None);
            let mut run = state(&d, &ctx, &mut observe);
            run.limits.phases[3] = budget;
            let result = run.sequential(&mut d, false);
            run.soft(result).unwrap();
            assert!(run.r.written > 0 && run.r.drop_at.is_none());
            assert!(run.r.warnings.is_empty(), "{:?}", run.r.warnings);
            assert!(run.r.summary_lines().iter().any(|s| s.contains("no drop in the first")));
        }
        // Find the confirmation deadline with the same simulated cliff, then stop there.
        let mut confirmation = 0;
        let mut deadline = 0.;
        for limit in ["none", "capacity", "time"] {
            let mut d = CacheDev::new(32 * MIB, 32 * MIB, SimMode::Genuine, 6 * MIB, 1e6, 2e5);
            let ctx = Ctx::new();
            let snapshot_time = std::sync::Mutex::new(0.);
            let clock = d.clock.clone();
            let mut observe = || { *snapshot_time.lock().unwrap() = clock.now(); Ok(None) };
            let mut run = state(&d, &ctx, &mut observe);
            run.limits.check_every = GB;
            if limit == "capacity" { run.limits.seq_cap = confirmation; }
            if limit == "time" { run.limits.phases[3] = deadline - 0.1; }
            let result = run.sequential(&mut d, false);
            run.soft(result).unwrap();
            assert!(run.r.drop_at.is_some());
            if limit != "none" {
                assert!(run.r.post_mb_s.is_none());
                assert!(run.r.warnings.iter().any(|s| s.contains("before the speed after it could be measured")));
                if limit == "time" { assert!(run.r.warnings[0].contains("budget ended")); }
            } else { confirmation = run.r.drop_confirmed_at.unwrap(); deadline = *snapshot_time.lock().unwrap(); }
        }
    }

    #[test]
    fn baseline_precedes_an_early_cache_drop_and_covers_small_drives() {
        struct Logged { disk: CacheDev, reads: Vec<(u64, u64)> }
        impl Dev for Logged {
            fn len(&self) -> u64 { self.disk.len() }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
            fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> { self.disk.write_at(off, buf) }
            fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
                self.reads.push((off, self.disk.state.lock().unwrap().written));
                self.disk.read_at(off, buf)
            }
        }
        for slow in [false, true] {
            let d = CacheDev::new(32 * MIB, 32 * MIB, SimMode::Genuine, if slow { 6 * MIB } else { 32 * MIB }, if slow { 1e6 } else { 100e6 }, 2e5);
            let ctx = Ctx::new();
            let mut observe = || Ok(None);
            let mut run = state(&d, &ctx, &mut observe);
            run.limits = Limits::new(DriveClass::Usb, d.len(), 4096, None); // Real 1 GB wrap cadence.
            run.limits.block = 64 * 1024;
            let mut d = Logged { disk: d, reads: vec![] };
            run.sequential(&mut d, false).unwrap();
            assert!(run.seq_end < GB);
            assert!(run.r.baseline_read_mb_s.unwrap() > 0.);
            if slow {
                assert!(run.r.drop_at.is_some(), "{:?}", run.r.samples);
                let first_baseline = d.reads.iter().find(|(off, _)| *off == 64 * 1024).unwrap().1;
                assert_eq!(first_baseline, 4 * MIB);
                assert!(first_baseline < run.r.drop_at.unwrap());
                assert!(run.r.drop_read_mb_s.unwrap() > 0.);
                assert!(!run.r.heat_suspected);
            } else { assert!(run.r.drop_at.is_none()); }
        }
    }

    #[test]
    fn ladder_uses_full_recovery_speed_and_honest_bounds() {
        assert_eq!(next_idle(3., 900, 1000, 90., 100., false), None);
        assert_eq!(next_idle(3., 900, 1000, 89., 100., false), Some(15.));
        assert_eq!(next_idle(3., 100, 1000, 100., 100., false), Some(60.));
        assert_eq!(next_idle(3., 100, 1000, 100., 100., true), Some(15.));
        assert_eq!(next_idle(15., 500, 1000, 100., 100., false), Some(30.));
        assert_eq!(next_idle(30., 500, 1000, 100., 100., false), None);
        assert_eq!(next_idle(60., 0, 1000, 0., 100., false), None);
    }

    #[test]
    fn recovery_ladder_runs_against_the_cache_drive() {
        let mut d = CacheDev::new(2 * MIB, 2 * MIB, SimMode::Genuine, 65536, 65536., 8192.);
        let ctx = Ctx::new();
        let mut observe = || Ok(None);
        let mut run = state(&d, &ctx, &mut observe);
        fill(&mut d, run.pat, 0, 256 * 1024);
        d.state.lock().unwrap().available = 0.;
        run.seq_end = 256 * 1024;
        run.r.drop_at = Some(65536);
        run.r.peak_mb_s = 65536. / 1e6;
        run.recover(&mut d).unwrap();
        assert_eq!(run.r.recovery.len(), 1);
        assert_eq!(run.r.recovery[0].idle_seconds, 3.);
        assert!(run.r.recovery[0].full, "{:?}", run.r.recovery);
        assert!(run.recovery.1 <= run.limits.write_cap);
    }

    #[test]
    fn mixed_writes_read_back_and_random_io_stay_within_the_region() {
        let mut d = CacheDev::new(8 * MIB, 8 * MIB, SimMode::Genuine, 8 * MIB, 1e6, 5e5);
        let ctx = Ctx::new();
        let mut observe = || Ok(None);
        let mut run = state(&d, &ctx, &mut observe);
        fill(&mut d, run.pat, 0, MIB);
        run.seq_end = MIB;
        run.limits.write_cap = 3 * MIB;
        run.limits.phases[4] = 20.;
        run.small(&mut d).unwrap();
        assert!(run.r.mixed.as_ref().unwrap().operations > 16);
        assert!(run.r.mixed_verified);
        assert!(run.r.iops.rows.iter().any(|r| r.kind == "read" && r.qd == 1));
        assert!(run.r.iops.rows.iter().any(|r| r.kind == "write" && r.qd == 1));
        assert_eq!(run.mixed, (MIB, 3 * MIB));
        assert!(d.state.lock().unwrap().disk.data[3 * MIB as usize..].iter().all(|b| *b == 0));
    }

    #[test]
    fn thermal_recovery_only_reads() {
        let mut d = CacheDev::new(8 * MIB, 8 * MIB, SimMode::Genuine, 8 * MIB, 100e6, 20e6);
        let ctx = Ctx::new();
        let mut observe = || Ok(None);
        let mut run = state(&d, &ctx, &mut observe);
        fill(&mut d, run.pat, 0, MIB);
        run.seq_end = MIB;
        run.r.drop_at = Some(MIB / 2);
        run.r.heat_suspected = true;
        run.r.baseline_read_mb_s = Some((4 * MIB) as f64 / 1e6);
        let before = d.state.lock().unwrap().written;
        run.recover(&mut d).unwrap();
        assert!(run.r.thermal_recovery);
        assert_eq!(d.state.lock().unwrap().written, before);
        assert!(run.r.recovery.is_empty());
    }

    #[test]
    fn identity_change_after_idle_stops_before_more_writes() {
        let mut d = CacheDev::new(8 * MIB, 8 * MIB, SimMode::Genuine, MIB, 100e6, 20e6);
        let ctx = Ctx::new();
        let mut observe = || Err(io::Error::other("Drive identity changed: serial"));
        let mut run = state(&d, &ctx, &mut observe);
        run.seq_end = MIB;
        run.r.drop_at = Some(MIB / 2);
        assert!(run.recover(&mut d).is_err());
        assert_eq!(d.state.lock().unwrap().written, 0);
        assert_eq!(run.r.identity_changes.len(), 1);
    }

    #[test]
    fn final_checks_use_latest_tags_and_split_the_written_zones() {
        for corrupt in [false, true] {
            let mut d = CacheDev::new(32 * MIB, 32 * MIB, SimMode::Genuine, 32 * MIB, 100e6, 20e6);
            let ctx = Ctx::new();
            let mut observe = || Ok(None);
            let mut run = state(&d, &ctx, &mut observe);
            let positions = vec![0, MIB, 2 * MIB, 3 * MIB, 20 * MIB];
            for &off in &positions { fill(&mut d, Pattern::Seeded { tag: 7 }, off, off + 4096); }
            fill(&mut d, run.pat, 0, 4 * MIB);
            run.seq_end = 2 * MIB;
            run.mixed = (2 * MIB, 3 * MIB);
            run.recovery = (3 * MIB, 4 * MIB);
            run.r.drop_at = Some(MIB);
            run.r.probe = Some(ProbeResult { positions, tag: Some(7), block: 4096, ..Default::default() });
            if corrupt { d.state.lock().unwrap().disk.data[3 * MIB as usize + 100] ^= 1; }
            let result = run.final_checks(&mut d);
            if corrupt { assert!(result.is_err()); assert_eq!(run.r.status, Status::Defective); }
            else {
                result.unwrap();
                assert!(run.r.final_complete);
                assert_eq!(run.r.zones.len(), 3);
                assert!(run.r.zones.iter().all(|z| z.verified == z.end - z.start && z.read_4k_ms.is_some()));
                assert!(run.r.read_mb_s.unwrap() > 0.);
            }
        }
    }

    #[test]
    fn stall_interpretation_requires_evidence() {
        let mut stalls = vec![Stall { phase: "write".into(), offset: Some(2 * MIB), at: 10., seconds: 2., ..Default::default() }; 2];
        assert!(interpret_stalls(&stalls).contains("Too few"));
        stalls.push(Stall { phase: "read".into(), offset: Some(2 * MIB), at: 40., seconds: 3., ..Default::default() });
        assert!(interpret_stalls(&stalls).contains("Bad regions"));
        for (i,s) in stalls.iter_mut().enumerate() { s.offset = Some(i as u64 * MIB); s.at = i as f64 * 10.; s.seconds = 2. + i as f64; }
        assert!(interpret_stalls(&stalls).contains("Throttling"));
        stalls[2].at = 900.;
        assert!(interpret_stalls(&stalls).contains("Random hiccups"));
    }

    #[test]
    fn warnings_do_not_become_failed_verdicts() {
        let good = Diagnostics { final_complete: true, probe: Some(ProbeResult::default()), written: MIB, trim: "unsupported".into(), reads_freeze: true, link_limited: true, ..Default::default() };
        let mut r = good.clone();
        finish_verdict(&mut r);
        assert_eq!(r.status, Status::Ok);
        assert_eq!(r.status.exit_code(), 0);
        assert!(r.warnings.iter().any(|w| w.contains("Bulk storage")));
        for problem in 0..5 {
            let mut r = good.clone();
            match problem {
                0 => r.triage_failing = true,
                1 => r.identity_changes.push("serial".into()),
                2 => r.tally.stale = 1,
                3 => r.smart = Some(SmartRun { changes: vec![crate::smart::Change { what: Counter::LinkErrors, before: 0, after: 1 }], ..Default::default() }),
                _ => { r.status = Status::Unreliable; r.reason = "No I/O completed for 60 s".into(); }
            }
            finish_verdict(&mut r);
            assert_eq!(r.status.exit_code(), 1);
        }
    }

    #[test]
    fn verification_stall_replays_once_and_no_completion_aborts() {
        let ctx = Ctx::new();
        let mut observe = || Ok(None);
        let mut d = CacheDev::new(8 * MIB, 8 * MIB, SimMode::Genuine, 8 * MIB, 100e6, 20e6);
        let mut run = state(&d, &ctx, &mut observe);
        fill(&mut d, run.pat, 0, 8 * MIB);
        run.phase(6, "Final verification");
        run.verify(&mut d, run.pat, 0, 8 * MIB as usize, false).unwrap();
        assert_eq!(run.r.stalls.len(), 2);
        assert!(run.replayed);
        run.verify(&mut d, run.pat, 0, 8 * MIB as usize, false).unwrap();
        assert_eq!(run.r.stalls.len(), 3);
        let mut slow = CacheDev::new(MIB, MIB, SimMode::Genuine, 0, 64., 64.);
        let mut run = state(&slow, &ctx, &mut observe);
        assert!(run.sequential(&mut slow, true).unwrap_err().to_string().contains("No I/O completed"));
        assert_eq!(slow.state.lock().unwrap().written, 4096);
    }

    #[test]
    fn active_completions_distinguish_a_slow_operation_from_no_io() {
        let clock = Clock::simulated();
        clock.sleep(5.);
        assert_eq!(clock.complete(0., clock.now()), 5.);
        clock.sleep(5.);
        assert_eq!(clock.complete(5., clock.now()), 5.);
        clock.sleep(2.);
        assert_eq!(clock.complete(0., clock.now()), 2.);
        clock.sleep(10.);
        assert_eq!(clock.complete(12., clock.now()), 10.);
    }

    #[test]
    fn random_reads_classify_corruption_and_stop() {
        let mut d = CacheDev::new(4096, 4096, SimMode::Genuine, 4096, 100e6, 20e6);
        let result = crate::perf::random_io(vec![d.fork().unwrap()], crate::perf::RandomOpts { start: 0, span: 4096, write: false, pattern: Some(Pattern::Seeded { tag: 7 }), deadline: 100. }, &d.clock, &Ctx::new());
        assert_eq!(result.error.unwrap().kind(), io::ErrorKind::InvalidData);
        assert_eq!(result.tally.lost(), 8);
        assert!(d.clock.now() < 0.1);
    }
}
