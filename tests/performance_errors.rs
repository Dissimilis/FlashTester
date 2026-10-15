use flashtester::{ctx::Ctx, dev::Dev, perf};
use std::io;

struct ShortRead;
impl Dev for ShortRead {
    fn len(&self) -> u64 { 8192 }
    fn read_at(&mut self, _off: u64, _buf: &mut [u8]) -> io::Result<usize> { Ok(0) }
    fn write_at(&mut self, _off: u64, _buf: &[u8]) -> io::Result<()> { panic!("read benchmark must not write") }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

#[test]
fn read_benchmark_rejects_early_eof() {
    let error = perf::seq_read(&mut ShortRead, 8192, 4096, &Ctx::new()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
fn invalid_iops_duration_never_opens_a_target() {
    for secs in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::MAX] {
        let result = perf::run_iops(&|_| panic!("invalid duration opened target"), 4096, false, secs, &Ctx::new());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
    assert!(perf::iops_duration(0.5).is_ok());
}

#[test]
fn verification_stops_diagnostic_rereads_when_cancelled() {
    struct CancelRead { ctx: Ctx, reads: usize }
    impl Dev for CancelRead {
        fn len(&self) -> u64 { 4096 }
        fn read_at(&mut self, _: u64, buf: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            buf.fill(0);
            if self.reads == 2 { self.ctx.cancel(); }
            Ok(buf.len())
        }
        fn write_at(&mut self, _: u64, _: &[u8]) -> io::Result<()> { panic!("verification must not write") }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    let ctx = Ctx::new();
    let mut dev = CancelRead { ctx: ctx.clone(), reads: 0 };
    let result = flashtester::engine::verify_pass(&mut dev, flashtester::pattern::Pattern::Seeded { tag: 7 }, 0, 4096, 4096, &ctx);
    assert_eq!(result.stop, flashtester::engine::Stop::Cancelled);
    assert_eq!(dev.reads, 2);
}

#[cfg(windows)]
#[test]
fn windows_iops_handles_have_independent_file_positions() {
    use std::{fs::OpenOptions, io::{Seek, SeekFrom}};
    use std::os::windows::fs::OpenOptionsExt;
    let path = std::env::temp_dir().join(format!("flashtester-handles-{}-{}", std::process::id(), flashtester::pattern::new_tag()));
    let mut first = OpenOptions::new().create_new(true).read(true).write(true).share_mode(3).open(&path).unwrap();
    first.set_len(8192).unwrap();
    let mut second = flashtester::dev::io_handle(&first, false).unwrap();
    first.seek(SeekFrom::Start(4096)).unwrap();
    assert_eq!(second.stream_position().unwrap(), 0);
    second.seek(SeekFrom::Start(8192)).unwrap();
    assert_eq!(first.stream_position().unwrap(), 4096);
    drop(second);
    drop(first);
    std::fs::remove_file(path).unwrap();
}
