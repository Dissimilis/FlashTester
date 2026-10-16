//! File-tree backups and whole-disk images. A complete manifest is committed only
//! after read-back verification. Restore validates every input before writing.
use crate::{ctx::Ctx, dev::{self, AlignedBuf, Dev, MIB}, fulltest::Where, platform::{self, RawDisk}, report::{Report, Status}};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};
use std::{collections::HashSet, fs::{self, File, OpenOptions}, io::{self, Read, Seek, SeekFrom, Write}, path::{Component, Path, PathBuf}, time::{Instant, SystemTime}};

const MANIFEST: &str = "backup.json";
const CHUNK: usize = 4 * MIB as usize;
const MAX_ENTRIES: usize = 1_000_000;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Entry {
    path: String,
    directory: bool,
    size: u64,
    modified: Option<SystemTime>,
    sha256: String,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: String,
    version: u32,
    complete: bool,
    created: String,
    bytes: u64,
    sector_size: u32,
    sha256: String,
    entries: Vec<Entry>,
    excluded: Vec<String>,
}

impl Manifest {
    fn new(format: &str) -> Self {
        Self { format: format.into(), version: 1, complete: false, created: crate::engine::now(), bytes: 0, sector_size: 0, sha256: String::new(), entries: vec![], excluded: vec![] }
    }
}

fn error(s: impl Into<String>) -> io::Error { io::Error::other(s.into()) }
fn cancelled(ctx: &Ctx) -> io::Result<()> {
    if ctx.cancelled() { Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled; the output is incomplete.")) } else { Ok(()) }
}

/// Refuse links in every existing component, including junctions on Windows.
fn plain_path(path: &Path) -> io::Result<PathBuf> {
    let path = std::path::absolute(path)?;
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(error("Use a path without '..'."));
    }
    for p in path.ancestors() { dev::no_link(p)?; }
    Ok(path)
}

fn new_path(path: &Path) -> io::Result<PathBuf> {
    let path = plain_path(path)?;
    match fs::symlink_metadata(&path) {
        Ok(_) => return Err(error(format!("{} already exists. Choose a new path.", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e),
    }
    if !path.parent().is_some_and(Path::is_dir) { return Err(error("The destination's parent folder must already exist.")); }
    Ok(path)
}

/// Names must remain ordinary relative paths on every supported platform.
fn relative(path: &str) -> io::Result<PathBuf> {
    if path.split('/').next().is_some_and(|s| s.eq_ignore_ascii_case(".flashtester-restore-incomplete")) || path.is_empty() || path.split('/').any(|s| {
        let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
        s.is_empty() || s == "." || s == ".." || s.ends_with(['.', ' '])
            || s.chars().any(|c| c < ' ' || "\\:*?\"<>|".contains(c))
            || ["CON", "PRN", "AUX", "NUL", "CLOCK$", "CONIN$", "CONOUT$"].contains(&stem.as_str())
            || ["COM¹", "COM²", "COM³", "LPT¹", "LPT²", "LPT³"].contains(&stem.as_str())
            || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit())
    }) { return Err(error(format!("Unsupported backup path: {path:?}. Use a whole-disk image for names that are not portable."))); }
    let p = PathBuf::from(path);
    if p.components().any(|c| !matches!(c, Component::Normal(_))) { return Err(error("Backup contains a path outside its file tree.")); }
    Ok(p)
}

fn open_read(path: &Path) -> io::Result<File> {
    plain_path(path)?;
    let mut o = OpenOptions::new();
    o.read(true);
    #[cfg(windows)] {
        use std::os::windows::fs::OpenOptionsExt;
        o.share_mode(1).custom_flags(0x0020_0000); // no concurrent writers/deletes; open reparse point itself
    }
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let f = o.open(path)?;
    if !f.metadata()?.is_file() { return Err(error(format!("{} is not a regular file.", path.display()))); }
    dev::no_link(path)?;
    Ok(f)
}

fn create_file(path: &Path, fs_id: u64) -> io::Result<File> {
    plain_path(path)?;
    let mut o = OpenOptions::new();
    o.read(true).write(true).create_new(true);
    #[cfg(windows)] {
        use std::os::windows::fs::OpenOptionsExt;
        o.share_mode(1);
    }
    let f = o.open(path)?;
    if platform::fs_id(&f)? != fs_id { return Err(error("Destination volume changed; stopping.")); }
    Ok(f)
}

fn write_manifest(file: &mut File, m: &Manifest) -> io::Result<()> {
    let data = serde_json::to_vec_pretty(m)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&data)?;
    file.set_len(data.len() as u64)?;
    file.sync_all()
}

// Unix file fsync does not persist directory entries. Windows uses file
// handle flushes; directory handles do not support FlushFileBuffers.
fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        plain_path(path)?;
        OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(path)?.sync_all()?;
    }
    #[cfg(not(unix))] let _ = path;
    Ok(())
}

