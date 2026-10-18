//! Block I/O targets: a set of test files (file mode), a raw disk (see
//! `platform::RawDisk`) and an in-memory simulated drive for tests.
//!
//! Everything addresses one linear "test stream": in file mode stream offset
//! `n` lives in file `n / FILE_SIZE + 1` at `n % FILE_SIZE`; on a raw disk it is
//! the disk byte offset.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::fs::{File, OpenOptions};
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{FileExt, OpenOptionsExt};
#[cfg(windows)]
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const ALIGN: usize = 4096;
pub const MIB: u64 = 1 << 20;
pub const GIB: u64 = 1 << 30;
pub const FILE_SIZE: u64 = GIB;

/// Haiku's O_DIRECT (fcntl.h), which the libc crate does not define.
#[cfg(target_os = "haiku")]
const O_DIRECT: i32 = 0x0010_0000;
#[cfg(all(unix, not(target_os = "haiku")))]
use libc::O_DIRECT;

pub const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
pub const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;

/// Open bypassing the OS cache (unbuffered / O_DIRECT), shared with other
/// openers; `write_through` also makes each write durable before it returns.
pub fn direct(o: &mut OpenOptions, write_through: bool) -> &mut OpenOptions {
    #[cfg(windows)]
    {
        o.share_mode(3).custom_flags(FILE_FLAG_NO_BUFFERING | if write_through { FILE_FLAG_WRITE_THROUGH } else { 0 })
    }
    #[cfg(unix)]
    {
        o.custom_flags(O_DIRECT | libc::O_NOFOLLOW | if write_through { libc::O_DSYNC } else { 0 })
    }
}

/// Independent I/O handle for the already-open object; never resolve its path again.
pub fn io_handle(file: &File, write: bool) -> io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        use windows_sys::Win32::{Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE}, Storage::FileSystem::{ReOpenFile, FILE_SHARE_READ, FILE_SHARE_WRITE}};
        // Duplicating a synchronous Windows handle would serialize QD32 workers.
        let access = GENERIC_READ | if write { GENERIC_WRITE } else { 0 };
        let h = unsafe { ReOpenFile(file.as_raw_handle(), access, FILE_SHARE_READ | FILE_SHARE_WRITE, FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH) };
        if h == INVALID_HANDLE_VALUE { return Err(io::Error::last_os_error()); }
        Ok(unsafe { File::from_raw_handle(h) })
    }
    #[cfg(unix)]
    {
        let _ = write;
        file.try_clone()
    }
}

/// Refuse a path that is a link (symlink, junction or other reparse point). Test files
/// live on the drive under test; a crafted drive could otherwise point one at the
/// system disk, and writing or cleaning would follow it.
pub fn no_link(p: &Path) -> io::Result<()> {
    let m = match std::fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    #[cfg(windows)]
    let link = m.file_type().is_symlink() || std::os::windows::fs::MetadataExt::file_attributes(&m) & 0x400 != 0;
    #[cfg(unix)]
    let link = m.file_type().is_symlink();
    if link {
        return Err(io::Error::other(format!("{} is a link; refusing to follow it", p.display())));
    }
    Ok(())
}

/// Positioned read (does not use or move a shared file position).
pub fn pread(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    #[cfg(windows)]
    {
        f.seek_read(buf, off)
    }
    #[cfg(unix)]
    {
        f.read_at(buf, off)
    }
}

pub fn pwrite(f: &File, buf: &[u8], off: u64) -> io::Result<usize> {
    #[cfg(windows)]
    {
        f.seek_write(buf, off)
    }
    #[cfg(unix)]
    {
        f.write_at(buf, off)
    }
}

/// Writes all of `buf` at `off`; a write of zero bytes is an error.
pub fn pwrite_all(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = pwrite(f, &buf[done..], off + done as u64)?;
        if n == 0 { return Err(io::Error::new(io::ErrorKind::WriteZero, "disk stopped accepting writes")); }
        done += n;
    }
    Ok(())
}

