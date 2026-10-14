//! Progress, log and cancel flag shared between an engine and a front-end.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Log prefix of lines asking the user to do something; shown even with -q.
pub const ACTION: &str = "ACTION: ";

#[derive(Clone, Default)]
pub struct Ctx {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    cancel: AtomicBool,
    status: Mutex<Status>,
}

#[derive(Clone, Default)]
pub struct Status {
    pub phase: String,
    pub done: u64,
    pub total: u64,
    pub phase_start: Option<Instant>,
    /// Log lines; front-ends print the ones they haven't shown yet.
    pub log: Vec<String>,
    /// (position in GB, MB/s) of recent I/O for live graphs, per phase.
    pub series: Vec<(f64, f64)>,
    /// Set when the engine waits for the user (replug etc.).
    pub prompt: Option<String>,
    /// Stream offset the current phase starts at (`done` counts from here).
    pub base: u64,
    /// Bad regions found so far: (start, end, kind), for the live block map.
    pub marks: Vec<(u64, u64, String)>,
    /// (position in GB, °C) the drive reported during the job, for the speed graph.
    pub temps: Vec<(f64, f64)>,
}

impl Ctx {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.inner.cancel.store(true, Ordering::SeqCst);
    }
    pub fn cancelled(&self) -> bool {
        self.inner.cancel.load(Ordering::SeqCst)
    }
    pub fn reset_cancel(&self) {
        self.inner.cancel.store(false, Ordering::SeqCst);
    }
    pub fn phase(&self, name: &str, total: u64) {
        self.phase_at(name, 0, total);
    }
    pub fn phase_at(&self, name: &str, base: u64, total: u64) {
        let mut s = self.inner.status.lock().unwrap();
        s.base = base;
        s.phase = name.to_string();
        s.done = 0;
        s.total = total;
        s.phase_start = Some(Instant::now());
        s.series.clear();
    }
    pub fn progress(&self, done: u64) {
        self.inner.status.lock().unwrap().done = done;
    }
    pub fn sample(&self, pos: u64, mbps: f64) {
        let mut s = self.inner.status.lock().unwrap();
        s.series.push((pos as f64 / 1e9, mbps));
    }
    pub fn mark(&self, start: u64, end: u64, kind: &str) {
        let mut s = self.inner.status.lock().unwrap();
        if s.marks.len() < 10_000 {
            s.marks.push((start, end, kind.to_string()));
        }
    }
    pub fn temp(&self, pos: u64, celsius: u64) {
        self.inner.status.lock().unwrap().temps.push((pos as f64 / 1e9, celsius as f64));
    }
    pub fn log(&self, msg: impl Into<String>) {
        self.inner.status.lock().unwrap().log.push(msg.into());
    }
    pub fn prompt(&self, msg: Option<String>) {
        if let Some(m) = &msg {
            self.log(format!("{ACTION}{m}"));
        }
        self.inner.status.lock().unwrap().prompt = msg;
    }
    pub fn status(&self) -> Status {
        self.inner.status.lock().unwrap().clone()
    }
    /// Log lines from index `from` on, without cloning the whole status.
    pub fn log_since(&self, from: usize) -> Vec<String> {
        let s = self.inner.status.lock().unwrap();
        s.log.get(from..).map(|v| v.to_vec()).unwrap_or_default()
    }
}
