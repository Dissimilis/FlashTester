//! Reports: the result record every command produces, a plain-text summary,
//! a detailed HTML report and the refund-evidence one-pager.

use crate::engine::{IoStats, VerifyResult, WriteResult};
use crate::perf::{IopsResult, SmallFilesResult};
use crate::platform::{DiskInfo, VolumeInfo};
use crate::probe::ProbeResult;
use crate::scan::ScanResult;
use crate::trust::PlugPullResult;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

/// Human-readable size in decimal units (as drive makers count).
pub fn size(b: u64) -> String {
    let b = b as f64;
    if b >= 1e12 {
        format!("{:.2} TB", b / 1e12)
    } else if b >= 1e9 {
        format!("{:.2} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.1} KB", b / 1e3)
    } else {
        format!("{b} B")
    }
}

pub fn duration(s: f64) -> String {
    let s = s.round() as u64;
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Passed.
    Ok,
    /// Holds less than it claims.
    Fake,
    /// Loses or corrupts data.
    Defective,
    /// Ignores writes.
    ReadOnly,
    /// Works, but with I/O errors, disconnects or unstable reads.
    Unreliable,
    /// Informational command (list, info, iops) without a pass/fail.
    #[default]
    Info,
    Cancelled,
    /// Refused for safety or bad arguments (system disk, missing --destroy, not admin).
    Refused,
    Error,
}