/// Reads into `buf` at `off` until it is full or the data ends; returns bytes read.
pub fn pread_full(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        let n = pread(f, &mut buf[done..], off + done as u64)?;
        if n == 0 {
            break;
        }
        done += n;
    }
    Ok(done)
}

/// Page-aligned heap buffer, as unbuffered I/O requires.
pub struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for AlignedBuf {}
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    pub fn new(len: usize) -> Self {
        let len = len.max(ALIGN);
        let ptr = unsafe { alloc_zeroed(Layout::from_size_align(len, ALIGN).unwrap()) };
        assert!(!ptr.is_null(), "out of memory");
        AlignedBuf { ptr, len }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, Layout::from_size_align(self.len, ALIGN).unwrap()) }
    }
}

impl std::ops::Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl std::ops::DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

pub fn is_disk_full(e: &io::Error) -> bool {
    #[cfg(windows)]
    let codes: &[i32] = &[112, 39]; // DISK_FULL, HANDLE_DISK_FULL
    #[cfg(unix)]
    let codes: &[i32] = &[libc::ENOSPC, libc::EDQUOT];
    e.raw_os_error().is_some_and(|c| codes.contains(&c))
}

/// The device went away (unplugged, reset, reader lost the card).
pub fn is_disconnect(e: &io::Error) -> bool {
    // ERROR_FILE_NOT_FOUND, PATH_NOT_FOUND, INVALID_HANDLE, NOT_READY, DEV_NOT_EXIST,
    // NO_SUCH_DEVICE, FILE_INVALID, DEVICE_NOT_CONNECTED, DEVICE_REMOVED
    #[cfg(windows)]
    let codes: &[i32] = &[2, 3, 6, 21, 55, 433, 1006, 1167, 1617];
    // A pulled USB disk gives ENODEV/ENXIO; ENOENT once its mount is gone.
    #[cfg(unix)]
    let codes: &[i32] = &[libc::ENODEV, libc::ENXIO, libc::ENOENT, libc::ESHUTDOWN];
    e.raw_os_error().is_some_and(|c| codes.contains(&c))
}

pub trait Dev: Send {
    /// Bytes available for the test stream.
    fn len(&self) -> u64;
    fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()>;
    /// Reads up to `buf.len()`; returns bytes read (short only at the end of data).
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize>;
    fn flush(&mut self) -> io::Result<()>;
    /// Granularity reads and writes must be aligned to.
    fn align(&self) -> usize {
        ALIGN
    }
    /// Whether `[off, off + len)` may be written at all (free-space mode can only
    /// reach the clusters it allocated).
    fn usable(&self, _off: u64, _len: u64) -> bool {
        true
    }
}

/// Test files `00001.ftd`, `00002.ftd`, ... of `FILE_SIZE` bytes each, opened unbuffered.
pub struct FileSet {
    pub dir: PathBuf,
    pub ext: &'static str,
    pub file_size: u64,
    pub len: u64,
    /// (file index, handle, opened for writing)
    open: Option<(u64, File, bool)>,
    /// File system the folder is on; every file opened must be on it too.
    fs: Option<u64>,
}

impl FileSet {
    pub fn new(dir: &Path, len: u64) -> Self {
        FileSet { dir: dir.to_path_buf(), ext: "ftd", file_size: FILE_SIZE, len, open: None, fs: crate::platform::fs_id_path(dir).ok() }
    }

    /// h2testw's `1.h2w`, `2.h2w`, ... in `dir`.
    pub fn h2testw(dir: &Path, len: u64) -> Self {
        FileSet { dir: dir.to_path_buf(), ext: "h2w", file_size: FILE_SIZE, len, open: None, fs: crate::platform::fs_id_path(dir).ok() }
    }

    pub fn path(&self, idx: u64) -> PathBuf {
        if self.ext == "h2w" {
            self.dir.join(format!("{}.h2w", idx + 1))
        } else {
            self.dir.join(format!("{:05}.{}", idx + 1, self.ext))
        }
    }