fn load_manifest(path: &Path, kind: &str) -> io::Result<Manifest> {
    let f = open_read(path)?;
    if f.metadata()?.len() > 128 * MIB { return Err(error("Backup manifest is too large.")); }
    let m: Manifest = serde_json::from_reader(f.take(128 * MIB + 1))?;
    validate_manifest(&m, kind)?;
    Ok(m)
}

fn validate_manifest(m: &Manifest, kind: &str) -> io::Result<()> {
    if m.version != 1 || m.format != kind || !m.complete { return Err(error("This backup is incomplete or has an unsupported format.")); }
    if m.entries.len() > MAX_ENTRIES { return Err(error("Too many entries in backup manifest.")); }
    let mut seen = HashSet::new();
    let mut total = 0u64;
    for e in &m.entries {
        relative(&e.path)?;
        // Also refuse collisions when a backup moves to a case-insensitive file system.
        if !seen.insert(e.path.to_lowercase()) { return Err(error("Backup contains duplicate or case-colliding paths.")); }
        if !e.directory {
            valid_hash(&e.sha256)?;
            total = total.checked_add(e.size).ok_or_else(|| error("Backup size overflow."))?;
        }
    }
    let files: HashSet<_> = m.entries.iter().filter(|e| !e.directory).map(|e| e.path.to_lowercase()).collect();
    let directories: HashSet<_> = m.entries.iter().filter(|e| e.directory).map(|e| e.path.to_lowercase()).collect();
    for e in &m.entries {
        for parent in relative(&e.path)?.ancestors().skip(1) {
            let name = parent.to_string_lossy().replace('\\', "/").to_lowercase();
            if files.contains(&name) || (!name.is_empty() && !directories.contains(&name)) { return Err(error("Backup has a missing or invalid parent folder.")); }
        }
    }
    if kind == "flashtester-files" && total != m.bytes { return Err(error("Backup size does not match its manifest.")); }
    Ok(())
}

fn valid_hash(hash: &str) -> io::Result<()> {
    if hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) { Err(error("Invalid SHA-256 in backup manifest.")) } else { Ok(()) }
}

fn hash_file(f: &mut File, len: u64, ctx: &Ctx, base: u64) -> io::Result<String> {
    f.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut buf = vec![0; CHUNK];
    let mut done = 0;
    while done < len {
        cancelled(ctx)?;
        let n = (len - done).min(buf.len() as u64) as usize;
        f.read_exact(&mut buf[..n])?;
        hash.update(&buf[..n]);
        done += n as u64;
        ctx.progress(base + done);
    }
    if f.metadata()?.len() != len { return Err(error("File length changed during verification.")); }
    Ok(format!("{:x}", hash.finalize()))
}

