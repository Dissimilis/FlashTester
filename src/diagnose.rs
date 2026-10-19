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
        return if smart.is_some_and(|s| s.via.contains("NVMe") || s.via.contains("SAT")) { DriveClass::Sata } else { DriveClass::Usb };
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
    pub offset: u64,
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
    pub peak_mb_s: f64,
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

/// Windows use write-active time, excluding the deliberate verification pauses.
#[derive(Default)]
struct DropWatch { samples: Vec<Sample>, peak: f64, low25: Option<(f64, u64)>, low15: Option<(f64, u64)> }
impl DropWatch {
    fn push(&mut self, s: Sample) -> Option<u64> {
        self.samples.push(s);
        let last = self.samples.last().unwrap();
        let now = last.seconds;
        let window = |seconds: f64| {
            let start = (now - seconds).max(0.0);
            let mut bytes = 0.;
            let mut duration = 0.;
            let mut speeds = vec![];
            for (i, x) in self.samples.iter().enumerate().rev() {
                let from = if i == 0 { 0. } else { self.samples[i - 1].seconds };
                if x.seconds <= start { break; }
                let overlap = x.seconds - from.max(start);
                bytes += x.mb_s * overlap;
                duration += overlap;
                speeds.push(x.mb_s);
            }
            (bytes / duration.max(1e-9), duration, speeds)
        };
        if (1.0..=10.0).contains(&now) {
            let (speed, _, speeds) = window(1.0);
            if speeds.iter().all(|s| (s - speed).abs() <= speed * 0.15) { self.peak = self.peak.max(speed); }
        }
        let (speed, duration, _) = window(5.0);
        if duration < 4.999 || self.peak == 0. { return None; }
        let mut found = None;
        for (threshold, hold, since) in [(0.75, 10., &mut self.low25), (0.85, 20., &mut self.low15)] {
            if speed <= self.peak * threshold {
                let onset = self.samples.iter().rev().take_while(|s| s.mb_s <= self.peak * 0.85).last().map_or(last.offset, |s| s.offset);
                let (t, p) = *since.get_or_insert((now, onset));
                if now - t >= hold { found = Some(p); }
            } else { *since = None; }
        }
        found
    }
}

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
        if self.clock.now() >= self.end { return Err(io::Error::new(io::ErrorKind::TimedOut, "phase budget reached")); }
        Ok(())
    }
    fn record(&mut self, off: u64, n: usize, start: f64, write: bool) -> io::Result<()> {
        let elapsed = self.clock.now() - start;
        self.record_at(off, n, elapsed, self.clock.now(), write);
        if self.clock.complete(start, self.clock.now()) >= 10.0 { return Err(io::Error::other(format!("No I/O completed for {elapsed:.1} s; a blocked call overran the limit."))); }
        Ok(())
    }
    fn record_at(&mut self, off: u64, n: usize, elapsed: f64, at: f64, write: bool) {
        let p = self.r.phases.last_mut().unwrap();
        p.bytes += n as u64;
        p.operations += 1;
        p.worst_ms = p.worst_ms.max(elapsed * 1000.);
        if p.latency.len() < 65536 { p.latency.push(elapsed); }
        else { let slot = crate::pattern::xorshift(p.operations) % p.operations; if slot < p.latency.len() as u64 { p.latency[slot as usize] = elapsed; } }
        if elapsed >= 2.0 { self.r.stalls.push(Stall { phase: p.name.clone(), offset: off, at, seconds: elapsed, write, write_path: false }); }
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
    fn trim(&mut self, dev: &mut dyn Dev) -> io::Result<()> {
        self.phase(1, "TRIM and idle");
        self.r.trim = "not checked".into();
        self.ready()?;
        let command_end = self.end.min(self.clock.now() + 15.);
        self.end = command_end;
        let t = self.clock.now();
        let result = dev.discard(0, dev.len() / dev.align() as u64 * dev.align() as u64);
        self.record(0, 0, t, true)?;
        if let Err(e) = result {
            if crate::dev::is_disconnect(&e) || !matches!(e.kind(), io::ErrorKind::Unsupported | io::ErrorKind::InvalidInput) { return Err(e); }
            self.r.trim = format!("unsupported: {e}");
            return Ok(());
        }
        let n = (4 * MIB).min(self.limits.seq_cap) as usize;
        let mut buf = AlignedBuf::new(n);
        self.pat.fill(0, &mut buf[..n]);
        self.write(dev, 0, &buf[..n])?;
        self.overhead += n as u64;
        self.flush(dev)?;
        self.verify(dev, self.pat, 0, n, false)?;
        self.ready()?;
        let t = self.clock.now();
        let discarded = dev.discard(0, n as u64);
        self.record(0, 0, t, true)?;
        if let Err(e) = discarded { self.r.trim = format!("sample discard failed: {e}"); return Err(e); }
        self.read(dev, 0, &mut buf[..n])?;
        if buf[..n].as_chunks::<512>().0.iter().enumerate().any(|(i, b)| self.pat.buffer_ok(i as u64 * 512, b)) {
            self.r.trim = "probably not effective (tagged data remains; legal on some drives)".into();
        } else {
            self.r.trim = "effective in the sample".into();
            self.end = self.limits.end(1, self.phase_start);
            self.idle(30.)?;
            self.snapshot()?;
        }
        Ok(())
    }
    fn probe(&mut self, dev: &mut dyn Dev) -> io::Result<()> {
        self.phase(2, "Capacity probe");
        let (step, mut points) = probe::grid(dev.len(), dev.align() as u64, 1024);
        let max = (self.limits.write_cap.saturating_sub(self.overhead) / dev.align() as u64).min(1024) as usize;
        if max < 2 { return Err(io::Error::other("device too small for diagnostics")); }
        if points.len() > max {
            points = (0..max).map(|i| points[i * (points.len() - 1) / (max - 1)]).collect();
        }
        self.overhead += points.len() as u64 * dev.align() as u64;
        let budget = (self.end - self.clock.now()).max(0.0);
        let ctx = self.ctx.clone();
        let result = probe::run_limited(&mut Measured { dev, run: self }, points, step, budget, &ctx)?;
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
        let mut pos = 0;
        while self.clock.now() < end {
            let n = (len - pos).min(self.limits.block as u64) as usize;
            self.verify(dev, self.pat, pos, n, false)?;
            bytes += n as u64;
            pos = (pos + n as u64) % len;
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
            if done.iter().any(|c| c.gap >= 10.) { return Err(io::Error::other("No I/O completed for 10 s; a blocked write overran the limit.")); }
            for c in &done { self.check_stall(dev, c.off, c.seconds)?; }
            if self.seq_end == off { break; }
            active += elapsed;
            let sample = Sample { seconds: active, offset: off, bytes: self.seq_end - off, mb_s: (self.seq_end - off) as f64 / 1e6 / elapsed.max(1e-9) };
            self.ctx.progress(self.seq_end);
            self.ctx.sample(off, sample.mb_s);
            self.r.samples.push(sample.clone());
            let drop = watch.push(sample);
            self.r.peak_mb_s = watch.peak;
            if off == 0 { self.verify(dev, self.pat, 0, MIB.min(self.seq_end) as usize, false)?; }
            if self.seq_end >= next_check {
                self.wrap_check(dev, self.seq_end, next_check / self.limits.check_every - 1)?;
                next_check += self.limits.check_every;
                if self.r.baseline_read_mb_s.is_none() { self.r.baseline_read_mb_s = self.read_speed(dev, self.seq_end.min(GB), 2.)?; }
            }
            if let Some((_, pos)) = post_start {
                post_time += elapsed;
                self.r.post_mb_s = Some((self.seq_end - pos) as f64 / 1e6 / post_time.max(1e-9));
                if post_time >= 15. || self.seq_end - pos >= self.r.class.post_cap() { break; }
            } else if let Some(pos) = drop {
                self.r.drop_at = Some(pos);
                self.r.drop_confirmed_at = Some(self.seq_end);
                self.r.drop_read_mb_s = self.read_speed(dev, self.seq_end.min(GB), 3.)?;
                self.snapshot()?;
                self.r.heat_suspected |= self.r.baseline_read_mb_s.zip(self.r.drop_read_mb_s).is_some_and(|(a,b)| b < a * 0.75);
                post_start = Some((active, self.seq_end));
            }
            if self.r.class == DriveClass::Usb2 && active >= 10. && (35.0..=42.0).contains(&watch.peak) {
                self.r.link_limited = true;
                break;
            }
        }
        if self.seq_end > 0 { self.wrap_check(dev, self.seq_end, next_check / self.limits.check_every)?; }
        self.flush(dev)?;
        Ok(())
    }
}