    fn file(&mut self, idx: u64, create: bool) -> io::Result<&File> {
        let reuse = matches!(&self.open, Some((i, _, w)) if *i == idx && (*w || !create));
        if !reuse {
            self.close()?;
            no_link(&self.path(idx))?;
            let mut o = OpenOptions::new();
            o.read(true);
            direct(&mut o, false);
            if create {
                o.write(true).create(true);
            }
            let f = o.open(self.path(idx))?;
            // A swapped folder or an unmounted drive must not send writes elsewhere.
            if let Some(id) = self.fs {
                if crate::platform::fs_id(&f)? != id {
                    return Err(io::Error::other(format!("{} is not on the drive under test; stopping", self.path(idx).display())));
                }
            }
            self.open = Some((idx, f, create));
        }
        Ok(&self.open.as_ref().unwrap().1)
    }

    /// Total bytes in existing data files (stops at the first missing file).
    pub fn existing_len(dir: &Path, ext: &str) -> (u64, u64) {
        let fs = FileSet { dir: dir.to_path_buf(), ext: if ext == "h2w" { "h2w" } else { "ftd" }, file_size: FILE_SIZE, len: 0, open: None, fs: None };
        let (mut total, mut n) = (0, 0);
        while let Ok(m) = std::fs::metadata(fs.path(n)) {
            total += m.len();
            n += 1;
        }
        (total, n)
    }

    pub fn close(&mut self) -> io::Result<()> {
        if let Some((_, f, true)) = self.open.take() {
            f.sync_all()?;
        }
        Ok(())
    }
}

impl Dev for FileSet {
    fn len(&self) -> u64 {
        self.len
    }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        let fsz = self.file_size;
        let (idx, pos) = (off / fsz, off % fsz);
        debug_assert!(pos + buf.len() as u64 <= fsz);
        let f = self.file(idx, true)?;
        let n = pwrite(f, buf, pos)?;
        if n != buf.len() {
            return Err(io::Error::from_raw_os_error(112));
        }
        Ok(())
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        let fsz = self.file_size;
        let (idx, pos) = (off / fsz, off % fsz);
        let dir_ok = self.dir.is_dir();
        let f = match self.file(idx, false) {
            Ok(f) => f,
            // A missing file is missing data (reported as such), not a lost device.
            Err(e) if e.kind() == io::ErrorKind::NotFound && dir_ok => return Ok(0),
            Err(e) => return Err(e),
        };
        pread_full(f, buf, pos)
    }
    fn flush(&mut self) -> io::Result<()> {
        if let Some((_, f, true)) = &self.open {
            f.sync_all()?;
        }
        Ok(())
    }
}

/// Simulated drive for tests: a real region of `real` bytes; what happens beyond
/// it depends on `mode`.
pub struct MemDev {
    pub data: Vec<u8>,
    pub claimed: u64,
    pub mode: SimMode,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SimMode {
    Genuine,
    /// Addresses wrap modulo the real size (address bits ignored).
    Wrap,
    /// Writes beyond the real size are dropped; reads return zeros.
    Drop,
    /// Every write is silently ignored.
    ReadOnly,
}

impl MemDev {
    pub fn new(real: u64, claimed: u64, mode: SimMode) -> Self {
        MemDev { data: vec![0; real as usize], claimed, mode }
    }
    fn map(&self, off: u64) -> Option<usize> {
        let real = self.data.len() as u64;
        match self.mode {
            SimMode::Wrap => Some((off % real) as usize),
            _ if off < real => Some(off as usize),
            _ => None,
        }
    }
}

impl Dev for MemDev {
    fn len(&self) -> u64 {
        self.claimed
    }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        if self.mode == SimMode::ReadOnly {
            return Ok(());
        }
        for (i, chunk) in buf.chunks(512).enumerate() {
            if let Some(p) = self.map(off + i as u64 * 512) {
                self.data[p..p + chunk.len()].copy_from_slice(chunk);
            }
        }
        Ok(())
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        for (i, chunk) in buf.chunks_mut(512).enumerate() {
            match self.map(off + i as u64 * 512) {
                Some(p) => chunk.copy_from_slice(&self.data[p..p + chunk.len()]),
                None => chunk.fill(0),
            }
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