impl Status {
    pub fn exit_code(self) -> i32 {
        match self {
            Status::Ok | Status::Info => 0,
            Status::Fake | Status::Defective | Status::ReadOnly | Status::Unreliable => 1,
            Status::Refused => 2,
            Status::Error => 3,
            Status::Cancelled => 130,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::Fake => "FAKE",
            Status::Defective => "DEFECTIVE",
            Status::ReadOnly => "READ-ONLY",
            Status::Unreliable => "UNRELIABLE",
            Status::Info => "DONE",
            Status::Cancelled => "CANCELLED",
            Status::Refused => "REFUSED",
            Status::Error => "ERROR",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Verdict {
    pub status: Status,
    /// One plain-language sentence.
    pub headline: String,
    pub details: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Target {
    /// "file" or "raw"
    pub mode: String,
    pub path: Option<String>,
    pub disk: Option<DiskInfo>,
    pub volume: Option<VolumeInfo>,
    /// Capacity the drive (or volume) claims.
    pub claimed: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PassSummary {
    pub pass: u32,
    pub tag: String,
    pub bytes: u64,
    pub write_mb_s: f64,
    pub read_mb_s: f64,
    pub lost_sectors: u64,
    pub result: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Report {
    pub tool: String,
    pub command: String,
    pub started: String,
    pub finished: String,
    pub seconds: f64,
    pub target: Target,
    pub verdict: Verdict,
    pub warnings: Vec<String>,
    pub notes: Vec<String>,
    /// The operation opens the target only for reading, including failed checks.
    #[serde(default)]
    pub read_only: bool,
    /// Bytes of test data covered.
    pub tested: u64,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub write: Vec<WriteResult>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub verify: Vec<VerifyResult>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub passes: Vec<PassSummary>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub probe: Option<ProbeResult>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub scan: Option<ScanResult>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub iops: Option<IopsResult>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub small_files: Option<SmallFilesResult>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub plug_pull: Option<PlugPullResult>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bench: Option<crate::perf::BenchResult>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub identity_changes: Vec<String>,
    /// What the drive's own SMART counters did during the run.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub smart: Option<crate::smart::SmartRun>,
    /// Offline NAND lookup or imported controller response, never evidence of usable capacity.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub flash: Option<crate::flash::Identification>,
}

pub fn tool_name() -> String {
    format!("FlashTester {}", env!("CARGO_PKG_VERSION"))
}

impl Report {
    pub fn new(command: &str, target: Target) -> Self {
        let mut r = Report { tool: tool_name(), command: command.into(), started: crate::engine::now(), target, ..Default::default() };
        r.identity_warnings();
        r
    }

    fn identity_warnings(&mut self) {
        if let Some(d) = &self.target.disk {
            if let Some(u) = &d.usb
                && u.slow_link {
                    let cap = if u.superspeed_capable == Some(true) { " The drive supports USB 3; try a USB 3 port, cable or reader." } else { "" };
                    self.warnings.push(format!(
                        "Connected at {}. The USB link limits transfer speed.{cap}",
                        u.link
                    ));
                }
            if let Some(u) = d.usb.as_ref().filter(|u| u.superspeed_plus_capable == Some(true) && !u.link.contains("SuperSpeed+")) {
                self.warnings.push(format!(
                    "Connected at {}. The drive supports 10 Gbit/s USB. Use a compatible 10 Gbit/s port and cable for a faster link.",
                    u.link
                ));
            }
            if let Some(p) = &d.placeholder_serial {
                self.warnings.push(format!("Check the serial number: {p}. This alone does not show whether the drive is fake."));
            }
            if d.vendor.is_empty() && d.product.is_empty() {
                self.warnings.push("The drive reports no vendor or product name.".into());
            }
        }
    }

    pub fn finish(&mut self, start: std::time::Instant) {
        self.finished = crate::engine::now();
        self.seconds = start.elapsed().as_secs_f64();
    }

    pub fn set(&mut self, status: Status, headline: impl Into<String>) {
        self.verdict.status = status;
        self.verdict.headline = headline.into();
    }

    pub fn exit_code(&self) -> i32 {
        self.verdict.status.exit_code()
    }

    /// Short human-readable summary.
    pub fn text(&self) -> String {
        let mut s = String::new();
        if self.verdict.status == Status::Refused {
            let _ = writeln!(s, "Refused: {}", self.verdict.headline);
            return s;
        }
        let _ = writeln!(s, "{} - {}", self.tool, self.command);
        let _ = writeln!(s, "Target: {}", target_line(&self.target));
        let _ = writeln!(s);
        let _ = writeln!(s, "Verdict: {} - {}", self.verdict.status.label(), self.verdict.headline);
        for d in &self.verdict.details {
            let _ = writeln!(s, "  {d}");
        }
        if let Some(p) = &self.probe {
            let _ = writeln!(s);
            for l in p.summary_lines() {
                let _ = writeln!(s, "{l}");
            }
        }
        if self.passes.len() > 1 && self.command == "verify" {
            let _ = writeln!(s, "\nLoop  Read MB/s  Lost sectors  Result");
            for p in &self.passes {
                let _ = writeln!(s, "{:>4}  {:>9.1}  {:>12}  {}", p.pass, p.read_mb_s, p.lost_sectors, p.result);
            }
        } else if self.passes.len() > 1 {
            let _ = writeln!(s, "\nPass  Written     Write MB/s  Read MB/s  Lost sectors  Result");
            for p in &self.passes {
                let _ = writeln!(s, "{:>4}  {:>10}  {:>10.1}  {:>9.1}  {:>12}  {}", p.pass, size(p.bytes), p.write_mb_s, p.read_mb_s, p.lost_sectors, p.result);
            }
        }
        if let Some(w) = self.write.last() {
            let _ = writeln!(s);
            let _ = writeln!(s, "Write: {}", io_line(&w.stats));
            if let Some(c) = slc_cliff(&w.stats) {
                let _ = writeln!(s, "       {c}");
            }
        }
        if let Some(v) = self.verify.last() {
            let _ = writeln!(s, "Read:  {}", io_line(&v.stats));
            let t = &v.tally;
            let _ = writeln!(s, "Sectors: {} OK, {} lost", t.ok, t.lost());
            if t.lost() > 0 {
                let _ = writeln!(
                    s,
                    "  changed (<8 bits) {}, corrupted {}, aliased {}, stale (new data was not stored) {}, unreadable {}",
                    t.changed, t.corrupted, t.aliased, t.stale, t.unreadable
                );
                if let Some(f) = &t.first_error {
                    let _ = writeln!(s, "  First error at offset 0x{:x} ({}): expected 0x{:016x}, found 0x{:016x}", f.offset, size(f.offset), f.expected, f.found);
                }
            }
            for r in v.stats.slow_regions().iter().take(5) {
                let _ = writeln!(s, "  Slow read region at {}: {:.1} MB/s", size(r.pos), r.mb_s);
            }
        }
        if let Some(sc) = &self.scan {
            let _ = writeln!(s);
            for l in sc.summary_lines() {
                let _ = writeln!(s, "{l}");
            }
        }
        if let Some(i) = &self.iops {
            let _ = writeln!(s, "\nRandom 4K I/O ({} span):", size(i.span));
            let _ = writeln!(s, "  Kind   QD   IOPS     MB/s   p50 ms  p99 ms  max ms");
            for r in &i.rows {
                let _ = writeln!(s, "  {:<5} {:>3} {:>7.0} {:>8.2} {:>8.2} {:>7.2} {:>7.1}", r.kind, r.qd, r.iops, r.mb_s, r.p50_ms, r.p99_ms, r.max_ms);
            }
        }
        if let Some(f) = &self.small_files {
            let _ = writeln!(s);
            for l in f.summary_lines() {
                let _ = writeln!(s, "{l}");
            }
        }
        if let Some(b) = &self.bench {
            let _ = writeln!(s);
            for l in b.summary_lines() {
                let _ = writeln!(s, "{l}");
            }
        }
        if let Some(p) = &self.plug_pull {
            let _ = writeln!(s);
            for l in p.summary_lines() {
                let _ = writeln!(s, "{l}");
            }
        }
        let events: Vec<_> = self.write.iter().flat_map(|w| &w.events).chain(self.verify.iter().flat_map(|v| &v.events)).collect();
        if !events.is_empty() {
            let _ = writeln!(s, "\nEvents:");
            for e in events.iter().take(20) {
                let _ = writeln!(s, "  {} {} at {}: {}", e.time, e.kind, size(e.offset), e.detail);
            }
            if events.len() > 20 {
                let _ = writeln!(s, "  ... {} more in the JSON/HTML report", events.len() - 20);
            }
        }
        if !self.identity_changes.is_empty() {
            let _ = writeln!(s, "\nIdentity changed after reconnecting:");
            for c in &self.identity_changes {
                let _ = writeln!(s, "  {c}");
            }
        }
        if let Some(m) = &self.smart {
            let _ = writeln!(s, "\nDrive's own counters (SMART via {}):", m.via);
            if let (Some(lo), Some(hi)) = (m.temps.iter().map(|t| t.c).min(), m.temps.iter().map(|t| t.c).max()) {
                let range = if lo == hi { lo.to_string() } else { format!("{lo}-{hi}") };
                let _ = writeln!(s, "  Temperature {range} C");
            }
            for c in &m.changes {
                let _ = writeln!(s, "  {}: {} -> {}", c.what.label(), c.what.show(c.before), c.what.show(c.after));
            }
        }
        if !self.warnings.is_empty() {
            let _ = writeln!(s, "\nWarnings:");
            for w in &self.warnings {
                let _ = writeln!(s, "  ! {w}");
            }
        }
        for n in &self.notes {
            let _ = writeln!(s, "Note: {n}");
        }
        let _ = writeln!(s, "\nFinished {} ({})", self.finished, duration(self.seconds));
        s
    }
}

pub fn target_line(t: &Target) -> String {
    let mut parts = vec![];
    if let Some(p) = &t.path {
        parts.push(p.clone());
    }
    if let Some(d) = &t.disk {
        parts.push(format!("disk {} {} ({}, {})", d.id, d.name(), size(d.size), d.bus));
        if let Some(u) = &d.usb {
            parts.push(if u.link.is_empty() { format!("USB {}:{}", u.vid, u.pid) } else { format!("USB {}:{} at {}", u.vid, u.pid, u.link) });
        }
    }
    if let Some(v) = &t.volume {
        parts.push(format!("{} {}", v.fs, size(v.size)));
    }
    parts.join(", ")
}

fn io_line(st: &IoStats) -> String {
    let mut s = format!("{} in {} at {:.1} MB/s", size(st.bytes), duration(st.seconds), st.mb_s);
    if st.worst_ms > 0.0 {
        let _ = write!(s, "; slowest I/O {:.0} ms at {}", st.worst_ms, size(st.worst_at));
    }
    if st.stalls_over_1s > 0 {
        let _ = write!(s, "; {} stalls over 1 s", st.stalls_over_1s);
    }
    s
}

/// "Write speed drops from X to Y after Z" when the start is much faster than the rest.
pub fn slc_cliff(st: &IoStats) -> Option<String> {
    let m = &st.speed_map;
    if m.len() < 10 {
        return None;
    }
    let head = m[0].mb_s;
    let mut tail: Vec<f64> = m[m.len() / 2..].iter().map(|p| p.mb_s).collect();
    tail.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let tail_med = tail[tail.len() / 2];
    if head < tail_med * 1.8 {
        return None;
    }
    let at = m.iter().find(|p| p.mb_s < (head + tail_med) / 2.0)?;
    Some(format!("Write speed drops from {:.0} to about {:.0} MB/s after {} (possible write-cache limit)", head, tail_med, size(at.pos)))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Inline SVG line chart of MB/s by position.
fn speed_svg(series: &[(&str, &str, &IoStats)]) -> String {
    let (w, h, pad) = (760.0, 220.0, 40.0);
    let maxx = series.iter().flat_map(|s| s.2.speed_map.iter().map(|p| p.pos)).max().unwrap_or(1).max(1) as f64;
    let maxy = series.iter().flat_map(|s| s.2.speed_map.iter().map(|p| p.mb_s)).fold(1.0, f64::max) * 1.1;
    let mut s = format!(r#"<svg viewBox="0 0 {w} {h}" class="chart" role="img">"#);
    for i in 0..=4 {
        let y = pad / 2.0 + (h - pad * 1.5) * i as f64 / 4.0;
        let v = maxy * (1.0 - i as f64 / 4.0);
        let _ = write!(s, r#"<line x1="{pad}" x2="{}" y1="{y}" y2="{y}" class="grid"/><text x="2" y="{}" class="lbl">{v:.0}</text>"#, w - 10.0, y + 4.0);
    }
    let _ = write!(s, r#"<text x="{}" y="{}" class="lbl">{}</text><text x="{pad}" y="{}" class="lbl">0</text>"#, w - 70.0, h - 4.0, size(maxx as u64), h - 4.0);
    for (name, color, st) in series {
        let pts: Vec<String> = st
            .speed_map
            .iter()
            .map(|p| {
                let x = pad + (w - pad - 10.0) * p.pos as f64 / maxx;
                let y = pad / 2.0 + (h - pad * 1.5) * (1.0 - p.mb_s / maxy);
                format!("{x:.1},{y:.1}")
            })
            .collect();
        let _ = write!(s, r#"<polyline fill="none" stroke="{color}" stroke-width="2" points="{}"><title>{name}</title></polyline>"#, pts.join(" "));
    }
    s.push_str("</svg>");
    let legend: Vec<String> = series.iter().map(|(n, c, _)| format!(r#"<span style="color:{c}">&#9644; {n}</span>"#)).collect();
    format!("{s}<div class=\"legend\">{}. MB/s by position</div>", legend.join(" "))
}

const CSS: &str = r#"
:root{--bg:#fff;--fg:#1b1f24;--mut:#5b6470;--card:#f5f7fa;--line:#d8dde3;--ok:#1a7f37;--bad:#c62828;--warn:#9a6700}
@media (prefers-color-scheme: dark){:root{--bg:#111418;--fg:#e6e9ed;--mut:#9aa4af;--card:#1a1f25;--line:#2d343c;--ok:#4cc26b;--bad:#ff6b6b;--warn:#e3b341}}
body{background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,Segoe UI,sans-serif;max-width:860px;margin:0 auto;padding:16px}
h1{font-size:22px;margin:0 0 4px}h2{font-size:17px;margin:24px 0 8px;border-bottom:1px solid var(--line);padding-bottom:4px}
.mut{color:var(--mut)}.card{background:var(--card);border-radius:8px;padding:12px 16px;margin:12px 0}
.verdict{font-size:20px;font-weight:600}.s-ok{color:var(--ok)}.s-bad{color:var(--bad)}.s-warn{color:var(--warn)}
table{border-collapse:collapse;width:100%;font-size:14px}td,th{border-bottom:1px solid var(--line);padding:4px 6px;text-align:left;vertical-align:top}
.chart{width:100%;height:auto}.grid{stroke:var(--line)}.lbl{fill:var(--mut);font-size:11px}.legend{font-size:13px;color:var(--mut)}
.map{display:flex;flex-wrap:wrap;gap:1px}.map i{width:8px;height:12px;display:block}.p-ok{background:var(--ok)}.p-bad{background:var(--bad)}.p-alias{background:var(--warn)}.p-unknown{background:var(--mut)}
code{font-size:13px}ul{margin:4px 0;padding-left:20px}
"#;

fn status_class(s: Status) -> &'static str {
    match s {
        Status::Ok | Status::Info => "s-ok",
        Status::Unreliable | Status::Cancelled => "s-warn",
        _ => "s-bad",
    }
}

fn identity_rows(t: &Target) -> String {
    let mut s = String::new();
    let mut row = |k: &str, v: String| {
        if !v.is_empty() {
            let _ = write!(s, "<tr><th>{k}</th><td>{}</td></tr>", esc(&v));
        }
    };
    row("Path", t.path.clone().unwrap_or_default());
    if let Some(d) = &t.disk {
        row("Disk", format!("{} {} ({})", d.id, d.name(), d.path));
        let none = |x: &str| if x.is_empty() { "(none)".to_string() } else { x.to_string() };
        row("Vendor / product / revision", format!("{} / {} / {}", none(&d.vendor), none(&d.product), none(&d.revision)));
        row("Serial", d.serial.clone());
        row("Claimed capacity", format!("{} ({} bytes)", size(d.size), d.size));
        row("Bus", format!("{}{}", d.bus, if d.removable { ", removable" } else { "" }));
        if let Some(u) = &d.usb {
            row("USB ID", format!("{}, USB {}", u.id_text(), u.usb_version));
            row("USB serial", u.serial.clone().unwrap_or_else(|| "(none)".into()));
            row("Link", u.link.clone());
            row("USB device", u.device_text());
        }
        if !d.controller.is_empty() {
            row("Controller", d.controller.clone());
        }
        if let Some(c) = &d.card {
            row("Card", format!("{}, serial {}", c.text(), c.serial));
        }
        row(if cfg!(windows) { "Windows device" } else { "Device" }, d.instance_id.clone());
    }
    if let Some(v) = &t.volume {
        row("File system", format!("{} \"{}\", {} ({} free)", v.fs, v.label, size(v.size), size(v.free)));
    }
    s
}

impl Report {
    /// Detailed standalone HTML report.
    pub fn html(&self) -> String {
        let mut b = String::new();
        let _ = write!(b, "<!doctype html><html><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>FlashTester report</title><style>{CSS}</style></head><body>");
        let _ = write!(b, "<h1>FlashTester report</h1><div class=mut>{}: {}. Started {}. Duration: {}.</div>", esc(&self.tool), esc(&self.command), esc(&self.started), duration(self.seconds));
        let _ = write!(b, "<div class=card><div class=\"verdict {}\">{}: {}</div>", status_class(self.verdict.status), self.verdict.status.label(), esc(&self.verdict.headline));
        if !self.verdict.details.is_empty() {
            b.push_str("<ul>");
            for d in &self.verdict.details {
                let _ = write!(b, "<li>{}</li>", esc(d));
            }
            b.push_str("</ul>");
        }
        b.push_str("</div>");
        if !self.warnings.is_empty() {
            b.push_str("<h2>Warnings</h2><ul>");
            for w in &self.warnings {
                let _ = write!(b, "<li class=s-warn>{}</li>", esc(w));
            }
            b.push_str("</ul>");
        }
        let _ = write!(b, "<h2>Drive</h2><table>{}</table>", identity_rows(&self.target));
        if let Some(p) = &self.probe {
            b.push_str("<h2>Capacity probe</h2><ul>");
            for l in p.summary_lines() {
                let _ = write!(b, "<li>{}</li>", esc(&l));
            }
            let _ = write!(b, "</ul>{}", p.map_html());
        }
        if !self.write.is_empty() || !self.verify.is_empty() {
            b.push_str("<h2>Speed</h2>");
            let mut series = vec![];
            if let Some(w) = self.write.last() {
                series.push(("write", "#2f81f7", &w.stats));
            }
            if let Some(v) = self.verify.last() {
                series.push(("read", "#a371f7", &v.stats));
            }
            b.push_str(&speed_svg(&series));
            b.push_str("<table><tr><th></th><th>Data</th><th>Time</th><th>MB/s</th><th>Slowest I/O</th><th>Stalls &gt;1 s</th></tr>");
            for (n, st) in self.write.iter().map(|w| ("Write", &w.stats)).chain(self.verify.iter().map(|v| ("Read", &v.stats))) {
                let _ = write!(
                    b,
                    "<tr><td>{n}</td><td>{}</td><td>{}</td><td>{:.1}</td><td>{:.0} ms at {}</td><td>{}</td></tr>",
                    size(st.bytes),
                    duration(st.seconds),
                    st.mb_s,
                    st.worst_ms,
                    size(st.worst_at),
                    st.stalls_over_1s
                );
            }
            b.push_str("</table>");
            if let Some(c) = self.write.last().and_then(|w| slc_cliff(&w.stats)) {
                let _ = write!(b, "<p>{}</p>", esc(&c));
            }
        }
        if let Some(v) = self.verify.last() {
            let t = &v.tally;
            b.push_str("<h2>Verification</h2><table>");
            for (k, n) in [("OK", t.ok), ("Slightly changed (&lt;8 bits)", t.changed), ("Corrupted", t.corrupted), ("Aliased (wraparound)", t.aliased), ("Stale (new data was not stored)", t.stale), ("Unreadable", t.unreadable)] {
                let _ = write!(b, "<tr><th>{k}</th><td>{n} sectors ({})</td></tr>", size(n * 512));
            }
            b.push_str("</table>");
            if let Some(f) = &t.first_error {
                let _ = write!(b, "<p>First error at offset <code>0x{:x}</code> ({}): expected <code>0x{:016x}</code>, found <code>0x{:016x}</code></p>", f.offset, size(f.offset), f.expected, f.found);
            }
            if !t.bad_ranges.is_empty() {
                b.push_str("<details><summary>Bad ranges</summary><table><tr><th>Start</th><th>End</th><th>Kind</th></tr>");
                for r in t.bad_ranges.iter().take(300) {
                    let _ = write!(b, "<tr><td>{} (0x{:x})</td><td>{}</td><td>{}</td></tr>", size(r.start), r.start, size(r.end), esc(&r.kind));
                }
                b.push_str("</table></details>");
            }
        }
        if self.passes.len() > 1 {
            b.push_str("<h2>Passes</h2><table><tr><th>Pass</th><th>Written</th><th>Write MB/s</th><th>Read MB/s</th><th>Lost sectors</th><th>Result</th></tr>");
            for p in &self.passes {
                let _ = write!(b, "<tr><td>{}</td><td>{}</td><td>{:.1}</td><td>{:.1}</td><td>{}</td><td>{}</td></tr>", p.pass, size(p.bytes), p.write_mb_s, p.read_mb_s, p.lost_sectors, esc(&p.result));
            }
            b.push_str("</table>");
        }
        if let Some(sc) = &self.scan {
            b.push_str("<h2>Surface scan</h2><ul>");
            for l in sc.summary_lines() {
                let _ = write!(b, "<li>{}</li>", esc(&l));
            }
            b.push_str("</ul>");
            b.push_str(&speed_svg(&[("read", "#a371f7", &sc.stats)]));
        }
        if let Some(bn) = &self.bench {
            b.push_str("<h2>Sequential speed</h2><ul>");
            for l in bn.summary_lines() {
                let _ = write!(b, "<li>{}</li>", esc(&l));
            }
            b.push_str("</ul>");
            let mut series = vec![];
            if let Some(w) = &bn.write {
                series.push(("write", "#2f81f7", w));
            }
            series.push(("read", "#a371f7", &bn.read));
            b.push_str(&speed_svg(&series));
        }
        if let Some(i) = &self.iops {
            b.push_str("<h2>Random 4K I/O</h2><table><tr><th>Kind</th><th>QD</th><th>IOPS</th><th>MB/s</th><th>p50 ms</th><th>p99 ms</th><th>max ms</th></tr>");
            for r in &i.rows {
                let _ = write!(b, "<tr><td>{}</td><td>{}</td><td>{:.0}</td><td>{:.2}</td><td>{:.2}</td><td>{:.2}</td><td>{:.1}</td></tr>", esc(&r.kind), r.qd, r.iops, r.mb_s, r.p50_ms, r.p99_ms, r.max_ms);
            }
            b.push_str("</table>");
        }
        for (title, lines) in [
            ("Small files", self.small_files.as_ref().map(|f| f.summary_lines())),
            ("Pull-the-plug test", self.plug_pull.as_ref().map(|p| p.summary_lines())),
        ] {
            if let Some(lines) = lines {
                let _ = write!(b, "<h2>{title}</h2><ul>");
                for l in lines {
                    let _ = write!(b, "<li>{}</li>", esc(&l));
                }
                b.push_str("</ul>");
            }
        }
        let events: Vec<_> = self.write.iter().flat_map(|w| &w.events).chain(self.verify.iter().flat_map(|v| &v.events)).collect();
        if !events.is_empty() {
            b.push_str("<h2>Events</h2><table><tr><th>Time</th><th>Kind</th><th>Offset</th><th>Detail</th></tr>");
            for e in events.iter().take(500) {
                let _ = write!(b, "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>", esc(&e.time), esc(&e.kind), size(e.offset), esc(&e.detail));
            }
            b.push_str("</table>");
            if events.iter().any(|e| e.kind == "disconnect" || e.kind == "unstable" || e.kind == "transient-io") {
                b.push_str("<p class=s-warn>Disconnects and intermittent read errors can come from the drive or its connection. Retest with another port, cable or reader to help locate the fault.</p>");
            }
        }
        if !self.identity_changes.is_empty() {
            b.push_str("<h2>Identity changes after reconnecting</h2><ul>");
            for c in &self.identity_changes {
                let _ = write!(b, "<li class=s-bad>{}</li>", esc(c));
            }
            b.push_str("</ul>");
        }
        for n in &self.notes {
            let _ = write!(b, "<p class=mut>{}</p>", esc(n));
        }
        let _ = write!(b, "<p class=mut>Generated {} by {}.</p></body></html>", esc(&self.finished), esc(&self.tool));
        b
    }

    /// One page to attach to a marketplace dispute: claimed vs real capacity,
    /// identity, the evidence, timestamps and tool version.
    pub fn evidence_html(&self) -> String {
        let mut b = String::new();
        let identification_only = (self.command == "identify" || self.flash.is_some()) && self.probe.is_none() && self.write.is_empty() && self.verify.is_empty();
        let transfer = matches!(self.command.as_str(), "backup" | "restore");
        let title = if transfer { "Backup and restore summary" } else if identification_only { "Flash identification evidence" } else { "Flash drive test evidence" };
        let _ = write!(b, "<!doctype html><html><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>{title}</title><style>{CSS} @media print{{body{{max-width:none}}:root{{--bg:#fff;--fg:#000;--mut:#444;--card:#f3f3f3;--line:#ccc;--ok:#1a7f37;--bad:#c62828;--warn:#9a6700}}}}</style></head><body>");
        let _ = write!(b, "<h1>{title}</h1>");
        let claimed = self.target.disk.as_ref().map(|d| d.size).unwrap_or(self.target.claimed);
        let real = self.real_capacity();
        let _ = write!(b, "<div class=card><div class=\"verdict {}\">{}: {}</div>", status_class(self.verdict.status), self.verdict.status.label(), esc(&self.verdict.headline));
        b.push_str("<table>");
        if !identification_only {
            let _ = write!(b, "<tr><th>Claimed capacity</th><td>{} ({claimed} bytes)</td></tr>", size(claimed));
        }
        if let Some(r) = real {
            let pct = if claimed > 0 { r as f64 * 100.0 / claimed as f64 } else { 0.0 };
            let _ = write!(b, "<tr><th>Estimated usable capacity</th><td><b>{}</b> ({r} bytes, {pct:.1}% of claimed)</td></tr>", size(r));
        }
        let operation = if identification_only { "Offline identification".to_string() } else { format!("{} ({})", self.command, self.target.mode) };
        let operation_label = if identification_only || transfer { "Operation" } else { "Test" };
        let _ = write!(b, "<tr><th>{operation_label}</th><td>{}</td></tr><tr><th>Started / finished</th><td>{} / {}</td></tr><tr><th>Tool</th><td>{}</td></tr></table></div>", esc(&operation), esc(&self.started), esc(&self.finished), esc(&self.tool));
        let identity_label = if identification_only { "Input" } else { "Drive identity" };
        let _ = write!(b, "<h2>{identity_label}</h2><table>{}</table>", identity_rows(&self.target));
        b.push_str("<h2>Evidence</h2><ul>");
        for d in &self.verdict.details {
            let _ = write!(b, "<li>{}</li>", esc(d));
        }
        if let Some(v) = self.verify.last() {
            let t = &v.tally;
            let _ = write!(b, "<li>{} checked: {} intact, {} lost, missing or unreadable.</li>", size((t.ok + t.lost()) * 512), size(t.ok * 512), size(t.lost() * 512));
            if t.aliased > 0 {
                let _ = write!(b, "<li>{} of data read back contained data written to a different position: the drive wraps around and overwrites itself.</li>", size(t.aliased * 512));
            }
            if let Some(f) = &t.first_error {
                let _ = write!(b, "<li>First lost data at offset {} (0x{:x}).</li>", size(f.offset), f.offset);
            }
        }
        for w in &self.warnings {
            let _ = write!(b, "<li>{}</li>", esc(w));
        }
        b.push_str("</ul>");
        if let Some(p) = &self.probe {
            let _ = write!(b, "<h2>Capacity map</h2><p class=mut>Each cell is one test position across the claimed capacity, in order. Green: data stored and read back correctly. Red: data lost. Amber: data overwritten by a write to another position (wraparound).</p>{}", p.map_html());
        } else if let Some(v) = self.verify.last() {
            b.push_str("<h2>Test coverage</h2><p class=mut>Green: checked without errors. Red: lost or unreadable data. Amber: data from another position. Gray: not checked or detailed results unavailable.</p>");
            let span = self.tested.max(self.write.last().map_or(0, |w| w.end));
            b.push_str(&tally_map_html(v, span));
        }
        if transfer {
            b.push_str("<h2>Method</h2><p>Backup or restore with SHA-256 integrity checks. See the result for completion status and limitations. This operation does not test the drive's usable capacity.</p>");
        } else if self.command == "info" {
            b.push_str("<h2>Method</h2><p>Identity reported by the operating system. No storage test was run. Reported capacity has not been verified.</p>");
        } else if self.read_only || (self.target.mode == "raw" && (self.command == "scan" || (self.command == "bench" && self.bench.as_ref().is_some_and(|b| b.write.is_none())))) {
            b.push_str("<h2>Method</h2><p>This operation opens the disk for reading only. No test data is written. See the result for the amount read and any errors. Reads cannot confirm usable capacity. Other programs using the disk can affect speed.</p>");
        } else if identification_only {
            b.push_str("<h2>Method</h2><p>This report uses a supplied NAND ID or saved device response, the built-in database and the chosen response layout. No device was queried or tested. Matching parts and identity fields cannot confirm usable capacity or whether the drive is genuine.</p>");
        } else if self.verdict.status == Status::Refused {
            b.push_str("<h2>Method</h2><p>The operation was refused before testing. No test data was written and capacity was not checked.</p>");
        } else if self.probe.is_some() {
            b.push_str("<h2>Method</h2><p>The capacity probe writes and reads selected positions across the available area. It estimates a capacity boundary when the samples show wraparound or a failed tail. Space between samples is not verified; a full write-and-verify test is needed to check it.</p>");
        } else if self.command == "verify" {
            b.push_str("<h2>Method</h2><p>Existing test data was read and compared with its expected pattern. This verification did not write new test data. See the result for the area checked and any errors.</p>");
        } else if !self.write.is_empty() && !self.verify.is_empty() {
            b.push_str("<h2>Method</h2><p>Test data uses 512-byte sectors containing their position and a random per-run tag. Reads compare the stored data with that pattern. A sector holding another position's pattern shows that writes reached the wrong address. See the result for the amount written and checked; cancelled or failed runs may cover only part of the target.</p>");
        } else {
            b.push_str("<h2>Method</h2><p>See the operation and result for the work completed. This report does not establish that the drive's full claimed capacity stores data.</p>");
        }
        let _ = write!(b, "<p class=mut>Generated by {} on {}.</p></body></html>", esc(&self.tool), esc(&self.finished));
        b
    }

    /// Best estimate of the real capacity, if the test found the drive fake.
    pub fn real_capacity(&self) -> Option<u64> {
        if let Some(p) = &self.probe {
            return (p.finding == crate::probe::Finding::Fake && p.usable > 0).then_some(p.usable);
        }
        if self.verdict.status == Status::Fake
            && let Some(v) = self.verify.last() {
                if let Some(p) = v.tally.alias_period {
                    return Some(p);
                }
                return v.tally.first_error.as_ref().map(|f| f.offset);
            }
        None
    }
}

/// 200-cell map of good/bad regions from a verify tally.
fn tally_map_html(v: &VerifyResult, span: u64) -> String {
    let checked = v.tally.ok.saturating_add(v.tally.lost()).saturating_mul(512);
    let total = span.max(checked).max(1);
    let detailed = if v.tally.bad_ranges_truncated {
        v.tally.bad_ranges.last().map_or(0, |r| r.end).min(checked)
    } else { checked };
    let cells = 200u64;
    let mut s = String::from("<div class=map>");
    for i in 0..cells {
        let (a, b) = ((total as u128 * i as u128 / cells as u128) as u64, (total as u128 * (i + 1) as u128 / cells as u128) as u64);
        let bad = v.tally.bad_ranges.iter().find(|r| r.start < b && r.end > a);
        let cls = match bad.map(|r| r.kind.as_str()) {
            None if b <= detailed && b > a => "p-ok",
            None => "p-unknown",
            Some("aliased") => "p-alias",
            Some(_) => "p-bad",
        };
        let _ = write!(s, "<i class={cls} title=\"{}\"></i>", size(a));
    }
    s.push_str("</div>");
    s
}