fn scan_tree(root: &Path, ctx: &Ctx, exclude_os: bool) -> io::Result<(Vec<Entry>, Vec<String>)> {
    let root = plain_path(root)?;
    let fs_id = platform::fs_id_path(&root)?;
    let mut pending = vec![root.clone()];
    let mut entries = vec![];
    let mut excluded = vec![];
    let mut names = HashSet::new();
    while let Some(dir) = pending.pop() {
        cancelled(ctx)?;
        plain_path(&dir)?;
        if platform::fs_id_path(&dir)? != fs_id { return Err(error("A folder is on another volume; back it up separately.")); }
        for child in fs::read_dir(&dir)? {
            cancelled(ctx)?;
            let child = child?;
            let p = child.path();
            let rel = p.strip_prefix(&root).map_err(|_| error("Path escaped source folder."))?;
            let name = rel.to_str().ok_or_else(|| error("A filename is not Unicode; use a whole-disk image."))?.replace(std::path::MAIN_SEPARATOR, "/");
            if exclude_os && dir == root && ["$recycle.bin", "system volume information"].contains(&name.to_lowercase().as_str()) {
                excluded.push(name);
                continue;
            }
            relative(&name)?;
            if !names.insert(name.to_lowercase()) { return Err(error("Source has case-colliding names; use a whole-disk image.")); }
            dev::no_link(&p)?;
            let meta = fs::symlink_metadata(&p)?;
            if !meta.is_file() && !meta.is_dir() { return Err(error(format!("{} is not a regular file or folder; use a whole-disk image.", p.display()))); }
            if entries.len() >= MAX_ENTRIES { return Err(error("Too many files; back up smaller folders separately.")); }
            entries.push(Entry { path: name, directory: meta.is_dir(), size: if meta.is_file() { meta.len() } else { 0 }, modified: if meta.is_file() { meta.modified().ok() } else { None }, sha256: String::new() });
            if meta.is_dir() { pending.push(p); }
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    excluded.sort();
    Ok((entries, excluded))
}

fn total_bytes(entries: &[Entry]) -> io::Result<u64> {
    entries.iter().try_fold(0u64, |n, e| n.checked_add(e.size).ok_or_else(|| error("Backup size overflow.")))
}

fn ensure_space(parent: &Path, bytes: u64) -> io::Result<()> {
    if platform::free_space(parent)? < bytes { return Err(error("There is not enough free space at the destination.")); }
    Ok(())
}

fn zip_error(e: zip::result::ZipError) -> io::Error { error(format!("ZIP archive: {e}")) }

fn archive_manifest(archive: &mut ZipArchive<File>) -> io::Result<Manifest> {
    if archive.len() > MAX_ENTRIES + 1 { return Err(error("Too many entries in archive.")); }
    let mut f = archive.by_name(MANIFEST).map_err(zip_error)?;
    if f.size() > 128 * MIB { return Err(error("Backup manifest is too large.")); }
    let m: Manifest = serde_json::from_reader((&mut f).take(128 * MIB + 1))?;
    drop(f);
    validate_manifest(&m, "flashtester-files")?;
    if archive.len() != m.entries.len() + 1 { return Err(error("Archive entries do not match the manifest.")); }
    Ok(m)
}

/// Both validation and extraction stream only manifest-listed files. Never use
/// ZipArchive::extract on an untrusted archive or follow its embedded paths.
fn read_archive(archive: &mut ZipArchive<File>, m: &Manifest, output: Option<&Path>, ctx: &Ctx) -> io::Result<()> {
    let target_fs = output.map(platform::fs_id_path).transpose()?;
    let mut done = 0;
    let mut buf = vec![0; CHUNK];
    for e in &m.entries {
        cancelled(ctx)?;
        let rel = relative(&e.path)?;
        let name = format!("files/{}{}", e.path, if e.directory { "/" } else { "" });
        let mut entry = archive.by_name(&name).map_err(zip_error)?;
        if entry.is_symlink() || entry.is_dir() != e.directory || entry.size() != e.size { return Err(error(format!("Invalid archive entry: {}", e.path))); }
        let target = output.map(|p| p.join(&rel));
        if e.directory {
            if let Some(p) = target {
                plain_path(&p)?;
                fs::create_dir(&p)?;
                if platform::fs_id_path(&p)? != target_fs.unwrap() { return Err(error("Destination folder changed volume.")); }
            }
            continue;
        }
        let mut out = target.as_ref().map(|p| create_file(p, target_fs.unwrap())).transpose()?;
        let mut hash = Sha256::new();
        let mut off = 0;
        while off < e.size {
            cancelled(ctx)?;
            let n = (e.size - off).min(buf.len() as u64) as usize;
            entry.read_exact(&mut buf[..n])?;
            if let Some(f) = &mut out { f.write_all(&buf[..n])?; }
            hash.update(&buf[..n]);
            off += n as u64;
            ctx.progress(done + off);
        }
        if entry.read(&mut buf[..1])? != 0 || !format!("{:x}", hash.finalize()).eq_ignore_ascii_case(&e.sha256) { return Err(error(format!("Checksum mismatch: {}", e.path))); }
        if let Some(f) = &mut out {
            if let Some(modified) = e.modified { f.set_modified(modified)?; }
            f.sync_all()?;
        }
        done += e.size;
    }
    Ok(())
}

fn verify_restored(root: &Path, entries: &[Entry], ctx: &Ctx) -> io::Result<()> {
    let fs_id = platform::fs_id_path(root)?;
    let mut done = 0;
    for e in entries.iter().filter(|e| e.directory) {
        cancelled(ctx)?;
        let p = plain_path(&root.join(relative(&e.path)?))?;
        if !fs::symlink_metadata(&p)?.is_dir() || platform::fs_id_path(&p)? != fs_id {
            return Err(error(format!("Restored folder is missing or changed: {}", e.path)));
        }
    }
    for e in entries.iter().filter(|e| !e.directory) {
        cancelled(ctx)?;
        let mut f = open_read(&root.join(relative(&e.path)?))?;
        if platform::fs_id(&f)? != fs_id || !hash_file(&mut f, e.size, ctx, done)?.eq_ignore_ascii_case(&e.sha256) { return Err(error(format!("Restored file checksum mismatch: {}", e.path))); }
        done += e.size;
    }
    Ok(())
}

fn backup_files(src: &Path, output: &Path, ctx: &Ctx, exclude_os: bool) -> io::Result<Manifest> {
    let output = new_path(output)?;
    let id = platform::fs_id_path(output.parent().unwrap())?;
    let source_fs = platform::fs_id_path(src)?;
    ctx.phase("Reading file list", 0);
    let (entries, excluded) = scan_tree(src, ctx, exclude_os)?;
    let mut m = Manifest::new("flashtester-files");
    m.bytes = total_bytes(&entries)?;
    m.entries = entries;
    m.excluded = excluded;
    // Allow archive headers and worst-case deflate expansion without depending
    // on an optimistic compression ratio.
    let required = m.bytes.checked_add(m.bytes / 100).and_then(|n| n.checked_add(m.entries.len() as u64 * 4096 + MIB)).ok_or_else(|| error("Backup size overflow."))?;
    ensure_space(output.parent().unwrap(), required)?;
    cancelled(ctx)?;
    if platform::fs_id_path(src)? != source_fs { return Err(error("Source volume changed during backup.")); }
    let f = create_file(&output, id)?;
    let mut zip = ZipWriter::new(f);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).compression_level(Some(1)).large_file(true);
    ctx.phase("Creating backup archive", m.bytes);
    let mut done = 0;
    let mut buf = vec![0; CHUNK];
    for e in &mut m.entries {
        cancelled(ctx)?;
        let name = format!("files/{}", e.path);
        if e.directory {
            zip.add_directory(format!("{name}/"), options).map_err(zip_error)?;
            continue;
        }
        let mut input = open_read(&src.join(relative(&e.path)?))?;
        let meta = input.metadata()?;
        if platform::fs_id(&input)? != source_fs || meta.len() != e.size || meta.modified().ok() != e.modified { return Err(error(format!("{} changed since the backup started.", e.path))); }
        zip.start_file(name, options).map_err(zip_error)?;
        let mut hash = Sha256::new();
        let mut off = 0;
        while off < e.size {
            cancelled(ctx)?;
            let n = (e.size - off).min(buf.len() as u64) as usize;
            input.read_exact(&mut buf[..n])?;
            zip.write_all(&buf[..n])?;
            hash.update(&buf[..n]);
            off += n as u64;
            ctx.progress(done + off);
        }
        let after = input.metadata()?;
        if after.len() != meta.len() || after.modified().ok() != meta.modified().ok() { return Err(error(format!("{} changed while being copied.", e.path))); }
        e.sha256 = format!("{:x}", hash.finalize());
        done += e.size;
    }
    let (current, excluded) = scan_tree(src, ctx, exclude_os)?;
    let mut original = m.entries.clone();
    for e in &mut original { e.sha256.clear(); }
    if current != original || excluded != m.excluded { return Err(error("Source files changed during backup. Close programs using the drive and retry.")); }
    cancelled(ctx)?;
    let f = zip.finish().map_err(zip_error)?;
    f.sync_all()?;
    sync_directory(output.parent().unwrap())?;
    let mut archive = ZipArchive::new(f).map_err(zip_error)?;
    ctx.phase("Checking backup archive", m.bytes);
    read_archive(&mut archive, &m, None, ctx)?;
    cancelled(ctx)?;
    // Commit the manifest only after verifying the payload. Before this point,
    // even a structurally valid partial ZIP is not a restorable backup.
    m.complete = true;
    let manifest_data = serde_json::to_vec(&m)?;
    if manifest_data.len() as u64 > 128 * MIB { return Err(error("Manifest is too large; back up smaller folders separately.")); }
    let mut zip = ZipWriter::new_append(archive.into_inner()).map_err(zip_error)?;
    zip.start_file(MANIFEST, SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).map_err(zip_error)?;
    zip.write_all(&manifest_data)?;
    zip.finish().map_err(zip_error)?.sync_all()?;
    sync_directory(output.parent().unwrap())?;
    Ok(m)
}

fn restore_files(input: &Path, output: &Path, ctx: &Ctx) -> io::Result<Manifest> {
    let output = new_path(output)?;
    let target_fs = platform::fs_id_path(output.parent().unwrap())?;
    let mut archive = ZipArchive::new(open_read(input)?).map_err(zip_error)?;
    let m = archive_manifest(&mut archive)?;
    // Reserve space for directory entries, allocation rounding and the marker.
    let required = m.bytes.checked_add(m.entries.len() as u64 * 4096 + MIB).ok_or_else(|| error("Backup size overflow."))?;
    ensure_space(output.parent().unwrap(), required)?;
    ctx.phase("Checking backup before restore", m.bytes);
    read_archive(&mut archive, &m, None, ctx)?;
    cancelled(ctx)?;
    if platform::fs_id_path(output.parent().unwrap())? != target_fs { return Err(error("Destination volume changed before restore.")); }
    fs::create_dir(&output)?;
    if platform::fs_id_path(&output)? != target_fs { return Err(error("Destination volume changed before restore.")); }
    // A marker remains on failure/cancellation. Never erase partial output.
    let marker = output.join(".flashtester-restore-incomplete");
    let marker_file = create_file(&marker, target_fs)?;
    marker_file.sync_all()?;
    drop(marker_file);
    sync_directory(&output)?;
    sync_directory(output.parent().unwrap())?;
    ctx.phase("Restoring files", m.bytes);
    read_archive(&mut archive, &m, Some(&output), ctx)?;
    ctx.phase("Checking restored files", m.bytes);
    verify_restored(&output, &m.entries, ctx)?;
    // Persist child names before their parents and before removing the marker.
    let mut directories: Vec<_> = m.entries.iter().filter(|e| e.directory).collect();
    directories.sort_by_key(|e| std::cmp::Reverse(e.path.matches('/').count()));
    for e in directories { cancelled(ctx)?; sync_directory(&output.join(relative(&e.path)?))?; }
    sync_directory(&output)?;
    cancelled(ctx)?;
    fs::remove_file(marker)?;
    sync_directory(&output)?;
    Ok(m)
}

fn image_manifest(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_os_string();
    p.push(".json");
    PathBuf::from(p)
}

fn save_image(dev: &mut dyn Dev, output: &mut File, bytes: u64, ctx: &Ctx) -> io::Result<String> {
    let mut buf = AlignedBuf::new(CHUNK);
    let mut hash = Sha256::new();
    let mut off = 0;
    while off < bytes {
        cancelled(ctx)?;
        let n = (bytes - off).min(buf.len() as u64) as usize;
        if dev.read_at(off, &mut buf[..n])? != n { return Err(error(format!("Short disk read at byte {off}; image is incomplete."))); }
        output.write_all(&buf[..n])?;
        hash.update(&buf[..n]);
        off += n as u64;
        ctx.progress(off);
    }
    output.sync_all()?;
    Ok(format!("{:x}", hash.finalize()))
}

fn check_image(input: &mut File, m: &Manifest, ctx: &Ctx) -> io::Result<Vec<[u8; 32]>> {
    input.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut chunks = vec![];
    let mut buf = vec![0; CHUNK];
    let mut off = 0;
    while off < m.bytes {
        cancelled(ctx)?;
        let n = (m.bytes - off).min(buf.len() as u64) as usize;
        input.read_exact(&mut buf[..n])?;
        hash.update(&buf[..n]);
        chunks.push(Sha256::digest(&buf[..n]).into());
        off += n as u64;
        ctx.progress(off);
    }
    if input.metadata()?.len() != m.bytes || !format!("{:x}", hash.finalize()).eq_ignore_ascii_case(&m.sha256) {
        return Err(error("Image checksum mismatch. Nothing was restored."));
    }
    Ok(chunks)
}

fn restore_image(dev: &mut dyn Dev, input: &mut File, m: &Manifest, chunks: &[[u8; 32]], ctx: &Ctx) -> io::Result<()> {
    if m.bytes == 0 || m.bytes > dev.len() || m.bytes % u64::from(m.sector_size.max(1)) != 0 { return Err(error("The image does not fit the destination disk.")); }
    if chunks.len() as u64 != m.bytes.div_ceil(CHUNK as u64) { return Err(error("Missing image validation data.")); }
    input.seek(SeekFrom::Start(0))?;
    let mut buf = AlignedBuf::new(CHUNK);
    let mut hash = Sha256::new();
    ctx.phase("Restoring disk image", m.bytes);
    let mut off = 0;
    while off < m.bytes {
        cancelled(ctx)?;
        let n = (m.bytes - off).min(buf.len() as u64) as usize;
        input.read_exact(&mut buf[..n])?;
        let digest: [u8; 32] = Sha256::digest(&buf[..n]).into();
        if digest != chunks[(off / CHUNK as u64) as usize] { return Err(error("The image changed during restore; stopped before writing the changed chunk. The destination may be incomplete.")); }
        hash.update(&buf[..n]);
        dev.write_at(off, &buf[..n])?;
        off += n as u64;
        ctx.progress(off);
    }
    dev.flush()?;
    if !format!("{:x}", hash.finalize()).eq_ignore_ascii_case(&m.sha256) { return Err(error("The image changed during restore; the destination is incomplete.")); }
    ctx.phase("Checking restored disk", m.bytes);
    let mut hash = Sha256::new();
    off = 0;
    while off < m.bytes {
        cancelled(ctx)?;
        let n = (m.bytes - off).min(buf.len() as u64) as usize;
        if dev.read_at(off, &mut buf[..n])? != n { return Err(error("Short read while checking restored disk.")); }
        hash.update(&buf[..n]);
        off += n as u64;
        ctx.progress(off);
    }
    if !format!("{:x}", hash.finalize()).eq_ignore_ascii_case(&m.sha256) { return Err(error("Restored disk does not match the image.")); }
    Ok(())
}

/// Backup storage must be on a different physical disk, including other partitions.
fn separate_disk(w: &Where, path: &Path) -> io::Result<()> {
    let p = plain_path(path)?;
    let existing = if p.exists() { p.as_path() } else { p.parent().ok_or_else(|| error("Missing parent folder."))? };
    let v = platform::volume_of_path(existing)?;
    if v.extents.is_empty() { return Err(error("Cannot identify the disk holding the backup.")); }
    let ids: Vec<&str> = match w {
        Where::Disk(d) => vec![d.id.as_str()],
        Where::Dir { vol, .. } => vol.extents.iter().map(|e| e.0.as_str()).collect(),
    };
    if ids.is_empty() || v.extents.iter().any(|e| ids.contains(&e.0.as_str())) { return Err(error("Keep the backup on a different physical disk from the source or restore destination.")); }
    Ok(())
}

pub fn run(w: &Where, path: &Path, restore: bool, folder: Option<&Path>, ctx: &Ctx) -> Report {
    let start = Instant::now();
    let mut rep = Report::new(if restore { "restore" } else { "backup" }, w.target());
    rep.read_only = !restore;
    let result = (|| -> io::Result<Manifest> {
        let extension = if matches!(w, Where::Disk(_)) { "img" } else { "zip" };
        if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case(extension)) {
            return Err(error(format!("Choose a .{extension} file for this backup mode.")));
        }
        separate_disk(w, path)?;
        match w {
            Where::Dir { root, vol, .. } => {
                if restore {
                    let name = folder.ok_or_else(|| error("Choose a new restore folder."))?;
                    let name = name.to_str().ok_or_else(|| error("Restore folder must be Unicode."))?;
                    let dst = root.join(relative(name)?);
                    restore_files(path, &dst, ctx)
                } else {
                    let is_root = vol.mount_points.iter().any(|m| fs::canonicalize(m).ok() == fs::canonicalize(root).ok());
                    backup_files(root, path, ctx, cfg!(windows) && is_root)
                }
            }
            Where::Disk(d) => {
                if restore {
                    let m = load_manifest(&image_manifest(path), "flashtester-image")?;
                    valid_hash(&m.sha256)?;
                    if !matches!(m.sector_size, 512 | 4096) || m.sector_size != d.sector_size || m.bytes == 0 || m.bytes > d.size || m.bytes % u64::from(m.sector_size) != 0 { return Err(error("The destination must have the same sector size and at least as many bytes as the source disk.")); }
                    let mut f = open_read(path)?;
                    ctx.phase("Checking image before restore", m.bytes);
                    let chunks = check_image(&mut f, &m, ctx)?;
                    cancelled(ctx)?;
                    separate_disk(w, path)?;
                    let mut disk = RawDisk::open_restore(d)?;
                    restore_image(&mut disk, &mut f, &m, &chunks, ctx)?;
                    Ok(m)
                } else {
                    if d.size == 0 || !matches!(d.sector_size, 512 | 4096) || d.size % u64::from(d.sector_size) != 0 { return Err(error("Unsupported disk size or sector size.")); }
                    let path = new_path(path)?;
                    let sidecar = new_path(&image_manifest(&path))?;
                    ensure_space(path.parent().unwrap(), d.size)?;
                    let mut disk = RawDisk::open_backup(d)?;
                    if disk.len() != d.size { return Err(error("Disk capacity changed before backup.")); }
                    let id = platform::fs_id_path(path.parent().unwrap())?;
                    let mut record = create_file(&sidecar, id)?;
                    let mut m = Manifest::new("flashtester-image");
                    m.bytes = d.size;
                    m.sector_size = d.sector_size;
                    write_manifest(&mut record, &m)?;
                    let mut f = create_file(&path, id)?;
                    ctx.phase("Saving disk image", m.bytes);
                    m.sha256 = save_image(&mut disk, &mut f, m.bytes, ctx)?;
                    ctx.phase("Checking saved image", m.bytes);
                    if hash_file(&mut f, m.bytes, ctx, 0)? != m.sha256 { return Err(error("Saved image checksum mismatch.")); }
                    cancelled(ctx)?;
                    sync_directory(path.parent().unwrap())?;
                    m.complete = true;
                    write_manifest(&mut record, &m)?;
                    Ok(m)
                }
            }
        }
    })();
    match result {
        Ok(m) => {
            rep.set(Status::Info, if restore { "Restore completed and checked." } else { "Backup completed and checked." });
            rep.verdict.details.push(format!("{} copied and checked using SHA-256.", crate::report::size(m.bytes)));
            rep.verdict.details.push(format!("Backup: {}", path.display()));
            if let Where::Dir { root, .. } = w {
                if restore { rep.verdict.details.push(format!("Restored to: {}", root.join(folder.unwrap()).display())); }
                rep.verdict.details.push(format!("{} files, {} folders.", m.entries.iter().filter(|e| !e.directory).count(), m.entries.iter().filter(|e| e.directory).count()));
                rep.notes.push("File backups preserve contents, folder structure and file modification times. Permissions, ownership, links, alternate streams and other filesystem metadata are not preserved. Use a whole-disk image when these matter. File backups are not filesystem snapshots; close programs using the source.".into());
            } else {
                rep.notes.push("Keep the .img file and its .img.json manifest together. Image restore copies the original partition layout; it does not expand partitions on a larger disk. Bytes beyond the image are left unchanged.".into());
            }
            for p in &m.excluded { rep.notes.push(format!("Excluded OS-managed folder: {p}")); }
        }
        Err(e) => {
            rep.set(if ctx.cancelled() { Status::Cancelled } else { Status::Error }, e.to_string());
            rep.notes.push("An interrupted backup or restore may leave partial output. It is never reported as complete. No existing backup is overwritten.".into());
        }
    }
    rep.finish(start);
    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!("ft-backup-test-{}-{}-{}", std::process::id(), crate::engine::now().replace(':', ""), NEXT.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Temp { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

    fn entry(path: &str, bytes: &[u8]) -> Entry {
        Entry { path: path.into(), directory: false, size: bytes.len() as u64, modified: None, sha256: format!("{:x}", Sha256::digest(bytes)) }
    }

    fn crafted_zip(p: &Path, entries: Vec<Entry>, payload: &[u8], complete: bool) {
        let mut z = ZipWriter::new(File::create_new(p).unwrap());
        z.start_file("files/data.txt", SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
        z.write_all(payload).unwrap();
        let mut m = Manifest::new("flashtester-files");
        m.complete = complete;
        m.bytes = total_bytes(&entries).unwrap();
        m.entries = entries;
        z.start_file(MANIFEST, SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
        serde_json::to_writer(&mut z, &m).unwrap();
        z.finish().unwrap();
    }

    #[test]
    fn zip_round_trip_preserves_bytes_unicode_empty_folders_and_file_times() {
        let t = Temp::new();
        let src = t.0.join("source");
        fs::create_dir(&src).unwrap();
        fs::create_dir(src.join("empty folder")).unwrap();
        fs::create_dir(src.join("ąžuolas")).unwrap();
        let data: Vec<u8> = (0..CHUNK + 513).map(|i| (i % 251) as u8).collect();
        fs::write(src.join("ąžuolas/文件.bin"), &data).unwrap();
        fs::write(src.join("zero"), []).unwrap();
        let stamp = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        File::options().write(true).open(src.join("zero")).unwrap().set_modified(stamp).unwrap();
        let zip = t.0.join("backup.zip");
        let m = backup_files(&src, &zip, &Ctx::new(), false).unwrap();
        assert_eq!(m.bytes, data.len() as u64);
        assert!(m.complete);
        let dst = t.0.join("restored");
        restore_files(&zip, &dst, &Ctx::new()).unwrap();
        assert_eq!(fs::read(dst.join("ąžuolas/文件.bin")).unwrap(), data);
        assert!(dst.join("empty folder").is_dir());
        assert_eq!(fs::metadata(dst.join("zero")).unwrap().modified().unwrap(), stamp);
        assert!(!dst.join(".flashtester-restore-incomplete").exists());
    }

    #[test]
    fn corrupt_or_incomplete_archive_never_creates_restore_destination() {
        let t = Temp::new();
        for (name, payload, complete) in [("bad", b"bad!".as_slice(), true), ("partial", b"good".as_slice(), false)] {
            let zip = t.0.join(format!("{name}.zip"));
            crafted_zip(&zip, vec![entry("data.txt", b"good")], payload, complete);
            let dest = t.0.join(name);
            assert!(restore_files(&zip, &dest, &Ctx::new()).is_err());
            assert!(!dest.exists());
        }
    }

    #[test]
    fn archive_paths_cannot_escape_or_alias() {
        for bad in ["../escape", "/absolute", "C:/windows", "a/../../x", "a\\..\\x", "data:stream", "CON", "CLOCK$.txt", "conin$", "CONOUT$", "COM¹.txt", "LPT³", "x.", "a//b", ".flashtester-restore-incomplete", ".FLASHTESTER-RESTORE-INCOMPLETE", ".flashtester-restore-incomplete/child"] {
            assert!(relative(bad).is_err(), "{bad}");
        }
        let mut m = Manifest::new("flashtester-files");
        m.complete = true;
        for entries in [vec![entry("A", b""), entry("a", b"")], vec![entry("file", b""), entry("file/nested", b"")], vec![entry("missing/file", b"")]] {
            m.entries = entries;
            assert!(validate_manifest(&m, "flashtester-files").is_err());
        }
        let t = Temp::new();
        let zip = t.0.join("evil.zip");
        crafted_zip(&zip, vec![entry("../escape", b"good")], b"good", true);
        assert!(restore_files(&zip, &t.0.join("restore"), &Ctx::new()).is_err());
        assert!(!t.0.join("escape").exists());
        assert!(!t.0.join("restore").exists());
    }

    #[test]
    fn backup_and_restore_never_replace_existing_output() {
        let t = Temp::new();
        let src = t.0.join("source");
        fs::create_dir(&src).unwrap();
        let zip = t.0.join("existing.zip");
        fs::write(&zip, b"keep me").unwrap();
        assert!(backup_files(&src, &zip, &Ctx::new(), false).is_err());
        assert_eq!(fs::read(&zip).unwrap(), b"keep me");
        let good = t.0.join("good.zip");
        backup_files(&src, &good, &Ctx::new(), false).unwrap();
        fs::write(src.join("existing"), b"keep me").unwrap();
        assert!(restore_files(&good, &src, &Ctx::new()).is_err());
        assert_eq!(fs::read(src.join("existing")).unwrap(), b"keep me");
    }

    #[test]
    fn restored_empty_directories_are_verified() {
        let t = Temp::new();
        let p = t.0.join("empty");
        fs::create_dir(&p).unwrap();
        let entries = vec![Entry { path: "empty".into(), directory: true, size: 0, modified: None, sha256: String::new() }];
        verify_restored(&t.0, &entries, &Ctx::new()).unwrap();
        fs::remove_dir(&p).unwrap();
        assert!(verify_restored(&t.0, &entries, &Ctx::new()).is_err());
        fs::write(&p, []).unwrap();
        assert!(verify_restored(&t.0, &entries, &Ctx::new()).is_err());
    }

    #[test]
    fn cancelled_backup_does_not_create_a_complete_archive() {
        let t = Temp::new();
        let src = t.0.join("source");
        fs::create_dir(&src).unwrap();
        let ctx = Ctx::new();
        ctx.cancel();
        let zip = t.0.join("cancel.zip");
        assert!(backup_files(&src, &zip, &ctx, false).is_err());
        assert!(!zip.exists());
    }

    struct Disk { data: Vec<u8>, corrupt: bool, short: bool, writes: usize }
    impl Dev for Disk {
        fn len(&self) -> u64 { self.data.len() as u64 }
        fn write_at(&mut self, off: u64, bytes: &[u8]) -> io::Result<()> {
            self.data[off as usize..off as usize + bytes.len()].copy_from_slice(bytes);
            if self.corrupt { self.data[off as usize] ^= 1; }
            self.writes += 1;
            Ok(())
        }
        fn read_at(&mut self, off: u64, bytes: &mut [u8]) -> io::Result<usize> {
            let n = if self.short { bytes.len() - 1 } else { bytes.len() };
            bytes[..n].copy_from_slice(&self.data[off as usize..off as usize + n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    #[test]
    fn raw_round_trip_preserves_every_byte_and_detects_write_corruption() {
        let t = Temp::new();
        let data: Vec<u8> = (0..CHUNK + 512).map(|i| (i % 239) as u8).collect();
        let mut source = Disk { data: data.clone(), corrupt: false, short: false, writes: 0 };
        let mut image = File::options().read(true).write(true).create_new(true).open(t.0.join("drive.img")).unwrap();
        let mut m = Manifest::new("flashtester-image");
        m.bytes = data.len() as u64;
        m.sector_size = 512;
        m.sha256 = save_image(&mut source, &mut image, m.bytes, &Ctx::new()).unwrap();
        assert_eq!(source.writes, 0);
        assert_eq!(m.sha256, format!("{:x}", Sha256::digest(&data)));
        let chunks = check_image(&mut image, &m, &Ctx::new()).unwrap();
        let mut target = Disk { data: vec![0; data.len()], corrupt: false, short: false, writes: 0 };
        restore_image(&mut target, &mut image, &m, &chunks, &Ctx::new()).unwrap();
        assert_eq!(target.data, data);
        target.corrupt = true;
        assert!(restore_image(&mut target, &mut image, &m, &chunks, &Ctx::new()).unwrap_err().to_string().contains("does not match"));
    }

    #[test]
    fn changed_image_chunks_are_never_written() {
        let t = Temp::new();
        let data = vec![7; CHUNK + 512];
        let mut image = File::options().read(true).write(true).create_new(true).open(t.0.join("change.img")).unwrap();
        image.write_all(&data).unwrap();
        let mut m = Manifest::new("flashtester-image");
        m.bytes = data.len() as u64;
        m.sector_size = 512;
        m.sha256 = format!("{:x}", Sha256::digest(&data));
        let chunks = check_image(&mut image, &m, &Ctx::new()).unwrap();
        for changed_offset in [0, CHUNK] {
            image.seek(SeekFrom::Start(0)).unwrap();
            image.write_all(&data).unwrap();
            image.seek(SeekFrom::Start(changed_offset as u64)).unwrap();
            image.write_all(&[8]).unwrap();
            let mut target = Disk { data: vec![99; data.len()], corrupt: false, short: false, writes: 0 };
            assert!(restore_image(&mut target, &mut image, &m, &chunks, &Ctx::new()).unwrap_err().to_string().contains("changed chunk"));
            assert_eq!(target.writes, changed_offset / CHUNK);
            assert!(target.data[changed_offset..].iter().all(|b| *b == 99));
            assert!(check_image(&mut image, &m, &Ctx::new()).is_err());
        }
    }

    #[test]
    fn raw_short_reads_cancellation_and_small_targets_fail() {
        let t = Temp::new();
        let mut disk = Disk { data: vec![0; 512], corrupt: false, short: true, writes: 0 };
        let mut image = File::options().read(true).write(true).create_new(true).open(t.0.join("drive.img")).unwrap();
        assert!(save_image(&mut disk, &mut image, 512, &Ctx::new()).is_err());
        let mut m = Manifest::new("flashtester-image");
        m.bytes = 1024;
        m.sector_size = 512;
        assert!(restore_image(&mut disk, &mut image, &m, &[], &Ctx::new()).is_err());
        assert_eq!(disk.writes, 0);
        m.bytes = 512;
        let ctx = Ctx::new();
        ctx.cancel();
        assert!(restore_image(&mut disk, &mut image, &m, &[[0; 32]], &ctx).is_err());
        assert_eq!(disk.writes, 0);
    }
}
