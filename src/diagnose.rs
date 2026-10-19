//! Bounded whole-disk diagnostics. Sampling can find a fault, never prove every byte.
use crate::ctx::Ctx;
use crate::dev::{AlignedBuf, Dev, MIB};
use crate::engine::{self, Clock, Tally};
use crate::pattern::{Pattern, new_tag};
use crate::platform::DiskInfo;
use crate::probe::{self, Finding, ProbeResult};
use crate::report::Status;
use crate::smart::{Counter, Smart};
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
            self.worst_ms = self.latency[n - 1] * 1000.;
        }
    }
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
                let (t, p) = *since.get_or_insert((now, last.offset));
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
}

impl Run<'_> {
    fn phase(&mut self, n: usize, name: &str) {
        self.finish_phase();
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
        if elapsed >= 10.0 { return Err(io::Error::other(format!("No I/O completed for {elapsed:.1} s; a blocked call overran the limit."))); }
        Ok(())
    }
    fn record_at(&mut self, off: u64, n: usize, elapsed: f64, at: f64, write: bool) {
        let p = self.r.phases.last_mut().unwrap();
        p.bytes += n as u64;
        p.latency.push(elapsed);
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
        self.read(dev, off, &mut buf[..n])?;
        let mut t = Tally::default();
        if !t.check(pat, off, &buf[..n]) {
            self.r.tally.merge(&t);
            // Retry once, but keep the original mismatch as a finding even if it clears.
            self.read(dev, off, &mut buf[..n])?;
            self.r.status = if pat.buffer_ok(off, &buf[..n]) { Status::Unreliable } else if wrap || t.aliased > 0 { Status::Fake } else { Status::Defective };
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Written data changed at byte {off}.")));
        }
        self.r.tally.merge(&t);
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
        dev.flush()?;
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
            if let Some((_, pos)) = post_start { end = end.min(pos + self.r.class.post_cap()); }
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
            let mut times: Vec<_> = done.iter().map(|c| c.at).collect();
            times.sort_by(f64::total_cmp);
            let mut previous = start;
            for at in times {
                if at - previous >= 10. { return Err(io::Error::other(format!("No I/O completed for {:.1} s; a blocked write overran the limit.", at - previous))); }
                previous = at;
            }
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
                self.r.heat_suspected = self.r.baseline_read_mb_s.zip(self.r.drop_read_mb_s).is_some_and(|(a,b)| b < a * 0.75);
                post_start = Some((active, self.seq_end));
            }
            if self.r.class == DriveClass::Usb2 && active >= 10. && (35.0..=42.0).contains(&watch.peak) {
                self.r.link_limited = true;
                break;
            }
        }
        if self.seq_end > 0 { self.wrap_check(dev, self.seq_end, next_check / self.limits.check_every)?; }
        dev.flush()?;
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
    run_with(dev, class, smart.is_some_and(failing), limits, Clock::default(), ctx)
}

fn run_with(dev: &mut dyn Dev, class: DriveClass, short: bool, limits: Limits, clock: Clock, ctx: &Ctx) -> Diagnostics {
    let mut run = Run { clock, ctx, r: Diagnostics { class, budget_seconds: limits.total, ..Default::default() }, limits,
        pat: Pattern::Seeded { tag: new_tag() }, end: 0., phase_start: 0., seq_end: 0, overhead: 0 };
    let result = (|| {
        run.phase(0, "Triage");
        if !short { match run.trim(dev) { Err(e) if e.kind() == io::ErrorKind::TimedOut => run.r.warnings.push("TRIM check ran out of time.".into()), r => r? } }
        run.probe(dev)?;
        if run.r.status.exit_code() == 1 { return Ok(()); }
        run.sequential(dev, short)?;
        if short { run.r.status = Status::Unreliable; run.r.reason = "The drive reports a SMART alarm or pending bad sectors; only a small sample was tested.".into(); }
        Ok::<_, io::Error>(())
    })();
    if let Err(e) = result {
        run.r.reason = e.to_string();
        if e.kind() == io::ErrorKind::Interrupted { run.r.status = Status::Cancelled; }
        else if e.kind() != io::ErrorKind::TimedOut && run.r.status.exit_code() != 1 { run.r.status = Status::Unreliable; }
    }
    run.r.distinct_upper_bound = run.overhead + run.seq_end;
    run.finish_phase();
    run.r.seconds = run.clock.now();
    run.r
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
            let mut run = Run { clock, ctx: &ctx, limits: limits(DriveClass::Usb, 32 * MIB), r: Diagnostics::default(), pat: Pattern::Seeded { tag: 9 }, end: 500., phase_start: 0., seq_end: 0, overhead: 0 };
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
        assert_eq!(r.status, Status::Info, "{}", r.reason);
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
        let r = run_with(&mut d, DriveClass::Usb, true, l, clock, &ctx);
        assert!(r.probe.is_none());
        assert!(r.seconds <= 1.1);
        assert_eq!(r.written, 0);
    }

    #[test]
    fn unsupported_trim_never_idles() {
        let mut d = MemDev::new(8 * MIB, 8 * MIB, SimMode::Genuine);
        let clock = Clock::simulated();
        let ctx = Ctx::new();
        let mut run = Run { clock, ctx: &ctx, limits: limits(DriveClass::Usb, 8 * MIB), r: Diagnostics::default(), pat: Pattern::Seeded { tag: 9 }, end: 500., phase_start: 0., seq_end: 0, overhead: 0 };
        run.trim(&mut d).unwrap();
        run.finish_phase();
        assert!(run.r.trim.starts_with("unsupported"));
        assert_eq!(run.r.phases[0].seconds, 0.);
    }
}
