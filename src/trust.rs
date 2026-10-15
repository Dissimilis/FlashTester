//! Trust checks that involve the user: unplug/replug (flushes the controller's
//! cache and catches identity changes) and the pull-the-plug-mid-write test.

use crate::ctx::Ctx;
use crate::dev::{AlignedBuf, Dev, FileSet, MIB, is_disconnect, is_disk_full};
use crate::engine::{Stop, verify_pass};
use crate::fulltest::{Manifest, Where, data_dir, save_manifest};
use crate::pattern::{Pattern, new_tag};
use crate::platform::{self, DiskInfo};
use crate::report::{Report, Status, size};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(15 * 60);

fn wait_until(ctx: &Ctx, what: &str, cond: impl Fn() -> bool) -> Result<(), String> {
    let t = Instant::now();
    while !cond() {
        if ctx.cancelled() {
            return Err("Cancelled.".into());
        }
        if t.elapsed() > WAIT {
            return Err(format!("Timed out waiting for {what}."));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

/// The drive is there: its folder exists on a volume whose device exists.
/// (Linux keeps serving cached directories of a pulled drive's dead mount.)
fn present(w: &Where) -> bool {
    match w {
        Where::Dir { root, .. } => data_dir(root).exists() && platform::volume_of_path(root).is_ok(),
        Where::Disk(d) => find_again(d).is_some(),
    }
}

/// Present again after a removal, with a readable test folder.
fn back(w: &Where) -> bool {
    match w {
        Where::Dir { root, .. } => present(w) && std::fs::read_dir(data_dir(root)).is_ok(),
        Where::Disk(_) => present(w),
    }
}

/// The same drive after a replug: same Windows instance, else same USB ID and size.
fn find_again(d: &DiskInfo) -> Option<DiskInfo> {
    let disks = platform::list_disks();
    if let Some(x) = disks.iter().find(|x| x.instance_id == d.instance_id) {
        return Some(x.clone());
    }
    // Another port (or a re-attached NVMe/SATA disk) gives another instance id: accept
    // only one unambiguous match, and only with a real serial: two identical blank-serial
    // sticks can't be told apart, and the other one never saw this run's data.
    let usb_id = |x: &DiskInfo| x.usb.as_ref().map(|u| (u.vid.clone(), u.pid.clone()));
    let same: Vec<&DiskInfo> = disks
        .iter()
        .filter(|x| x.protected.is_none() && x.size == d.size && x.serial == d.serial && usb_id(x) == usb_id(d))
        .filter(|x| !x.serial.is_empty() && x.placeholder_serial.is_none())
        .collect();
    if same.len() == 1 { Some(same[0].clone()) } else { None }
}

fn disk_after(w: &Where) -> Option<DiskInfo> {
    match w {
        Where::Dir { root, .. } => {
            let v = platform::volume_of_path(root).ok()?;
            platform::disk(&v.extents.first()?.0).ok()
        }
        Where::Disk(d) => find_again(d),
    }
}

/// Differences in identity between two sightings of a drive.
pub fn identity_changes(a: &DiskInfo, b: &DiskInfo) -> Vec<String> {
    let ua = a.usb.clone().unwrap_or_default();
    let ub = b.usb.clone().unwrap_or_default();
    let pairs = [
        ("Vendor", a.vendor.clone(), b.vendor.clone()),
        ("Product", a.product.clone(), b.product.clone()),
        ("Revision", a.revision.clone(), b.revision.clone()),
        ("Serial", a.serial.clone(), b.serial.clone()),
        ("USB VID", ua.vid.clone(), ub.vid.clone()),
        ("USB PID", ua.pid.clone(), ub.pid.clone()),
        ("USB release", ua.release.clone(), ub.release.clone()),
        ("USB serial", ua.serial.clone().unwrap_or_default(), ub.serial.clone().unwrap_or_default()),
        ("Capacity", a.size.to_string(), b.size.to_string()),
    ];
    pairs.into_iter().filter(|(_, x, y)| x != y).map(|(k, x, y)| format!("{k} changed from '{x}' to '{y}'")).collect()
}

/// Linux and FreeBSD servers usually don't mount a replugged drive by themselves.
fn plug_back_msg(w: &Where) -> String {
    match w {
        Where::Dir { root, .. } if cfg!(unix) => format!("Drive removed. Plug it back in and mount it again at {}.", root.display()),
        _ => "Drive removed. Plug it back in now.".into(),
    }
}

/// Ask the user to unplug and replug the drive; returns identity changes and the
/// drive as seen afterwards (its disk number may have changed).
pub fn replug(w: &Where, ctx: &Ctx) -> Result<(Vec<String>, Option<DiskInfo>), String> {
    let before = w.disk().cloned();
    ctx.prompt(Some("Unplug the drive now. Plug it back in after a few seconds.".into()));
    let gone = wait_until(ctx, "the drive to be unplugged", || !present(w));
    if let Err(e) = gone {
        ctx.prompt(None);
        return Err(e);
    }
    ctx.prompt(Some(plug_back_msg(w)));
    let back = wait_until(ctx, "the drive to come back", || back(w));
    ctx.prompt(None);
    back?;
    std::thread::sleep(Duration::from_secs(2));
    ctx.log("Drive is back.");
    let after = disk_after(w);
    let mut changes = vec![];
    if let (Some(a), Some(b)) = (&before, &after) {
        changes = identity_changes(a, b);
        for c in &changes {
            ctx.log(format!("Identity: {c}"));
        }
    }
    Ok((changes, after))
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PlugPullResult {
    pub written: u64,
    /// Data confirmed by a flush before the power was cut.
    pub flushed: u64,
    pub unplugged_during_write: bool,
    pub file_system_mounted: bool,
    pub flushed_ok_sectors: u64,
    pub flushed_lost_sectors: u64,
    pub in_flight_ok_sectors: u64,
    pub in_flight_lost_sectors: u64,
}

impl PlugPullResult {
    pub fn summary_lines(&self) -> Vec<String> {
        let mut v = vec![format!(
            "Wrote {} before the power was cut; {} had been confirmed by a flush.",
            size(self.written),
            size(self.flushed)
        )];
        if !self.unplugged_during_write {
            v.push("The drive was unplugged after writing had finished, so no write was interrupted.".into());
        }
        v.push(format!("File system after reconnecting: {}", if self.file_system_mounted { "mounted, test folder readable" } else { "unreadable" }));
        v.push(format!(
            "Confirmed data: {} intact, {} lost{}",
            size(self.flushed_ok_sectors * 512),
            size(self.flushed_lost_sectors * 512),
            if self.flushed_lost_sectors > 0 { " - the drive acknowledged writes it had not stored" } else { "" }
        ));
        let inflight = self.in_flight_ok_sectors + self.in_flight_lost_sectors;
        if inflight > 0 {
            v.push(format!(
                "Data not yet confirmed as saved: {} intact, {} lost. A power cut can lose pending writes.",
                size(self.in_flight_ok_sectors * 512),
                size(self.in_flight_lost_sectors * 512)
            ));
        }
        v
    }
}

/// Guided test: write with periodic flushes, the user pulls the drive mid-write,
/// then check that everything flushed before the cut is still intact.
pub fn run_plug_pull(w: &Where, checkpoint: u64, ctx: &Ctx) -> Report {
    let t0 = Instant::now();
    let mut rep = Report::new("plug-pull", w.target());
    let Where::Dir { root, vol, disk } = w else {
        rep.set(Status::Error, "The pull-the-plug test runs on a mounted drive (give a drive letter).");
        return rep;
    };
    let dir = data_dir(root);
    if let Err(e) = crate::fulltest::clean(root).and_then(|_| crate::fulltest::make_dir(&dir)) {
        rep.set(Status::Error, format!("Cannot prepare {}: {e}", dir.display()));
        return rep;
    }
    let pat = Pattern::Seeded { tag: new_tag() };
    let free = crate::fulltest::free_space(root).unwrap_or(0) / MIB * MIB;
    let m = Manifest {
        tool: crate::report::tool_name(),
        created: crate::engine::now(),
        mode: "file".into(),
        pattern: pat,
        bytes: free,
        complete: false,
        file_size: crate::dev::FILE_SIZE,
        volume_guid: Some(vol.guid.clone()),
        disk_instance: disk.as_ref().map(|d| d.instance_id.clone()),
        disk_id: disk.as_ref().map(|d| d.id.clone()),
        claimed: vol.size,
    };
    save_manifest(&m, Some(&dir));
    let free = crate::fulltest::free_space(root).unwrap_or(0) / MIB * MIB;
    if free < checkpoint.max(64 * MIB) {
        rep.set(Status::Refused, format!("Only {} free: the pull-the-plug test needs at least {}.", size(free), size(checkpoint.max(64 * MIB))));
        rep.finish(t0);
        return rep;
    }

    let mut dev = FileSet::new(&dir, free);
    let block = 8 * MIB as usize;
    let mut buf = AlignedBuf::new(block);
    let (mut pos, mut flushed) = (0u64, 0u64);
    let mut pulled = false;
    ctx.phase("Writing (pull the drive out any time)", free);
    ctx.prompt(Some(format!("Pull the drive out now, while it is writing (flush every {}).", size(checkpoint))));
    while pos < free {
        if ctx.cancelled() {
            break;
        }
        let n = (block as u64).min(free - pos) as usize;
        pat.fill(pos, &mut buf[..n]);
        match dev.write_at(pos, &buf[..n]) {
            Ok(()) => pos += n as u64,
            Err(e) if is_disk_full(&e) => break,
            Err(e) => {
                pulled = true;
                ctx.log(format!("Write stopped at {}: {e}", size(pos)));
                if !is_disconnect(&e) {
                    ctx.log("(not a typical disconnect error; continuing as if unplugged)");
                }
                break;
            }
        }
        ctx.progress(pos);
        if pos - flushed >= checkpoint {
            match dev.flush() {
                Ok(()) => flushed = pos,
                Err(_) => {
                    pulled = true;
                    break;
                }
            }
        }
    }
    if !pulled && !ctx.cancelled() {
        if dev.flush().is_ok() {
            flushed = pos;
        }
        ctx.log("Writing finished before the drive was pulled.");
    }
    drop(dev);
    if ctx.cancelled() {
        ctx.prompt(None);
        rep.set(Status::Cancelled, "Cancelled.");
        rep.finish(t0);
        return rep;
    }
    let w2 = w.clone();
    let wait = (|| -> Result<(), String> {
        if !pulled {
            ctx.prompt(Some("Pull the drive out now.".into()));
            wait_until(ctx, "the drive to be unplugged", || !present(&w2))?;
        } else {
            // The write failed, but the device can take a moment to disappear (and Linux
            // serves a dead mount from cache): wait for it to be gone, unless it is
            // already back (a very fast replug).
            let t = Instant::now();
            while present(&w2) && t.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        ctx.prompt(Some(plug_back_msg(w)));
        wait_until(ctx, "the drive to come back", || back(&w2))?;
        Ok(())
    })();
    ctx.prompt(None);
    if let Err(e) = wait {
        rep.set(Status::Error, e);
        rep.finish(t0);
        return rep;
    }
    std::thread::sleep(Duration::from_secs(3));
    let mut r = PlugPullResult { written: pos, flushed, unplugged_during_write: pulled, ..Default::default() };
    r.file_system_mounted = std::fs::read_dir(&dir).is_ok();
    if let (Some(a), Some(b)) = (disk.as_ref(), disk_after(w)) {
        rep.identity_changes = identity_changes(a, &b);
    }
    if r.file_system_mounted {
        let mut dev = FileSet::new(&dir, pos);
        let v1 = verify_pass(&mut dev, pat, 0, flushed, block, ctx);
        r.flushed_ok_sectors = v1.tally.ok;
        r.flushed_lost_sectors = v1.tally.lost();
        if pos > flushed {
            let v2 = verify_pass(&mut dev, pat, flushed, pos, block, ctx);
            r.in_flight_ok_sectors = v2.tally.ok;
            r.in_flight_lost_sectors = v2.tally.lost();
        }
        // Only a complete check of the confirmed data may end in a verdict.
        if v1.stop != Stop::Done {
            let (st, msg) = match v1.stop {
                Stop::Disconnected => (Status::Unreliable, "The drive disconnected again while checking.".to_string()),
                Stop::Cancelled => (Status::Cancelled, "Cancelled before the confirmed data was fully checked.".to_string()),
                ref s => (Status::Error, format!("Checking the confirmed data stopped: {s:?}")),
            };
            rep.verify.push(v1);
            rep.set(st, msg);
            rep.plug_pull = Some(r);
            rep.finish(t0);
            return rep;
        }
        rep.verify.push(v1);
    }
    let (st, head) = if !r.file_system_mounted {
        (Status::Defective, "After the power cut the file system could not be read.".to_string())
    } else if r.flushed_lost_sectors > 0 {
        (Status::Defective, format!("{} that the drive had confirmed as saved was lost when power was cut.", size(r.flushed_lost_sectors * 512)))
    } else if !rep.identity_changes.is_empty() {
        (Status::Unreliable, "Data survived. The drive identity changed after reconnecting.".to_string())
    } else if r.flushed == 0 {
        // Nothing was confirmed before the cut: there is nothing to judge.
        (Status::Info, format!("Inconclusive: the drive was pulled before the first flush ({}). Pull it later, or use a smaller --checkpoint.", size(checkpoint)))
    } else {
        (Status::Ok, format!("All {} confirmed before the power cut survived.", size(r.flushed)))
    };
    rep.set(st, head);
    rep.tested = pos;
    rep.plug_pull = Some(r);
    rep.notes.push(format!("Test data kept in {}; remove it with: flashtester clean {}", dir.display(), root.display()));
    rep.finish(t0);
    rep
}
