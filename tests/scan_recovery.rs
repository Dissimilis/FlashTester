use flashtester::{ctx::Ctx, dev::Dev, engine::Stop, scan::run_scan};
use std::io;

const UNIT: usize = 4096;
const BLOCK: usize = 128 * UNIT;

struct RecoveringDisk {
    bad_units: u64,
    small_reads: u64,
    cancel_after: Option<(u64, Ctx)>,
}

impl Dev for RecoveringDisk {
    fn len(&self) -> u64 { BLOCK as u64 }
    fn write_at(&mut self, _: u64, _: &[u8]) -> io::Result<()> { panic!("scan must never write") }
    fn flush(&mut self) -> io::Result<()> { panic!("scan must never flush") }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        if buf.len() > UNIT {
            return Err(io::Error::other("large read failed"));
        }
        self.small_reads += 1;
        if let Some((after, ctx)) = &self.cancel_after {
            if self.small_reads == *after { ctx.cancel(); }
        }
        if off < self.bad_units * UNIT as u64 {
            return Err(io::Error::other("unreadable block"));
        }
        buf.fill(0);
        Ok(buf.len())
    }
}

#[test]
fn recovered_reads_count_as_read_without_erasing_initial_error() {
    let mut dev = RecoveringDisk { bad_units: 0, small_reads: 0, cancel_after: None };
    let result = run_scan(&mut dev, None, BLOCK, &Ctx::new());
    assert_eq!(result.stop, Stop::Done);
    assert_eq!(result.stats.bytes, BLOCK as u64);
    assert_eq!(result.scanned, BLOCK as u64);
    assert_eq!(result.bad_bytes, 0);
    assert!(result.stats.seconds > 0.0 && result.stats.mb_s > 0.0);
    assert_eq!(result.events.len(), 1);
    assert_eq!(result.events[0].kind, "io-error");
    assert!(result.summary_lines().iter().any(|s| s.contains("original errors remain")));
}

#[test]
fn scan_continues_after_64_bad_blocks() {
    let mut dev = RecoveringDisk { bad_units: 64, small_reads: 0, cancel_after: None };
    let result = run_scan(&mut dev, None, BLOCK, &Ctx::new());
    assert_eq!(dev.small_reads, 128);
    assert_eq!(result.bad_bytes, 64 * UNIT as u64);
    assert_eq!(result.stats.bytes, 64 * UNIT as u64);
    assert_eq!(result.scanned, BLOCK as u64);
    assert_eq!(result.bad_ranges.len(), 1);
    assert_eq!(result.bad_ranges[0].end, 64 * UNIT as u64);
}

#[test]
fn cancellation_keeps_partial_fallback_coverage() {
    let ctx = Ctx::new();
    let mut dev = RecoveringDisk { bad_units: 0, small_reads: 0, cancel_after: Some((10, ctx.clone())) };
    let result = run_scan(&mut dev, None, BLOCK, &ctx);
    assert_eq!(result.stop, Stop::Cancelled);
    assert_eq!(result.scanned, 10 * UNIT as u64);
    assert_eq!(result.stats.bytes, 10 * UNIT as u64);
    assert_eq!(result.bad_bytes, 0);
}

#[test]
fn sub_block_scan_is_rejected_without_reads() {
    let mut dev = RecoveringDisk { bad_units: 0, small_reads: 0, cancel_after: None };
    let result = run_scan(&mut dev, Some(1), BLOCK, &Ctx::new());
    assert!(matches!(result.stop, Stop::Error(_)));
    assert_eq!(result.stats.bytes, 0);
    assert_eq!(result.scanned, 0);
    assert_eq!(dev.small_reads, 0);
}