/// No attribution from one or two pauses; position repetition needs distinct phases.
pub fn interpret_stalls(stalls: &[Stall]) -> String {
    if stalls.len() < 3 { return if stalls.is_empty() { "No stalls of 2 s or longer." } else { "Too few stalls to attribute a cause." }.into(); }
    for s in stalls {
        let same: Vec<_> = stalls.iter().filter(|x| x.offset / MIB == s.offset / MIB).collect();
        if same.len() >= 3 && same.iter().any(|x| x.phase != s.phase) {
            return format!("Bad regions near {}: stalls repeated here across phases.", crate::report::size(s.offset / MIB * MIB));
        }
    }
    let mut times: Vec<_> = stalls.iter().collect();
    times.sort_by(|a,b| a.at.total_cmp(&b.at));
    if times.windows(3).any(|w| w[2].at - w[0].at <= 60. && w[0].seconds < w[1].seconds && w[1].seconds < w[2].seconds && w[0].offset / MIB != w[2].offset / MIB) {
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
        match result {
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                self.r.warnings.push(format!("{} reached its budget; unfinished measurements are omitted.", self.r.phases.last().unwrap().name));
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
        for c in &result.stalls { self.r.stalls.push(Stall { phase: p.name.clone(), offset: c.off, at: c.at, seconds: c.seconds, write, write_path: false }); }
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
                if let Some(s) = self.r.stalls.iter_mut().rev().find(|s| s.offset == c.off && s.write) { s.write_path = self.clock.now() - t < 0.1; }
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
        // Initialise the bounded region so random reads always have known contents.
        let mut buf = AlignedBuf::new(self.limits.block.max(128 * 1024));
        while self.mixed.1 - self.mixed.0 < span {
            let n = (span - (self.mixed.1 - self.mixed.0)).min(self.limits.block as u64) as usize;
            self.pat.fill(self.mixed.1, &mut buf[..n]);
            self.write(dev, self.mixed.1, &buf[..n])?;
            self.mixed.1 += n as u64;
        }
        let seconds: f64 = if self.r.class == DriveClass::Nvme { 15. } else { 10. };
        let idle = self.random(dev, 0, self.seq_end, false, 1, seconds.min((self.end - self.clock.now()).max(0.) / 5.))?;
        self.r.idle_read_ms = Some(idle.p50_ms);
        let mixed_end = (self.clock.now() + (self.end - self.clock.now()) / 2.).min(self.end);
        let write_end = self.clock.now() + (mixed_end - self.clock.now()) * 2. / 3.;
        let started = self.clock.now();
        let mut mixed = Phase { name: "Mixed small writes".into(), ..Default::default() };
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
                loop {
                    match rx.recv_timeout(std::time::Duration::from_secs(1)) {
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        _ if stop_ref.load(std::sync::atomic::Ordering::SeqCst) || ctx.cancelled() || clock.now() >= write_end => break,
                        _ => {}
                    }
                    let start = clock.now();
                    let r = reader.read_at(0, &mut buf).and_then(|n| if n != 4096 { Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short overlapping read")) }
                        else if !pat.buffer_ok(0, &buf) { Err(io::Error::new(io::ErrorKind::InvalidData, "Old data changed during mixed writes")) } else { Ok(()) });
                    let gap = clock.complete(start, clock.now());
                    samples.push((start, clock.now() - start, r, gap));
                    if samples.last().unwrap().2.is_err() { stop_ref.store(true, std::sync::atomic::Ordering::SeqCst); break; }
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
            for (start, elapsed, result, gap) in reads {
                self.record_at(0, 4096, elapsed, start + elapsed, false);
                loaded.latency.push(elapsed); loaded.operations += 1;
                loaded.worst_ms = loaded.worst_ms.max(elapsed * 1000.);
                if result.is_err() { self.r.status = Status::Unreliable; }
                result?;
                if gap >= 10. { return Err(io::Error::other(format!("No I/O completed for {gap:.1} s during an overlapping read."))); }
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
        if let Some(stall) = self.r.stalls.iter_mut().rev().find(|s| s.offset == off && s.write) { stall.write_path = fast; }
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
                if done.iter().any(|c| c.gap >= 10.) { return Err(io::Error::other("No I/O completed for 10 s during stall replay.")); }
                if done.is_empty() { break; }
            }
        }
        Ok(())
    }
    fn recover(&mut self, dev: &mut dyn Dev) -> io::Result<()> {
        if self.r.drop_at.is_none() || self.r.class == DriveClass::Usb2 { return Ok(()); }
        self.phase(5, "Recovery");
        let original = self.r.drop_at.unwrap();
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
                if done.iter().any(|c| c.gap >= 10.) { return Err(io::Error::other("No I/O completed for 10 s during recovery.")); }
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
            let old = Pattern::Seeded { tag: p.tag.unwrap() };
            for off in p.positions {
                // Probe points touched by a later phase now carry the main run tag.
                let pat = if off < self.seq_end || (self.mixed.0..self.mixed.1).contains(&off) || (self.recovery.0..self.recovery.1).contains(&off) { self.pat } else { old };
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
        self.r.final_complete = true;
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
        if !short { let result = run.trim(dev); run.soft(result)?; }
        let result = run.probe(dev); run.soft(result)?;
        if run.r.status.exit_code() == 1 { return Ok(()); }
        let result = run.sequential(dev, short); run.soft(result)?;
        if !short {
            let result = run.small(dev); run.soft(result)?;
            let result = run.recover(dev); run.soft(result)?;
        }
        let result = run.final_checks(dev); run.soft(result)?;
        Ok::<_, io::Error>(())
    })();
    if let Err(e) = result {
        run.r.reason = e.to_string();
        if e.kind() == io::ErrorKind::Interrupted { run.r.status = Status::Cancelled; }
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
    if r.triage_failing { r.warnings.push("SMART was already reporting failure; only the probe and a small verified sample were run.".into()); }
    if r.reads_freeze { r.warnings.push("Bulk storage only: it freezes reads while writing.".into()); }
    if r.link_limited { r.warnings.push("The measured speed is at the USB 2 ceiling (about 35-40 MB/s); this port limits speed.".into()); }
    if r.peak_mb_s > 0. && r.peak_mb_s < 5. { r.warnings.push("Sequential writes are very slow.".into()); }
    if !r.trim.starts_with("effective") { r.warnings.push("Cache figures may be understated: the drive was not trimmed.".into()); }
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
        let mut s = Smart { via: "SAT over USB".into(), ..Default::default() };
        assert_eq!(classify(&d, Some(&s)), DriveClass::Sata);
        d.usb = Some(crate::platform::UsbInfo { slow_link: true, ..Default::default() });
        assert_eq!(classify(&d, Some(&s)), DriveClass::Usb2);
        s.key.push(crate::smart::Key { what: Counter::Reallocated, value: 42 });
        assert!(!failing(&s));
        s.key.push(crate::smart::Key { what: Counter::Pending, value: 1 });
        assert!(failing(&s));
    }

    #[test]
    fn drop_requires_sustained_slowdown() {
        for (slow, expected_time) in [(70., 24.), (82., 34.)] {
            let mut watch = DropWatch::default();
            let mut found = None;
            for i in 1..=250 {
                let speed = if i <= 50 { 100. } else { slow };
                if watch.push(Sample { seconds: i as f64 * 0.2, offset: i * 20000, bytes: 20000, mb_s: speed }).is_some() {
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
    fn probe_deadline_covers_speed_calibration() {
        let mut d = CacheDev::new(8 * MIB, 128 * MIB, SimMode::Drop, 0, 4096., 4096.);
        let clock = d.clock.clone();
        let ctx = Ctx::new();
        let mut l = limits(DriveClass::Usb, 128 * MIB);
        l.phases[2] = 0.5;
        let mut run = Run { clock, ctx: &ctx, limits: l, r: Diagnostics::default(), pat: Pattern::Seeded { tag: 9 }, end: 500., phase_start: 0., seq_end: 0, overhead: 0, mixed: (0,0), recovery: (0,0), before: None, observe: &mut || Ok(None), replayed: false, reads: Default::default() };
        assert_eq!(run.probe(&mut d).unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(run.r.probe.is_none());
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
        let mut stalls = vec![Stall { phase: "write".into(), offset: 2 * MIB, at: 10., seconds: 2., ..Default::default() }; 2];
        assert!(interpret_stalls(&stalls).contains("Too few"));
        stalls.push(Stall { phase: "read".into(), offset: 2 * MIB, at: 40., seconds: 3., ..Default::default() });
        assert!(interpret_stalls(&stalls).contains("Bad regions"));
        for (i,s) in stalls.iter_mut().enumerate() { s.offset = i as u64 * MIB; s.at = i as f64 * 10.; s.seconds = 2. + i as f64; }
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
                _ => { r.status = Status::Unreliable; r.reason = "No I/O completed for 10 s".into(); }
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
        let mut slow = CacheDev::new(MIB, MIB, SimMode::Genuine, 0, 256., 256.);
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
