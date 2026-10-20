//! GUI front-end (egui). A thin shell over `app`: pick a drive and a test,
//! watch progress, read the verdict, save reports. The drive list stays on the
//! left; the main area switches between setup, run and result views.

use crate::app::{self, FixSize, Job};
use crate::ctx::{Ctx, Status as Live};
use crate::fulltest::{TestOpts, Where};
use crate::gui_kit::{self as k, Icon, Look};
use crate::gui_map::{self, Curve};
use crate::platform::{self, DiskInfo, VolumeInfo};
use crate::report::{Report, Status, size};
use crate::smart::{self, Smart};
use egui::epaint::CornerRadius;
use egui::text::{LayoutJob, TextFormat};
use egui::{self, Align, Align2, Color32, FontId, Frame, Label, Layout, Margin, Rect, Sense, Stroke, StrokeKind, UiBuilder, Vec2, pos2, vec2};
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, PartialEq)]
enum Sel {
    Volume(String),
    Disk(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Backup,
    Restore,
    Info,
    Test,
    Endurance,
    Verify,
    Probe,
    Scan,
    Iops,
    SmallFiles,
    PlugPull,
    Fix,
    Bench,
}

#[cfg(test)]
mod operation_tests {
    use super::Kind;

    #[test]
    fn saved_report_buttons_work_with_long_paths_in_narrow_panels() {
        use super::*;
        let path = PathBuf::from("C:/Users/A user with a long name/Documents/FlashTester/reports with spaces/ataskaita-ąžuolas-20261015-163400.html");
        for width in [260.0, 520.0] {
            let ctx = egui::Context::default();
            k::style(&ctx);
            let frame = |events| {
                let mut action = None;
                let output = ctx.run_ui(egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(width, 500.0))), events, ..Default::default()
                }, |ui| {
                    action = saved_report_row(ui, &path);
                });
                (output, action)
            };
            frame(vec![]);
            let (output, _) = frame(vec![]);
            for (label, expected) in [("Open file", path.clone()), ("Open folder", path.parent().unwrap().to_path_buf())] {
                let rect = output.shapes.iter().find_map(|s| match &s.shape {
                    egui::Shape::Text(t) if t.galley.job.text == label => Some(Rect::from_min_size(t.pos, t.galley.size())),
                    _ => None,
                }).expect("saved-report action must be visible");
                assert!(rect.left() >= 0.0 && rect.right() <= width, "{label} is outside the panel: {rect:?}");
                let pos = rect.center();
                frame(vec![egui::Event::PointerMoved(pos), egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() }]);
                let (_, action) = frame(vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() }]);
                assert_eq!(action, Some(expected), "{label} must select the right path");
            }
        }
    }

    #[test]
    fn protected_drive_has_only_details_and_two_read_checks() {
        let choices = Kind::for_target(true, true);
        assert!(choices == [Kind::Info, Kind::Bench, Kind::Scan]);
        assert!(!Kind::Info.needs_admin(true));
        assert!(choices.iter().all(|k| !k.erases(true)));
        // A stale volume selection must not reveal file benchmarks.
        assert!(Kind::for_target(false, true) == choices);
    }

    #[test]
    fn disk_restore_requires_confirmation_of_the_resolved_device() {
        use super::*;
        let mut gui = Gui::new();
        let disk = DiskInfo { id: "3".into(), instance_id: "confirmed-device".into(), serial: "serial-a".into(), size: 4096, ..Default::default() };
        gui.sel = Some(Sel::Disk(disk.id.clone()));
        gui.disks = vec![disk.clone()];
        let restore = |d| Job::Restore { w: Where::Disk(d), path: "backup.img".into(), folder: None };
        assert!(!gui.confirmation_matches(&restore(disk.clone())));
        gui.confirm = true;
        assert!(gui.confirmation_matches(&restore(disk.clone())));
        let mut swapped = disk.clone();
        swapped.instance_id = "replacement-device".into();
        assert!(!gui.confirmation_matches(&restore(swapped)));
        let mut changed = disk;
        changed.serial = "serial-b".into();
        assert!(!gui.confirmation_matches(&restore(changed)));
        gui.sel = None;
        assert!(!gui.confirmation_matches(&restore(DiskInfo::default())));
    }

    #[test]
    fn disconnected_drive_keeps_its_completed_report() {
        use super::*;
        let mut gui = Gui::new();
        let disk = DiskInfo { id: "removed".into(), instance_id: "original-device".into(), ..Default::default() };
        gui.sel = Some(Sel::Disk(disk.id.clone()));
        let mut report = Report::new("scan", Where::Disk(disk).target());
        report.set(Status::Unreliable, "The drive disconnected during the scan.");
        gui.result = Some(report);
        gui.disks = vec![DiskInfo { id: "other".into(), ..Default::default() }];
        gui.reconcile_selection();
        assert!(gui.sel == Some(Sel::Disk("removed".into())));
        assert_eq!(gui.result.as_ref().unwrap().verdict.status, Status::Unreliable);
        assert_eq!(gui.result.as_ref().unwrap().target.disk.as_ref().unwrap().instance_id, "original-device");
    }
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Backup => "Create backup",
            Kind::Restore => "Restore backup",
            Kind::Info => "Drive details",
            Kind::Test => "Full test",
            Kind::Endurance => "Repeated write test",
            Kind::Verify => "Check saved test data",
            Kind::Probe => "Quick capacity check",
            Kind::Scan => "Read scan",
            Kind::Iops => "Random 4 KiB speed",
            Kind::SmallFiles => "Small files",
            Kind::PlugPull => "Pull-the-plug test",
            Kind::Fix => "Resize and format",
            Kind::Bench => "Sequential speed",
        }
    }
    fn start_label(self) -> &'static str {
        match self {
            Kind::Backup => "Create backup",
            Kind::Restore => "Restore files",
            Kind::Info => "Drive details",
            Kind::Test => "Start full test",
            Kind::Endurance => "Start repeated test",
            Kind::Verify => "Check test data",
            Kind::Probe => "Check capacity",
            Kind::Scan => "Start scan",
            Kind::Iops => "Start random I/O test",
            Kind::SmallFiles => "Start small file test",
            Kind::PlugPull => "Start pull-the-plug test",
            Kind::Fix => "Resize and format",
            Kind::Bench => "Start speed test",
        }
    }
    fn group(self) -> &'static str {
        match self {
            Kind::Backup | Kind::Restore => "Backup and restore",
            Kind::Info => "Information",
            Kind::Scan => "Read checks",
            Kind::Test | Kind::Probe | Kind::Verify => "Capacity and data loss",
            Kind::Bench | Kind::Iops => "Speed",
            Kind::Endurance | Kind::SmallFiles | Kind::PlugPull => "Reliability",
            Kind::Fix => "Formatting",
        }
    }
    fn help(self, raw: bool) -> &'static str {
        match (self, raw) {
            (Kind::Backup, false) => "Save files and folders in one ZIP archive, then check their checksums. Keeps file contents and modification times. Store the backup on another disk.",
            (Kind::Backup, true) => "Save every sector in a disk image, including partitions, boot records and unused space. Needs as much space as the whole source disk. Close programs using the drive; on Linux and FreeBSD, unmount it first.",
            (Kind::Restore, false) => "Check a FlashTester ZIP backup, restore it into a new folder on this volume, then check the restored files. Existing folders are refused.",
            (Kind::Restore, true) => "Check a FlashTester disk image, overwrite this disk with it, then read it back to verify. Keep the .img.json manifest alongside the image. On Linux and FreeBSD, unmount the destination first.",
            (Kind::Info, _) => "Read the device identity reported by the operating system.",
            (Kind::Test, false) => "Write test data into free space, then read it back and check for errors. Stops early if later writes overwrite earlier data. A fake drive can also overwrite your files.",
            (Kind::Test, true) => "Write test data across the whole disk, then read it back and check for errors. Erases all files and partitions.",
            (Kind::Endurance, _) => "Repeat the write and read-back test with new data each time. Stops at an error, the pass limit, or when you cancel.",
            (Kind::Verify, _) => "Read and check data saved by an earlier test. Does not write to the drive.",
            (Kind::Probe, true) => "Check for false capacity by writing small blocks across the disk. Estimates usable space. Erases the partition table and can overwrite files.",
            (Kind::Probe, false) => {
                if cfg!(windows) {
                    "Check for false capacity using small blocks in free space. Needs administrator rights. On a fake drive, writes can overwrite existing files."
                } else {
                    "Check for false capacity using small blocks in free space. Needs root access. On a fake drive, writes can overwrite existing files."
                }
            }
            (Kind::Scan, _) => "Read every sector and mark unreadable or slow areas. Does not write to the disk.",
            (Kind::Iops, _) => "Measure random 4 KiB reads at queue depths 1 and 32. File mode also measures writes. Takes about 40 seconds.",
            (Kind::SmallFiles, _) => "Create, rename and delete small files, then check their names and contents. Reports errors and files per second.",
            (Kind::PlugPull, _) => "Unplug the drive while it writes test data. After reconnecting, check whether the data confirmed as saved is still there.",
            (Kind::Fix, _) => "Estimate the usable capacity, then create and format a partition of that size. Erases all files and partitions.",
            (Kind::Bench, false) => "Measure write and read speed using one test file. The graph shows speed across the file. This test does not verify the full capacity.",
            (Kind::Bench, true) => "Read the start of the disk and graph the speed. Does not write to the disk or verify its capacity.",
        }
    }
    /// In the order the test list shows them; the first one is the suggested default.
    fn for_target(raw: bool, protected: bool) -> &'static [Kind] {
        if protected {
            &[Kind::Info, Kind::Bench, Kind::Scan]
        } else if raw {
            &[Kind::Info, Kind::Backup, Kind::Restore, Kind::Probe, Kind::Test, Kind::Verify, Kind::Scan, Kind::Bench, Kind::Iops, Kind::Endurance, Kind::Fix]
        } else if cfg!(target_os = "freebsd") {
            // No free-space probe on FreeBSD: it cannot map a file to disk blocks.
            &[Kind::Info, Kind::Backup, Kind::Restore, Kind::Test, Kind::Verify, Kind::Bench, Kind::Iops, Kind::Endurance, Kind::SmallFiles, Kind::PlugPull]
        } else {
            &[Kind::Info, Kind::Backup, Kind::Restore, Kind::Test, Kind::Probe, Kind::Verify, Kind::Bench, Kind::Iops, Kind::Endurance, Kind::SmallFiles, Kind::PlugPull]
        }
    }
    fn erases(self, raw: bool) -> bool {
        raw && matches!(self, Kind::Test | Kind::Endurance | Kind::Probe | Kind::Fix | Kind::Restore)
    }
    fn needs_admin(self, raw: bool) -> bool {
        self != Kind::Info && (raw || self == Kind::Probe)
    }
}

struct Running {
    ctx: Ctx,
    handle: JoinHandle<Report>,
    shown: usize,
    phases: Vec<String>,
    prompt_since: Option<Instant>,
}

/// The SMART view of one drive. The data is read on a thread: a slow USB bridge
/// must not freeze the window.
struct SmartView {
    disk: DiskInfo,
    rx: Option<std::sync::mpsc::Receiver<Result<Smart, String>>>,
    data: Option<Result<Smart, String>>,
    raw_open: bool,
}

impl SmartView {
    fn open(d: &DiskInfo, admin: bool) -> SmartView {
        let mut v = SmartView { disk: d.clone(), rx: None, data: None, raw_open: false };
        if admin {
            v.load();
        }
        v
    }
    fn load(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        let d = self.disk.clone();
        std::thread::spawn(move || {
            let _ = tx.send(smart::read(&d));
        });
        self.rx = Some(rx);
        self.data = None;
    }
    fn poll(&mut self) {
        if let Some(rx) = &self.rx
            && let Ok(r) = rx.try_recv()
        {
            self.data = Some(r);
            self.rx = None;
        }
    }
}

/// What a click in the result view asks for.
enum Act {
    Save(bool),
    Rerun(Kind),
    Fix(String),
    ChooseDrive,
}

pub struct Gui {
    disks: Vec<DiskInfo>,
    sel: Option<Sel>,
    kind: Kind,
    size_limit: String,
    backup_path: String,
    restore_folder: String,
    passes: u32,
    replug: bool,
    count: u64,
    fs: String,
    confirm: bool,
    running: Option<Running>,
    live: Live,
    curves: Vec<Curve>,
    /// Drive temperature during the job: (position in GB, °C).
    temps: Vec<[f64; 2]>,
    smart: Option<SmartView>,
    result: Option<Report>,
    saved: Vec<PathBuf>,
    log: Vec<String>,
    log_open: bool,
    summary_open: bool,
    hide_serial: bool,
    error: Option<String>,
    admin: bool,
    styled: bool,
    /// Height of the guards + Start block in the last frame.
    foot_h: f32,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    elevating: Option<std::process::Child>,
}

const ADMIN: &str = if cfg!(windows) { "administrator" } else { "root" };
const ADMIN_TITLE: &str = if cfg!(windows) { "Administrator" } else { "Root" };

impl Gui {
    fn new() -> Self {
        let mut g = Gui {
            disks: vec![],
            sel: None,
            kind: Kind::Info,
            size_limit: String::new(),
            backup_path: String::new(),
            restore_folder: "Restored files".into(),
            passes: 0,
            replug: false,
            count: 5000,
            fs: crate::fix::FILESYSTEMS[0].into(),
            confirm: false,
            running: None,
            live: Live::default(),
            curves: vec![],
            temps: vec![],
            smart: None,
            result: None,
            saved: vec![],
            log: vec![],
            log_open: false,
            summary_open: true,
            hide_serial: false,
            error: None,
            admin: platform::is_admin(),
            styled: false,
            foot_h: 36.0,
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            elevating: None,
        };
        g.refresh();
        g
    }

    fn refresh(&mut self) {
        self.confirm = false;
        self.disks = platform::list_disks();
        self.reconcile_selection();
    }

    fn reconcile_selection(&mut self) {
        // Removable first; the order within a group stays as the OS lists it.
        self.disks.sort_by_key(|d| !d.removable);
        // A disconnected drive's completed report must remain visible.
        if self.result.is_some() {
            return;
        }
        if self.sel_disk().is_none() {
            if let Some(d) = self.disks.first() {
                self.select(Sel::Disk(d.id.clone()));
            } else {
                self.sel = None;
            }
        }
        if self.protected() {
            if let Some(d) = self.sel_disk() {
                self.sel = Some(Sel::Disk(d.id.clone()));
            }
            if !Kind::for_target(true, true).contains(&self.kind) {
                self.kind = Kind::Info;
                self.confirm = false;
            }
        }
    }

    fn protected(&self) -> bool {
        self.sel_disk().is_some_and(|d| d.protected.is_some())
    }

    fn read_only(&self) -> bool {
        matches!(self.kind, Kind::Info | Kind::Backup) || (self.raw() && matches!(self.kind, Kind::Scan | Kind::Bench | Kind::Verify | Kind::Iops))
    }

    fn raw(&self) -> bool {
        matches!(self.sel, Some(Sel::Disk(_)))
    }

    fn sel_disk(&self) -> Option<&DiskInfo> {
        match &self.sel {
            Some(Sel::Disk(n)) => self.disks.iter().find(|d| &d.id == n),
            Some(Sel::Volume(m)) => self.disks.iter().find(|d| d.volumes.iter().any(|v| v.mount_points.contains(m))),
            None => None,
        }
    }

    fn sel_volume(&self) -> Option<&VolumeInfo> {
        let Some(Sel::Volume(m)) = &self.sel else {
            return None;
        };
        self.disks.iter().flat_map(|d| &d.volumes).find(|v| v.mount_points.contains(m))
    }

    fn select(&mut self, s: Sel) {
        self.sel = Some(s);
        self.kind = Kind::Info;
        self.size_limit.clear();
        self.confirm = false;
        self.smart = None;
        self.result = None;
        self.error = None;
        self.saved.clear();
    }

    fn build_job(&self) -> Result<Job, String> {
        if !Kind::for_target(self.raw(), self.protected()).contains(&self.kind) {
            return Err("This operation is not available for this drive.".into());
        }
        let w: Where = match &self.sel {
            Some(Sel::Volume(m)) => app::resolve_path(m)?,
            Some(Sel::Disk(n)) if matches!(self.kind, Kind::Bench | Kind::Scan) => app::resolve_read_disk(n)?,
            Some(Sel::Disk(n)) => app::resolve_disk(n)?,
            None => return Err("Select a drive first.".into()),
        };
        if let (Some(selected), Some(current)) = (self.sel_disk(), w.disk()) {
            selected.check_identity(current)?;
        }
        let size = if self.size_limit.trim().is_empty() { None } else { Some(crate::cli::parse_size(&self.size_limit)?) };
        let opts = |passes| TestOpts { size, passes, ..Default::default() };
        Ok(match self.kind {
            Kind::Backup => Job::Backup { w, path: PathBuf::from(self.backup_path.trim().trim_matches('"')) },
            Kind::Restore => Job::Restore { w, path: PathBuf::from(self.backup_path.trim().trim_matches('"')), folder: if self.raw() { None } else { Some(PathBuf::from(self.restore_folder.trim())) } },
            Kind::Info => return Err("Select a read check or test first.".into()),
            Kind::Test => Job::Test { w, o: TestOpts { replug: self.replug, ..opts(1) } },
            Kind::Endurance => Job::Test { w, o: opts(self.passes) },
            Kind::Verify => Job::Verify { w, h2w: false, manifest: None, loops: 1 },
            Kind::Probe => Job::Probe { w },
            Kind::Scan => Job::Scan { w, size },
            Kind::Iops => Job::Iops { w, secs: 8.0, write: false },
            Kind::SmallFiles => Job::SmallFiles { w, count: self.count, keep: false },
            Kind::PlugPull => Job::PlugPull { w, checkpoint: 256 << 20 },
            Kind::Bench => Job::Bench { w, size: size.unwrap_or(if self.protected() { 256 << 20 } else { 1 << 30 }), write: false },
            Kind::Fix => {
                let Where::Disk(d) = w else {
                    return Err("Fix needs a whole disk.".into());
                };
                Job::Fix { disk: d, size: FixSize::Probe, fs: self.fs.clone(), label: "FLASH".into() }
            }
        })
    }

    fn destructive(&self) -> bool {
        self.kind.erases(self.raw())
    }

    fn needs_admin(&self) -> bool {
        self.kind.needs_admin(self.raw())
    }

    /// The size field's complaint, if its text is not a size.
    fn size_error(&self) -> Option<String> {
        let t = self.size_limit.trim();
        match crate::cli::parse_size(t) {
            _ if t.is_empty() => None,
            // A bare number is bytes; anything under 1 MiB leaves nothing to test.
            Ok(n) if n < 1 << 20 => Some(format!("“{t}” is less than 1 MB. Add a unit, like 500M or 8G.")),
            Ok(_) => None,
            Err(_) => Some(format!("“{t}” is not a size. Use a number with K, M, G or T, like 8G.")),
        }
    }

    fn uses_size(&self) -> bool {
        matches!(self.kind, Kind::Test | Kind::Endurance | Kind::Bench | Kind::Scan)
    }

    fn can_start(&self) -> bool {
        if matches!(self.kind, Kind::Backup | Kind::Restore) && (self.backup_path.trim().is_empty() || (self.kind == Kind::Restore && !self.raw() && self.restore_folder.trim().is_empty())) {
            return false;
        }
        self.kind != Kind::Info && Kind::for_target(self.raw(), self.protected()).contains(&self.kind) && self.sel_disk().is_some() && (!self.uses_size() || self.size_error().is_none()) && (!self.destructive() || self.confirm) && (!self.needs_admin() || self.admin)
    }

    fn confirmation_matches(&self, job: &Job) -> bool {
        self.confirm && self.sel_disk().zip(job.disk()).is_some_and(|(shown, resolved)| shown.check_identity(resolved).is_ok())
    }

    fn start(&mut self) {
        self.error = None;
        let job = match self.build_job() {
            Ok(j) => j,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        // The user confirmed the disk shown; refuse if the number now means another disk.
        if job.destructive() && !self.confirmation_matches(&job) {
            self.error = Some("The selected disk changed; refresh and confirm again.".into());
            self.refresh();
            return;
        }
        let ctx = Ctx::new();
        let c2 = ctx.clone();
        let handle = std::thread::spawn(move || app::run(&job, &c2));
        self.result = None;
        self.saved.clear();
        self.log.clear();
        self.curves.clear();
        self.temps.clear();
        self.smart = None;
        self.live = Live::default();
        self.log_open = false;
        self.summary_open = true;
        self.confirm = false;
        self.running = Some(Running { ctx, handle, shown: 0, phases: vec![], prompt_since: None });
    }

    fn poll(&mut self) {
        let Some(r) = &mut self.running else { return };
        let new = r.ctx.log_since(r.shown);
        r.shown += new.len();
        self.log.extend(new);
        let s = r.ctx.status();
        if !s.phase.is_empty() && r.phases.last() != Some(&s.phase) {
            r.phases.push(s.phase.clone());
            // Keep one write and one read curve: the latest of each.
            let read = gui_map::is_read(&s.phase);
            self.curves.retain(|c| c.read != read);
            self.curves.push(Curve { read, pts: vec![] });
        }
        if let Some(c) = self.curves.last_mut()
            && !s.series.is_empty()
        {
            c.pts = s.series.iter().map(|&(x, y)| [x, y]).collect();
        }
        self.temps = s.temps.iter().map(|&(x, y)| [x, y]).collect();
        r.prompt_since = match (&s.prompt, r.prompt_since) {
            (Some(_), None) => Some(Instant::now()),
            (Some(_), t) => t,
            (None, _) => None,
        };
        self.live = s;
        if !r.handle.is_finished() {
            return;
        }
        let r = self.running.take().unwrap();
        let rep = r.handle.join().unwrap_or_else(|_| {
            let mut rep = Report::default();
            rep.set(Status::Error, "The test crashed.");
            rep
        });
        self.log.extend(r.ctx.log_since(r.shown));
        let curves = gui_map::report_curves(&rep);
        if !curves.is_empty() {
            self.curves = curves;
        }
        self.temps = rep.smart.iter().flat_map(|m| &m.temps).map(|t| [t.pos as f64 / 1e9, t.c as f64]).collect();
        self.result = Some(rep);
        self.refresh();
    }

    fn save(&mut self, evidence: bool) {
        let Some(mut rep) = self.result.clone() else { return };
        if self.hide_serial {
            rep.hide_serials();
        }
        let rep = &rep;
        let dir = platform::reports_dir();
        // Milliseconds: report files are never overwritten, and both buttons write a JSON.
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f");
        let (name, body) = if evidence { (format!("evidence-{stamp}.html"), rep.evidence_html()) } else { (format!("report-{}-{stamp}.html", rep.command), rep.html()) };
        let p = dir.join(name);
        let json = dir.join(format!("report-{}-{stamp}.json", rep.command));
        match write_report(rep, &json, &p, &body) {
            Ok(()) => {
                self.open_saved(&p);
                self.saved.retain(|old| old != &p && old != &json);
                self.saved.push(p);
                self.saved.push(json);
            }
            Err(e) => self.error = Some(format!("Cannot save: {e}")),
        }
    }

    fn open_saved(&mut self, path: &std::path::Path) {
        self.error = platform::open_file(path).err().map(|e| format!("Cannot open {}: {e}", path.display()));
    }

    fn saved_reports(&mut self, ui: &mut egui::Ui) {
        let mut open = None;
        for p in &self.saved {
            if let Some(path) = saved_report_row(ui, p) {
                open = Some(path);
            }
        }
        if let Some(path) = open {
            self.open_saved(&path);
        }
        if let Some(e) = &self.error {
            ui.add(Label::new(k::text(e, 13.0, k::DANGER_ERR)).wrap());
        }
    }

    /// Title and subtitle of the selected target.
    fn target_text(&self) -> (String, String) {
        let d = self.sel_disk();
        let name = d.map(|d| d.name()).unwrap_or_default();
        match &self.sel {
            Some(Sel::Volume(m)) => {
                let v = self.sel_volume();
                let (key, label) = vol_parts(m, v.map(|v| v.label.as_str()).unwrap_or_default());
                let disk = d.map(|d| format!("Disk {}", d.id)).unwrap_or_else(|| "disk".into());
                let sub = match v {
                    Some(v) => format!("Volume on {disk}: {name}, {}, {} free", v.fs, size(v.free)),
                    None => format!("Volume on {disk}"),
                };
                (if label.is_empty() { key } else { format!("{key}  {label}") }, sub)
            }
            Some(Sel::Disk(id)) => (format!("Disk {id}: whole disk"), format!("{name}, {}, all partitions", d.map(|d| size(d.size)).unwrap_or_default())),
            None => (String::new(), String::new()),
        }
    }

    /// "disk 2 (SanDisk Ultra, 128.0 GB)" for the confirm box.
    fn disk_desc(&self) -> String {
        self.sel_disk().map(|d| format!("disk {} ({}, {})", d.id, d.name(), size(d.size))).unwrap_or_default()
    }

    // ---- top bar and drive list ----

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let mut relaunch = false;
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            k::logo(ui);
            ui.label(k::medium("FlashTester", 16.0, k::TEXT));
            ui.label(k::text(env!("CARGO_PKG_VERSION"), 12.0, k::TEXT3));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if self.admin {
                    ui.label(k::text(ADMIN_TITLE, 12.0, k::TEXT3));
                    k::icon(ui, Icon::ShieldCheck, 14.0, k::TEXT3);
                } else {
                    relaunch = k::button(ui, Look::Secondary, None, &format!("Restart as {ADMIN}"), 28.0, 12.0, self.running.is_none()).clicked();
                    ui.add_space(4.0);
                    ui.label(k::text(format!("Whole-disk tests need {ADMIN} rights"), 12.0, k::WARN));
                    k::icon(ui, Icon::ShieldAlert, 14.0, k::WARN);
                }
            });
        });
        if relaunch {
            self.relaunch(ui.ctx());
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let locked = self.running.is_some();
        ui.allocate_ui_with_layout(vec2(ui.available_width(), 44.0), Layout::left_to_right(Align::Center), |ui| {
            ui.add_space(16.0);
            ui.label(k::medium("Drives", 16.0, k::TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(12.0);
                if k::icon_button(ui, Icon::Refresh, "Refresh", !locked, if locked { 0.35 } else { 1.0 }).on_hover_text("Refresh").clicked() {
                    self.refresh();
                }
                if locked {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(k::text("Drive selection locked during test", 12.0, k::TEXT3));
                    k::icon(ui, Icon::Lock, 13.0, k::TEXT3);
                }
            });
        });
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            Frame::new().inner_margin(Margin { left: 10, right: 10, top: 0, bottom: 10 }).show(ui, |ui| {
                if locked {
                    ui.multiply_opacity(0.5);
                }
                ui.spacing_mut().item_spacing.y = 8.0;
                let disks = self.disks.clone();
                for (label, removable) in [("Removable", true), ("Internal", false)] {
                    let group: Vec<&DiskInfo> = disks.iter().filter(|d| d.removable == removable).collect();
                    if group.is_empty() {
                        continue;
                    }
                    ui.horizontal(|ui| {
                        ui.add_space(4.0);
                        ui.label(k::medium(label, 12.0, k::TEXT3));
                    });
                    for d in group {
                        self.disk_card(ui, d, locked);
                    }
                }
                if disks.is_empty() {
                    ui.label(k::text("No drives found. Plug one in and press Refresh.", 12.0, k::TEXT2));
                }
            });
        });
    }

    fn disk_card(&mut self, ui: &mut egui::Ui, d: &DiskInfo, locked: bool) {
        let mine = self.sel_disk().is_some_and(|x| x.id == d.id);
        let mut pick = None;
        let mut open_smart = false;
        ui.scope(|ui| {
            Frame::new()
                .fill(k::DISK)
                .stroke(Stroke::new(1.0, if mine { k::LINE_STRONG } else { k::LINE_CARD }))
                .corner_radius(6)
                .inner_margin(Margin { left: 6, right: 6, top: 0, bottom: 6 })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 4.0;
                    let (h, response) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
                    if response.clicked() && !locked {
                        pick = Some(Sel::Disk(d.id.clone()));
                    }
                    k::pointer_on_hover(&response);
                    let p = ui.painter();
                    let ix = h.left() + 4.0;
                    k::paint_icon(p, Rect::from_center_size(pos2(ix + 10.0, h.center().y), Vec2::splat(16.0)), disk_icon(d), k::TEXT2);
                    let sz = k::paint_medium(p, pos2(h.right() - 4.0, h.center().y), Align2::RIGHT_CENTER, &size(d.size), 16.0, k::TEXT);
                    let tx = ix + 28.0;
                    let max_w = sz.left() - 8.0 - tx;
                    p.galley(pos2(tx, h.center().y - 17.0), k::galley(ui, &d.name(), k::medium_font(14.0), max_w), k::TEXT);
                    p.galley(pos2(tx, h.center().y + 1.0), k::galley(ui, &format!("Disk {}, {}", d.id, bus_text(d)), FontId::proportional(12.0), max_w), k::TEXT3);
                    if let Some(why) = &d.protected {
                        ui.horizontal(|ui| {
                            ui.add_space(32.0);
                            ui.add(Label::new(k::text(format!("Read only: {why}"), 12.0, k::TEXT2)).wrap());
                        });
                        // Reading SMART is read-only, so the system disk gets it too.
                        if smart::possible(d) && !locked {
                            ui.horizontal(|ui| {
                                ui.add_space(32.0);
                                open_smart = ui.link(k::text("SMART data", 12.0, k::LINK)).clicked();
                            });
                        }
                        ui.add_space(2.0);
                        return;
                    }
                    for v in &d.volumes {
                        if let Some(m) = v.mount_points.first() {
                            let s = Sel::Volume(m.clone());
                            let (key, label) = vol_parts(m, &v.label);
                            if self.choice_row(ui, &s, &key, &label, &format!("{} free", size(v.free)), locked).clicked() {
                                pick = Some(s);
                            }
                        }
                    }
                    let s = Sel::Disk(d.id.clone());
                    if self.choice_row(ui, &s, "", "Whole disk", "details and tests", locked).clicked() {
                        pick = Some(s);
                    }
                });
        });
        if let Some(s) = pick {
            self.select(s);
        }
        if open_smart {
            self.smart = Some(SmartView::open(d, self.admin));
        }
    }

    fn choice_row(&self, ui: &mut egui::Ui, s: &Sel, key: &str, title: &str, right: &str, locked: bool) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), if locked { Sense::hover() } else { Sense::click() });
        let on = self.sel.as_ref() == Some(s);
        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, !locked, on, format!("{key} {title}, {right}").trim()));
        let (fill, stroke, ring, dot) = k::row_colors(on, false, resp.hovered() && !locked, k::PANEL, k::LINE_CARD);
        let p = ui.painter();
        let cy = rect.center().y;
        p.rect(rect, 4.0, fill, Stroke::new(1.0, stroke), StrokeKind::Inside);
        k::radio(p, pos2(rect.left() + 15.0, cy), ring, dot);
        let rr = k::paint_text(p, pos2(rect.right() - 8.0, cy), Align2::RIGHT_CENTER, right, 12.0, k::TEXT2);
        let mut x = rect.left() + 32.0;
        if !key.is_empty() {
            // A drive letter is short; a Unix mount point may take the room the label leaves.
            let room = rr.left() - 8.0 - x;
            let max_w = if title.is_empty() { room } else { room.min(110.0) };
            let mut g = k::galley(ui, key, FontId::monospace(13.0), f32::INFINITY);
            if g.size().x > max_w
                && let Some((_, base)) = key.rsplit_once('/')
            {
                // Keep the end of a long mount point: "…/STICK" says more than "/media/us…".
                g = k::galley(ui, &format!("…/{base}"), FontId::monospace(13.0), max_w);
            } else if g.size().x > max_w {
                g = k::galley(ui, key, FontId::monospace(13.0), max_w);
            }
            let w = g.size().x;
            p.galley(pos2(x, cy - g.size().y / 2.0), g, k::LINK);
            x += w + 6.0;
        }
        let g = k::galley(ui, title, FontId::proportional(14.0), rr.left() - 8.0 - x);
        p.galley(pos2(x, cy - g.size().y / 2.0), g, k::TEXT);
        if !locked {
            k::pointer_on_hover(&resp);
        }
        resp
    }

    // ---- main area ----

    /// Back to the selected drive's operation list, as clicking the drive does.
    fn back_bar(&mut self, ui: &mut egui::Ui) {
        let enabled = self.running.is_none() && self.sel.is_some();
        if k::button(ui, Look::Secondary, Some(Icon::ChevronLeft), "Back to menu", 28.0, 12.0, enabled).clicked()
            && let Some(s) = self.sel.clone()
        {
            self.select(s);
            // The drive may have gone while its report was shown.
            self.reconcile_selection();
        }
        ui.add_space(4.0);
    }

    fn main_view(&mut self, ui: &mut egui::Ui) {
        if self.running.is_some() {
            self.run_view(ui);
        } else if self.smart.is_some() {
            self.smart_view(ui);
        } else if self.result.is_some() {
            self.result_view(ui);
        } else if self.sel.is_none() {
            self.empty_view(ui);
        } else {
            self.setup_view(ui);
        }
    }

    fn empty_view(&mut self, ui: &mut egui::Ui) {
        let h = ui.available_height();
        ui.add_space(((h - 330.0) / 2.0).max(0.0));
        ui.horizontal(|ui| {
            ui.add_space(40.0);
            ui.vertical(|ui| {
                ui.set_width(ui.available_width().min(640.0));
                ui.spacing_mut().item_spacing.y = 8.0;
                ui.label(k::light("Select a drive to test", 40.0, k::TEXT));
                ui.add(Label::new(k::text("Select a volume to test its free space, or a whole disk to test across all partitions.", 15.0, k::TEXT2)).wrap());
                ui.add_space(20.0);
                ui.scope(|ui| {
                    ui.spacing_mut().item_spacing.x = 20.0;
                    ui.columns(2, |columns| {
                    info_card(
                        &mut columns[0],
                        k::PANEL,
                        k::LINE_CARD,
                        "Volume: file mode",
                        k::LINK,
                        "Tests free space only",
                        if cfg!(windows) {
                            "Select a drive letter, such as E:. A fake drive can overwrite existing files during the test."
                        } else {
                            "Select a mounted volume, such as /media/stick. A fake drive can overwrite existing files during the test."
                        },
                    );
                    info_card(
                        &mut columns[1],
                        k::EMPTY_RAW,
                        k::EMPTY_RAW_STROKE,
                        "Whole disk: raw mode",
                        k::DANGER,
                        "Tests across the whole disk",
                        &format!("Select Whole disk under a drive. Write tests erase data and need {ADMIN} rights."),
                    );
                    });
                });
                ui.add_space(20.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    for (t, c) in [
                        ("For a new drive, select its volume and run a ", k::TEXT2),
                        ("quick capacity check", k::TEXT),
                        (", then a ", k::TEXT2),
                        ("full test", k::TEXT),
                        (".", k::TEXT2),
                    ] {
                        ui.label(k::text(t, 13.0, c));
                    }
                });
            });
        });
    }

    fn setup_view(&mut self, ui: &mut egui::Ui) {
        let raw = self.raw();
        if self.target_card(ui, raw)
            && let Some(d) = self.sel_disk()
        {
            self.smart = Some(SmartView::open(d, self.admin));
            return;
        }
        let avail = ui.available_rect_before_wrap();
        let h = avail.height().max(460.0);
        let left = Rect::from_min_size(avail.min, vec2(290.0, h));
        let right = Rect::from_min_max(pos2(left.right() + 20.0, avail.top()), pos2(avail.right(), avail.top() + h));
        egui::ScrollArea::vertical().id_salt("operation-list").max_height(h).show(&mut ui.new_child(UiBuilder::new().max_rect(left)), |ui| {
            self.test_list(ui, raw);
        });
        ui.painter().rect(right, 6.0, k::PANEL, Stroke::new(1.0, k::LINE_CARD), StrokeKind::Inside);
        self.test_detail(&mut ui.new_child(UiBuilder::new().max_rect(right.shrink(20.0))), raw);
        ui.allocate_rect(Rect::from_min_size(avail.min, vec2(avail.width(), h)), Sense::hover());
    }

    /// True when the SMART button was clicked.
    fn target_card(&self, ui: &mut egui::Ui, raw: bool) -> bool {
        let (title, sub) = self.target_text();
        let mut smart = false;
        let resp = Frame::new()
            .fill(k::PANEL)
            .stroke(Stroke::new(1.0, if self.destructive() { k::DANGER_STROKE } else { k::LINE }))
            .corner_radius(6)
            .inner_margin(Margin { left: 14, right: 14, top: 30 + 12, bottom: 14 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 10.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    ui.label(k::medium(title, 24.0, k::TEXT));
                    ui.add(Label::new(k::text(sub, 13.0, k::TEXT2)).truncate());
                });
                smart = self.identity(ui);
            });
        let r = resp.response.rect;
        mode_band(ui, Rect::from_min_size(r.min, vec2(r.width(), 30.0)).shrink(1.0), raw, self.read_only(), self.protected(), matches!(self.kind, Kind::Backup | Kind::Restore), CornerRadius { nw: 5, ne: 5, sw: 0, se: 0 }, None);
        smart
    }

    /// True when the SMART button was clicked.
    fn identity(&self, ui: &mut egui::Ui) -> bool {
        let Some(d) = self.sel_disk() else { return false };
        let mut smart = false;
        let usb = d.usb.as_ref();
        let items = [
            ("Device", d.name(), device_sub(d), false),
            ("Serial number", if d.serial.is_empty() { "(none)".into() } else { d.serial.clone() }, String::new(), d.placeholder_serial.is_some()),
            ("Reported capacity", size(d.size), format!("{} bytes", digits(d.size)), false),
            ("USB ID", usb.map(|u| format!("{}:{}", u.vid, u.pid)).unwrap_or_else(|| "-".into()), usb.map(usb_sub).unwrap_or_default(), false),
            ("Connection", usb.map(|u| u.link.clone()).filter(|l| !l.is_empty()).unwrap_or_else(|| d.bus.clone()), String::new(), usb.is_some_and(|u| u.slow_link)),
            ("Firmware", if d.revision.is_empty() { "Not reported".into() } else { d.revision.clone() }, String::new(), false),
        ];
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            for row in items.chunks(3) {
                ui.columns(3, |columns| {
                    for (ui, (label, value, sub, warn)) in columns.iter_mut().zip(row) {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(k::medium(*label, 11.0, k::TEXT3));
                        ui.add(Label::new(k::text(value, 13.0, if *warn { k::WARN_TEXT } else { k::TEXT })).wrap());
                        if !sub.is_empty() {
                            ui.add(Label::new(k::mono(sub, 11.0, k::TEXT3)).wrap());
                        }
                    }
                });
            }
            if smart::possible(d) {
                ui.add_space(4.0);
                smart = k::button(ui, Look::Secondary, None, "SMART data", 28.0, 12.0, true)
                    .on_hover_text("The drive's own counters: errors, temperature, hours, data written")
                    .clicked();
            }
        });
        let mut warns = vec![];
        if let Some(p) = &d.placeholder_serial {
            warns.push(format!("Check the serial number: {p}"));
        }
        if usb.is_some_and(|u| u.slow_link) {
            warns.push("Running at USB 2 speed or slower".to_string());
        }
        if !warns.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for w in &warns {
                    k::chip(ui, Some(Icon::Alert), w, 24.0, 12.0, k::WARN_TINT, k::WARN_STROKE, k::WARN_TEXT);
                }
            });
        }
        smart
    }

    fn smart_view(&mut self, ui: &mut egui::Ui) {
        let admin = self.admin;
        let Some(v) = &mut self.smart else { return };
        v.poll();
        if v.rx.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        let (mut back, mut again, mut relaunch) = (false, false, false);
        let d = v.disk.clone();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            back = k::button(ui, Look::Secondary, None, "Back", 30.0, 13.0, true).clicked();
            if admin {
                again = k::icon_button(ui, Icon::Refresh, "Read again", v.rx.is_none(), 1.0).on_hover_text("Read again").clicked();
            }
        });
        let ok = v.data.as_ref().and_then(|r| r.as_ref().ok());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            ui.label(k::medium(ok.map(|s| s.model.clone()).unwrap_or_else(|| d.name()), 24.0, k::TEXT));
            let sub = match ok {
                Some(s) => format!("Disk {} · read via {} · serial {} · firmware {}", d.id, s.via, s.serial, s.firmware),
                None => format!("Disk {} · {}", d.id, bus_text(&d)),
            };
            ui.add(Label::new(k::text(sub, 13.0, k::TEXT2)).truncate());
        });
        if !admin {
            ui.label(k::text(format!("Reading SMART data needs {ADMIN} rights."), 14.0, k::WARN_TEXT));
            relaunch = k::button(ui, Look::Amber, None, &format!("Restart as {ADMIN}"), 32.0, 13.0, true).clicked();
        } else if v.rx.is_some() {
            ui.label(k::text("Reading SMART data…", 14.0, k::TEXT2));
        } else {
            match &v.data {
                Some(Ok(s)) => smart_body(ui, s, &mut v.raw_open),
                Some(Err(e)) => {
                    ui.label(k::text("No SMART data from this drive.", 16.0, k::TEXT));
                    ui.add(Label::new(k::text(e, 13.0, k::TEXT3)).wrap());
                }
                None => {}
            }
        }
        if again {
            v.load();
        }
        if back {
            self.smart = None;
        }
        if relaunch {
            self.relaunch(ui.ctx());
        }
    }

    fn test_list(&mut self, ui: &mut egui::Ui, raw: bool) {
        ui.spacing_mut().item_spacing.y = 2.0;
        Frame::new().fill(k::HINT).stroke(Stroke::new(1.0, k::LINE_CARD)).corner_radius(4).inner_margin(Margin::symmetric(10, 8)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let hint =
                if self.protected() { "Read checks do not write test data. They cannot confirm usable capacity." } else { "Choose drive details, a backup operation or a test." };
            ui.add(Label::new(k::text(hint, 12.0, k::TEXT2)).wrap());
        });
        ui.add_space(4.0);
        let mut group = "";
        for &kd in Kind::for_target(raw, self.protected()) {
            if kd.group() != group {
                group = kd.group();
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.label(k::medium(group, 12.0, k::TEXT3));
                });
                ui.add_space(2.0);
            }
            if self.test_row(ui, kd, raw).clicked() {
                self.kind = kd;
                self.size_limit.clear();
                self.confirm = false;
                self.error = None;
            }
        }
    }

    fn test_row(&self, ui: &mut egui::Ui, kd: Kind, raw: bool) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
        let on = self.kind == kd;
        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, on, kd.name()));
        let (fill, stroke, ring, dot) = k::row_colors(on, kd.erases(raw), resp.hovered(), Color32::TRANSPARENT, Color32::TRANSPARENT);
        let p = ui.painter();
        let cy = rect.center().y;
        p.rect(rect, 3.0, fill, Stroke::new(1.0, stroke), StrokeKind::Inside);
        k::radio(p, pos2(rect.left() + 17.0, cy), ring, dot);
        let mut x = rect.right() - 10.0;
        if kd.erases(raw) {
            x = k::paint_medium(p, pos2(x, cy), Align2::RIGHT_CENTER, "Erases", 11.0, k::DANGER).left() - 8.0;
        }
        if !raw && !self.admin && kd.needs_admin(false) {
            k::paint_icon(p, Rect::from_center_size(pos2(x - 6.5, cy), Vec2::splat(13.0)), Icon::Shield, k::WARN);
            x -= 21.0;
        }
        if kd == Kind::Test && !self.protected() {
            x = k::paint_medium(p, pos2(x, cy), Align2::RIGHT_CENTER, "Suggested", 11.0, k::LINK).left() - 8.0;
        }
        let left = rect.left() + 34.0;
        let g = k::galley(ui, if raw && kd == Kind::Bench { "Read speed" } else { kd.name() }, FontId::proportional(14.0), x - left);
        p.galley(pos2(left, cy - g.size().y / 2.0), g, if on { k::TEXT } else { k::TEXT2B });
        k::pointer_on_hover(&resp);
        resp
    }

    fn drive_details(&mut self, ui: &mut egui::Ui) {
        let Some(d) = self.sel_disk().cloned() else { return };
        ui.label(k::medium("Drive details", 26.0, k::TEXT));
        ui.add(Label::new(k::text("Identity reported by the operating system. Capacity has not been verified.", 13.0, k::TEXT2)).wrap());
        ui.add_space(10.0);
        ui.horizontal_wrapped(|ui| {
            if k::button(ui, Look::Plain, None, "Refresh details", 28.0, 12.0, true).clicked() {
                self.refresh();
            }
            if k::button(ui, Look::Plain, None, "Copy details", 28.0, 12.0, true).clicked() {
                let rep = app::identification_report(&d);
                ui.ctx().copy_text(format!("{}\n\n{}\n\n{}", rep.verdict.headline, rep.verdict.details.join("\n"), rep.notes.join("\n")));
            }
            if k::button(ui, Look::Plain, None, "Save report", 28.0, 12.0, true).clicked() {
                let mut rep = app::identification_report(&d);
                if self.hide_serial {
                    rep.hide_serials();
                }
                let dir = platform::reports_dir();
                let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f");
                let html = dir.join(format!("drive-info-{stamp}.html"));
                let json = html.with_extension("json");
                match write_report(&rep, &json, &html, &rep.html()) {
                    Ok(()) => { self.error = None; self.saved = vec![html, json]; }
                    Err(e) => self.error = Some(format!("Cannot save report: {e}")),
                }
            }
        });
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            k::checkbox(ui, &mut self.hide_serial, 16.0, k::TEXT3, k::ACCENT, k::text("Hide serial numbers in saved reports", 12.0, k::TEXT2), None);
        });
        self.saved_reports(ui);
        ui.add_space(10.0);
        egui::ScrollArea::vertical().id_salt("drive-details").max_height(ui.available_height()).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            for (label, value) in d.details() {
                ui.label(k::medium(label, 11.0, k::TEXT3));
                ui.add(Label::new(k::text(value, 13.0, k::TEXT)).wrap());
                ui.add_space(7.0);
            }
            ui.add(Label::new(k::text("Controller hints do not confirm the controller or NAND inside the drive.", 12.0, k::TEXT2)).wrap());
        });
    }

    fn test_detail(&mut self, ui: &mut egui::Ui, raw: bool) {
        if self.kind == Kind::Info {
            self.drive_details(ui);
            return;
        }
        let inner = ui.max_rect();
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(k::medium(self.kind.group(), 12.0, k::TEXT3));
        ui.add_space(2.0);
        ui.label(k::medium(if raw && self.kind == Kind::Bench { "Read speed" } else { self.kind.name() }, 30.0, k::TEXT));
        ui.add_space(6.0);
        ui.scope(|ui| {
            ui.set_max_width(ui.available_width().min(440.0));
            ui.add(Label::new(k::text(self.kind.help(raw), 14.0, k::TEXT2)).wrap());
        });
        if self.protected() {
            ui.add_space(10.0);
            ui.add(Label::new(k::text("Other programs can affect the result and may respond more slowly during the check. You can cancel at any time.", 13.0, k::TEXT2)).wrap());
        }
        ui.add_space(20.0);
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 14.0;
            self.options(ui);
        });

        // Guards and the Start row sit at the bottom of the panel: place them by
        // their height in the last frame (egui lays out in one top-down pass).
        let (destructive, needs_admin) = (self.destructive(), self.needs_admin());
        let (mut start, mut relaunch) = (false, false);
        let top = (inner.bottom() - self.foot_h).max(ui.cursor().top() + 20.0);
        let mut ui = ui.new_child(UiBuilder::new().max_rect(Rect::from_min_max(pos2(inner.left(), top), pos2(inner.right(), top + 1000.0))).layout(Layout::top_down(Align::Min)));
        ui.spacing_mut().item_spacing.y = 16.0;
        let full = inner.width();
        if needs_admin && !self.admin {
            Frame::new().fill(k::WARN_TINT).stroke(Stroke::new(1.0, k::WARN_STROKE)).corner_radius(4).inner_margin(Margin::symmetric(14, 12)).show(&mut ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 14.0;
                    k::icon(ui, Icon::ShieldAlert, 22.0, k::WARN);
                    let label = format!("Restart as {ADMIN}");
                    let text_w = full - 28.0 - 22.0 - 14.0 - k::button_width(ui, &label, 32.0, 13.0, false) - 14.0;
                    ui.allocate_ui_with_layout(vec2(text_w, 10.0), Layout::top_down(Align::Min), |ui| {
                        ui.set_width(text_w);
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(k::medium(format!("This operation needs {ADMIN} rights"), 14.0, k::WARN_TEXT));
                        ui.add(Label::new(k::text("After restarting, select the drive and operation again.", 12.0, k::WARN_SUB)).wrap());
                    });
                    relaunch = k::button(ui, Look::Amber, None, &label, 32.0, 13.0, true).clicked();
                });
            });
        }
        if destructive {
            Frame::new().fill(k::DANGER_TINT).stroke(Stroke::new(1.0, k::DANGER_STROKE)).corner_radius(4).inner_margin(14).show(&mut ui, |ui| {
                ui.set_width(ui.available_width());
                let title = k::medium(format!("Erase all data on {}", self.disk_desc()), 14.0, k::DANGER_LIGHT);
                let sub = k::text("This removes all files and partitions. It cannot be undone.", 12.0, k::DANGER_SUB);
                k::checkbox(ui, &mut self.confirm, 18.0, k::DANGER, k::DANGER, title, Some(sub));
            });
        }
        if let Some(e) = &self.error {
            ui.add(Label::new(k::text(e, 13.0, k::DANGER_ERR)).wrap());
        }
        let (note, color) = self.foot_note(raw);
        let can = self.can_start();
        let label = if destructive { format!("Erase disk {} and start", self.sel_disk().map(|d| d.id.as_str()).unwrap_or_default()) } else { self.kind.start_label().to_string() };
        let note_w = full - k::button_width(&ui, &label, 36.0, 15.0, true) - 16.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            ui.allocate_ui_with_layout(vec2(note_w, 36.0), Layout::left_to_right(Align::Center), |ui| {
                ui.set_width(note_w);
                ui.add(Label::new(k::text(note, 12.0, color)).wrap());
            });
            let (look, icon) = if destructive { (Look::Danger, Icon::Alert) } else { (Look::Primary, Icon::Play) };
            start = k::button(ui, look, Some(icon), &label, 36.0, 15.0, can).clicked();
        });
        let h = ui.min_rect().height();
        if (h - self.foot_h).abs() > 0.5 {
            self.foot_h = h;
            ui.ctx().request_discard("footer height");
        }
        if start {
            self.start();
        }
        if relaunch {
            self.relaunch(ui.ctx());
        }
    }

    fn options(&mut self, ui: &mut egui::Ui) {
        match self.kind {
            Kind::Backup | Kind::Restore => {
                ui.label(k::text(if self.kind == Kind::Backup { "Save backup as (on another disk)" } else { "Backup file (on another disk)" }, 12.0, k::TEXT2));
                let hint = match (cfg!(windows), self.raw()) {
                    (true, true) => "C:\\Backups\\drive.img", (true, false) => "C:\\Backups\\files.zip",
                    (false, true) => "/home/user/backups/drive.img", (false, false) => "/home/user/backups/files.zip",
                };
                ui.add(egui::TextEdit::singleline(&mut self.backup_path).desired_width(f32::INFINITY).hint_text(hint));
                if self.kind == Kind::Restore && !self.raw() {
                    ui.label(k::text("New folder on the selected volume", 12.0, k::TEXT2));
                    ui.add(egui::TextEdit::singleline(&mut self.restore_folder).desired_width(f32::INFINITY));
                }
                ui.add(Label::new(k::text(if self.raw() { "Select a volume in the drive list for a file-level ZIP backup instead." } else { "Select the whole disk in the drive list for a byte-for-byte image instead." }, 12.0, k::TEXT3)).wrap());
            }
            Kind::Test | Kind::Endurance => {
                self.size_field(ui, "Size limit", "Leave blank to test all available space. Examples: 8G, 500M");
                if self.kind == Kind::Endurance {
                    ui.horizontal(|ui| {
                        ui.label(k::text("Passes", 12.0, k::TEXT2));
                        ui.add(egui::DragValue::new(&mut self.passes).range(0..=1000));
                        ui.add(Label::new(k::text("0 means repeat until an error or cancellation", 12.0, k::TEXT3)).wrap());
                    });
                } else {
                    k::checkbox(
                        ui,
                        &mut self.replug,
                        16.0,
                        k::TEXT3,
                        k::ACCENT,
                        k::text("Reconnect the drive before checking data", 14.0, k::TEXT),
                        Some(k::text("Clears the device cache before reading the test data back.", 12.0, k::TEXT3)),
                    );
                }
            }
            Kind::Bench => self.size_field(ui, "Test size", if self.protected() { "Leave blank for a short 256 MiB check. Examples: 1G, 500M" } else { "Leave blank for 1 GiB. Examples: 4G, 500M" }),
            Kind::Scan => self.size_field(ui, "Scan size", "Leave blank to read the whole disk. Examples: 4G, 500M"),
            Kind::SmallFiles => {
                ui.horizontal(|ui| {
                    ui.label(k::text("Files", 12.0, k::TEXT2));
                    ui.add(egui::DragValue::new(&mut self.count).range(100..=100_000));
                });
            }
            Kind::Fix => {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    ui.label(k::text("File system", 12.0, k::TEXT2));
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 16.0;
                        for f in crate::fix::FILESYSTEMS {
                            if k::radio_choice(ui, self.fs == *f, f).clicked() {
                                self.fs = f.to_string();
                            }
                        }
                    });
                });
            }
            _ => {}
        }
    }

    fn size_field(&mut self, ui: &mut egui::Ui, label: &str, help: &str) {
        let err = self.size_error();
        let hint = if self.kind == Kind::Bench { if self.protected() { "256 MiB" } else { "1 GiB" } } else { "all" };
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.label(k::text(label, 12.0, k::TEXT2));
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let (r, _) = ui.allocate_exact_size(vec2(140.0, 30.0), Sense::hover());
                ui.painter().rect(r, 3.0, k::BAR, Stroke::new(1.0, if err.is_some() { k::DANGER } else { k::LINE }), StrokeKind::Inside);
                let edit = egui::TextEdit::singleline(&mut self.size_limit)
                    .frame(Frame::NONE)
                    .margin(Margin::ZERO)
                    .font(FontId::monospace(13.0))
                    .text_color(if err.is_some() { k::DANGER_LIGHT } else { k::TEXT })
                    .hint_text(k::mono(hint, 13.0, k::TEXT3));
                ui.new_child(UiBuilder::new().max_rect(r.shrink2(vec2(10.0, 6.0)))).add(edit);
                ui.add(Label::new(k::text(help, 12.0, k::TEXT3)).wrap());
            });
            if let Some(e) = err {
                ui.label(k::text(e, 12.0, k::DANGER_ERR));
            }
        });
    }

    fn foot_note(&self, raw: bool) -> (String, Color32) {
        if self.destructive() && !self.confirm {
            return ("Confirm data erasure above to start.".into(), k::DANGER_ERR);
        }
        if self.needs_admin() && !self.admin {
            return (String::new(), k::TEXT2);
        }
        let free = self.sel_volume().map(|v| v.free).unwrap_or(0);
        let limit = crate::cli::parse_size(&self.size_limit).ok().map_or(free, |l| l.min(free));
        let fake = "Back up your files first. A fake drive can overwrite them during this test.";
        let id = self.sel_disk().map(|d| d.id.clone()).unwrap_or_default();
        let s = match (self.kind, raw) {
            (Kind::Backup, _) => "Reads the selected source and saves a new backup on another disk.".into(),
            (Kind::Restore, false) => "Restores into a new folder. Existing files are never overwritten.".into(),
            (Kind::Test | Kind::Endurance, false) => {
                format!("Writes up to {} into free space. {fake}", size(limit))
            }
            (Kind::Probe, false) => format!("Writes small blocks inside free space only. {fake}"),
            (Kind::PlugPull, false) => {
                format!("Writes into free space until you pull the drive. {fake}")
            }
            (Kind::SmallFiles, false) => {
                format!("Writes {} small files into a test folder.", self.count)
            }
            (Kind::Bench, false) => "Writes and reads one test file.".into(),
            (Kind::Iops, false) => "Reads and writes one test file in free space.".into(),
            (Kind::Verify | Kind::Scan | Kind::Bench | Kind::Iops, _) => "Reads data without writing to the disk.".into(),
            _ => format!("All data on disk {id} will be destroyed."),
        };
        (s, k::TEXT2)
    }

    /// The mode-coloured strip above the run and result views.
    fn slim_bar(&self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
        let mut test = self.kind.name().to_string();
        if self.kind == Kind::Test && self.replug {
            test += ", reconnect before checking data";
        }
        mode_band(ui, rect, self.raw(), self.read_only(), self.protected(), matches!(self.kind, Kind::Backup | Kind::Restore), CornerRadius::same(4), Some((self.target_text(), test)));
    }

    // ---- run view ----

    fn run_view(&mut self, ui: &mut egui::Ui) {
        self.slim_bar(ui);
        let s = std::mem::take(&mut self.live);
        if let Some(p) = &s.prompt {
            self.prompt_banner(ui, p);
        }
        if self.status_block(ui, &s) {
            if let Some(r) = &self.running {
                r.ctx.cancel();
            }
        }
        let cells = gui_map::live(&s, &self.curves, self.raw() && matches!(self.kind, Kind::Scan | Kind::Bench));
        if !matches!(self.kind, Kind::Backup | Kind::Restore) {
            self.viz_row(ui, cells.as_deref(), "Block map", None);
        }
        self.live = s;
        self.log_bar(ui, true);
    }

    fn prompt_banner(&self, ui: &mut egui::Ui, prompt: &str) {
        let (head, sub) = match prompt.find(". ") {
            Some(i) => (&prompt[..=i], prompt[i + 2..].trim()),
            None => (prompt, ""),
        };
        let at = if prompt.starts_with("Drive removed") { 1 } else { 0 };
        let steps = if self.kind == Kind::PlugPull { ["Pull the drive out", "Reconnect", "Check flushed data"] } else { ["Unplug", "Reconnect", "Check data"] };
        Frame::new().fill(k::WARN).corner_radius(6).inner_margin(Margin::symmetric(24, 20)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 22.0;
                k::icon(ui, Icon::Unplug, 56.0, k::WARN_INK);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.allocate_ui_with_layout(vec2(190.0, 74.0), Layout::top_down(Align::Min), |ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;
                        for (i, st) in steps.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                if i != at {
                                    ui.multiply_opacity(0.6);
                                }
                                let (r, _) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
                                let p = ui.painter();
                                let num = (i + 1).to_string();
                                if i == at {
                                    p.rect_filled(r, 0.0, k::WARN_INK);
                                    k::paint_text(p, r.center(), Align2::CENTER_CENTER, &num, 12.0, k::WARN);
                                } else {
                                    p.rect_stroke(r, 0.0, Stroke::new(1.0, k::WARN_INK), StrokeKind::Inside);
                                    k::paint_text(p, r.center(), Align2::CENTER_CENTER, &num, 12.0, k::WARN_INK);
                                }
                                let label = if i == at { format!("{st}: waiting") } else { st.to_string() };
                                ui.label(k::text(label, 14.0, k::WARN_INK));
                            });
                        }
                    });
                    ui.allocate_ui_with_layout(vec2(ui.available_width(), 10.0), Layout::top_down(Align::Min), |ui| {
                        ui.spacing_mut().item_spacing.y = 4.0;
                        ui.label(k::medium("Follow the steps above to continue the test", 13.0, k::WARN_INK));
                        ui.add(Label::new(k::medium(head, 34.0, k::WARN_INK)).wrap());
                        if !sub.is_empty() {
                            ui.add(Label::new(k::text(sub, 16.0, k::WARN_INK)).wrap());
                        }
                    });
                });
            });
        });
    }

    /// Steps, big percentage, stats, error chip and Cancel. Returns true on Cancel.
    fn status_block(&self, ui: &mut egui::Ui, s: &Live) -> bool {
        let waiting = s.prompt.is_some();
        let el = s.phase_start.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        let frac = if s.total > 0 { (s.done as f64 / s.total as f64).clamp(0.0, 1.0) } else { 0.0 };
        let bytes = s.total > 100_000;
        let mut cancel = false;
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 112.0), Sense::hover());

        let right = Rect::from_min_max(pos2(row.right() - 160.0, row.top()), row.max);
        let mut rc = ui.new_child(UiBuilder::new().max_rect(right).layout(Layout::bottom_up(Align::Max)));
        rc.spacing_mut().item_spacing.y = 12.0;
        rc.add_space(6.0);
        cancel |= k::button(&mut rc, Look::Secondary, Some(Icon::Stop), "Cancel", 34.0, 13.0, true).clicked();
        let bad = s.marks.len();
        if bad == 0 {
            k::chip(&mut rc, Some(Icon::Check), "0 errors", 34.0, 18.0, k::OK_TINT, k::OK_STROKE, k::OK_TEXT);
        } else {
            let t = format!("{bad} bad region{}", if bad == 1 { "" } else { "s" });
            k::chip(&mut rc, Some(Icon::Alert), &t, 34.0, 18.0, k::DANGER_TINT, k::DANGER_STROKE, k::DANGER_ERR);
        }

        let mut lc = ui.new_child(UiBuilder::new().max_rect(row).layout(Layout::top_down(Align::Min)));
        lc.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let phases = self.running.as_ref().map(|r| r.phases.as_slice()).unwrap_or_default();
            let n = phases.len();
            for (i, ph) in phases.iter().enumerate().skip(n.saturating_sub(3)) {
                let current = i + 1 == n;
                let color = if current { if waiting { k::WARN } else { k::TEXT } } else { k::TEXT3 };
                let dot = if !current {
                    k::TEXT3
                } else if waiting {
                    k::WARN
                } else {
                    k::OK
                };
                let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                ui.painter().rect(r, 0.0, dot, Stroke::new(1.0, color), StrokeKind::Inside);
                let name = ph.split(" (").next().unwrap_or(ph);
                ui.label(k::medium(name, 14.0, color));
                if !current {
                    k::icon(ui, Icon::Check, 12.0, k::TEXT3);
                    ui.add_space(4.0);
                }
            }
        });
        lc.add_space(8.0);
        let mut job = LayoutJob::default();
        let fmt = |size: f32, color: Color32| TextFormat { font_id: k::light_font(size), color, valign: Align::BOTTOM, ..Default::default() };
        if s.total > 0 {
            job.append(&format!("{:.0}", (frac * 100.0).floor()), 0.0, fmt(76.0, k::TEXT));
            job.append("%", 0.0, fmt(40.0, k::TEXT2));
        } else {
            job.append(&clock(el), 0.0, fmt(76.0, k::TEXT));
        }
        lc.label(job);
        let left_w = lc.min_rect().width();

        let mid = Rect::from_min_max(pos2(row.left() + left_w + 24.0, row.top()), pos2(right.left() - 24.0, row.bottom() - 6.0));
        let (done_label, done) = if bytes {
            let what = if s.phase.contains("Verif") {
                "Verified"
            } else if gui_map::is_read(&s.phase) {
                "Read"
            } else {
                "Written"
            };
            (what, pair(s.done, s.total))
        } else if s.total > 0 {
            ("Done", format!("{} / {}", s.done, s.total))
        } else {
            ("Phase", s.phase.split(" (").next().unwrap_or_default().to_string())
        };
        let (speed_label, speed) = match self.running.as_ref().and_then(|r| r.prompt_since) {
            Some(t) => ("Waiting", clock(t.elapsed().as_secs_f64())),
            None if el > 1.0 && bytes => ("Speed", format!("{:.1} MB/s", s.done as f64 / 1e6 / el)),
            None if el > 1.0 && s.total > 0 => ("Speed", format!("{:.0}/s", s.done as f64 / el)),
            None => ("Speed", "–".into()),
        };
        let eta =
            if s.done > 0 && el > 2.0 && s.total > 0 && !waiting { crate::report::duration(el * s.total.saturating_sub(s.done) as f64 / s.done as f64) } else { "–".into() };
        // Values at 24 pt, or 20 pt when a wide percentage leaves too little room.
        let stats = [(done_label, done), (speed_label, speed), ("Estimated time left", eta)];
        let width = |pt: f32| stats.iter().map(|(_, v)| ui.painter().layout_no_wrap(v.clone(), k::medium_font(pt), k::TEXT).size().x + 24.0).sum::<f32>();
        let pt = if width(24.0) <= mid.width() { 24.0 } else { 20.0 };
        let mut x = mid.left();
        let p = ui.painter_at(mid);
        for (label, value) in stats {
            k::paint_medium(&p, pos2(x, mid.bottom() - 30.0), Align2::LEFT_BOTTOM, label, 11.0, k::TEXT3);
            let r = k::paint_medium(&p, pos2(x, mid.bottom()), Align2::LEFT_BOTTOM, &value, pt, k::TEXT);
            x = r.right().max(x + 90.0) + 24.0;
        }

        ui.add_space(-6.0);
        let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 14.0), Sense::hover());
        let p = ui.painter();
        p.rect(bar, 4.0, k::BAR, Stroke::NONE, StrokeKind::Inside);
        if frac > 0.0 {
            let fill = Rect::from_min_size(bar.min, vec2((bar.width() * frac as f32).max(4.0), bar.height()));
            p.rect_filled(fill, 4.0, if waiting { k::WARN } else { k::OK });
        }
        p.rect_stroke(bar, 4.0, Stroke::new(1.0, k::LINE), StrokeKind::Inside);
        cancel
    }

    /// Block map on the left; speed graph or capacity bar on the right.
    fn viz_row(&self, ui: &mut egui::Ui, cells: Option<&[gui_map::Cell]>, title: &str, capacity: Option<(u64, u64)>) {
        let graph = self.curves.iter().any(|c| c.pts.len() > 1);
        if cells.is_none() && !graph && capacity.is_none() {
            return;
        }
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 20.0;
            if let Some(c) = cells {
                ui.vertical(|ui| {
                    ui.set_width(gui_map::MAP_W);
                    gui_map::show(ui, c, title);
                });
            }
            let w = ui.available_width();
            ui.vertical(|ui| {
                ui.set_width(w);
                if let Some((real, claimed)) = capacity {
                    gui_map::capacity(ui, real, claimed, w);
                } else if graph {
                    gui_map::graph(ui, &self.curves, &self.temps, w);
                }
            });
        });
    }

    fn log_bar(&mut self, ui: &mut egui::Ui, at_bottom: bool) {
        let need = 30.0 + if self.log_open { 236.0 } else { 0.0 };
        let gap = ui.available_height() - need - 16.0;
        if at_bottom && gap > 0.0 {
            ui.add_space(gap);
        }
        // A fixed id: an automatic one can match a widget of the previous view and inherit its state.
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
        let resp = ui.interact(rect, egui::Id::new("log-bar"), Sense::click());
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::CollapsingHeader, true, "Log"));
        let p = ui.painter();
        let cy = rect.center().y;
        p.rect(rect, 4.0, k::BAR, Stroke::new(1.0, if resp.hovered() { k::LINE } else { k::LINE_SUBTLE }), StrokeKind::Inside);
        k::paint_icon(p, Rect::from_center_size(pos2(rect.left() + 16.0, cy), Vec2::splat(12.0)), if self.log_open { Icon::ChevronDown } else { Icon::ChevronRight }, k::TEXT2);
        let lr = k::paint_medium(p, pos2(rect.left() + 32.0, cy), Align2::LEFT_CENTER, "Log", 12.0, k::TEXT2);
        let cr = k::paint_text(p, pos2(rect.right() - 10.0, cy), Align2::RIGHT_CENTER, &format!("{} lines", self.log.len()), 11.0, k::TEXT3);
        if let Some(last) = self.log.last() {
            let g = k::galley(ui, last, FontId::monospace(12.0), cr.left() - 10.0 - (lr.right() + 10.0));
            p.galley(pos2(lr.right() + 10.0, cy - g.size().y / 2.0), g, k::TEXT3);
        }
        k::pointer_on_hover(&resp);
        if resp.clicked() {
            self.log_open = !self.log_open;
        }
        if self.log_open {
            Frame::new().fill(k::BAR).stroke(Stroke::new(1.0, k::LINE_SUBTLE)).corner_radius(4).inner_margin(10).show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical().id_salt("log").max_height(200.0).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    for l in &self.log[self.log.len().saturating_sub(200)..] {
                        ui.add(Label::new(k::mono(l, 12.0, k::TEXT2)).wrap());
                    }
                });
            });
        }
    }

    // ---- result view ----

    fn result_view(&mut self, ui: &mut egui::Ui) {
        let Some(rep) = self.result.take() else {
            return;
        };
        self.slim_bar(ui);
        let mut act = self.verdict(ui, &rep);
        let cells = gui_map::from_report(&rep);
        let capacity = rep.probe.as_ref().map(|p| (p.usable.min(p.claimed), p.claimed));
        if !matches!(self.kind, Kind::Backup | Kind::Restore) {
            self.viz_row(ui, cells.as_deref(), "Final block map", capacity);
        }

        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                if k::button(ui, Look::Secondary, Some(Icon::Download), "Save detailed report", 32.0, 13.0, true).clicked() {
                    act = Some(Act::Save(false));
                }
                let summary_label = if matches!(self.kind, Kind::Backup | Kind::Restore) { "Save summary" } else { "Save evidence sheet" };
                if k::button(ui, Look::Secondary, Some(Icon::FileText), summary_label, 32.0, 13.0, true).clicked() {
                    act = Some(Act::Save(true));
                }
                if rep.target.disk.is_some() {
                    // The checkbox lays out from the top: centre it on the 32 px buttons.
                    ui.vertical(|ui| {
                        ui.add_space(7.0);
                        k::checkbox(ui, &mut self.hide_serial, 16.0, k::TEXT3, k::ACCENT, k::text("Hide serial numbers", 13.0, k::TEXT2), None);
                    });
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let r = ui
                        .horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            k::icon(ui, if self.summary_open { Icon::ChevronDown } else { Icon::ChevronRight }, 12.0, k::TEXT2);
                            ui.label(k::text("Operation details", 12.0, k::TEXT2));
                        })
                        .response;
                    let r = ui.interact(r.rect, egui::Id::new("summary-toggle"), Sense::click());
                    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::CollapsingHeader, true, "Operation details"));
                    k::pointer_on_hover(&r);
                    if r.clicked() {
                        self.summary_open = !self.summary_open;
                    }
                });
            });
            self.saved_reports(ui);
            if self.summary_open {
                Frame::new().fill(k::BAR).stroke(Stroke::new(1.0, k::LINE_SUBTLE)).corner_radius(4).inner_margin(10).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical().id_salt("summary").max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
                        ui.add(Label::new(k::mono(rep.text(), 12.0, k::TEXT2)).wrap());
                    });
                });
            }
        });
        self.result = Some(rep);
        self.log_bar(ui, true);

        match act {
            Some(Act::Save(evidence)) => self.save(evidence),
            Some(Act::Rerun(kd)) => {
                self.kind = kd;
                self.confirm = false;
                self.result = None;
            }
            Some(Act::Fix(id)) => {
                self.select(Sel::Disk(id));
                self.kind = Kind::Fix;
            }
            Some(Act::ChooseDrive) => {
                self.sel = None;
                self.result = None;
                self.saved.clear();
            }
            None => {}
        }
    }

    fn verdict(&self, ui: &mut egui::Ui, rep: &Report) -> Option<Act> {
        let st = rep.verdict.status;
        let (solid, ink, tint, stroke) = match st {
            Status::Ok => (k::OK, k::OK_INK, k::OK_TINT, k::OK_STROKE),
            Status::Unreliable | Status::Cancelled => (k::WARN, k::WARN_INK, k::WARN_TINT, k::WARN_STROKE),
            Status::Info => (k::ACCENT, k::DONE_INK, k::SEL, k::LINE),
            _ => (k::DANGER, k::DANGER_INK, k::DANGER_TINT, k::DANGER_STROKE),
        };
        let disk = rep.target.disk.as_ref().map(|d| d.id.clone()).or_else(|| self.sel_disk().map(|d| d.id.clone()));
        let mut act = None;
        let resp = Frame::new().fill(tint).stroke(Stroke::new(1.0, stroke)).corner_radius(6).inner_margin(Margin { left: 0, right: 20, top: 16, bottom: 16 }).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.add_space(150.0);
                ui.vertical(|ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 8.0;
                    ui.add(Label::new(k::medium(&rep.verdict.headline, 20.0, k::TEXT)).wrap());
                    ui.scope(|ui| {
                        ui.spacing_mut().item_spacing.y = 3.0;
                        for d in &rep.verdict.details {
                            ui.add(Label::new(k::text(d, 13.0, k::TEXT2B)).wrap());
                        }
                        for w in &rep.warnings {
                            ui.add(Label::new(k::text(format!("! {w}"), 13.0, k::WARN_TEXT)).wrap());
                        }
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        ui.label(k::medium("Next step", 12.0, k::TEXT2));
                        let mut b = |look, label: &str, a: Act| {
                            if k::button(ui, look, None, label, 32.0, 13.0, true).clicked() {
                                act = Some(a);
                            }
                        };
                        let evidence = Look::Solid(k::DANGER, k::DANGER_INK);
                        match st {
                            Status::Fake => {
                                b(evidence, "Save evidence sheet", Act::Save(true));
                                if let Some(id) = disk.clone() {
                                    b(Look::Outline(k::DANGER_STROKE), "Resize and format", Act::Fix(id));
                                }
                            }
                            Status::Defective | Status::ReadOnly => {
                                b(evidence, "Save evidence sheet", Act::Save(true));
                                if !self.protected() && !rep.read_only {
                                    b(Look::Outline(k::DANGER_STROKE), "Run full test again", Act::Rerun(Kind::Test));
                                }
                            }
                            _ => b(Look::Plain, "Choose another drive", Act::ChooseDrive),
                        }
                    });
                });
            });
        });
        let r = resp.response.rect;
        let col = Rect::from_min_size(r.min, vec2(130.0, r.height()));
        let p = ui.painter();
        p.rect_filled(col, CornerRadius { nw: 6, sw: 6, ne: 0, se: 0 }, solid);
        let word = st.label();
        let size = if word.len() > 6 { 22.0 } else { 34.0 };
        let wr = k::paint_medium(p, col.center() + vec2(0.0, 8.0), Align2::CENTER_CENTER, word, size, ink);
        k::paint_medium(p, pos2(col.center().x, wr.top() - 2.0), Align2::CENTER_BOTTOM, "Verdict", 11.0, ink.gamma_multiply(0.8));
        act
    }
}

/// Keep actions below the path so long file names cannot push them off-screen.
fn saved_report_row(ui: &mut egui::Ui, path: &std::path::Path) -> Option<PathBuf> {
    let mut open = None;
    ui.push_id(path, |ui| {
        ui.label(k::mono("Saved", 12.0, k::OK));
        ui.add(Label::new(k::mono(path.display().to_string(), 12.0, k::TEXT2)).wrap());
        ui.horizontal_wrapped(|ui| {
            if k::button(ui, Look::Secondary, None, "Open file", 28.0, 12.0, true).clicked() {
                open = Some(path.to_path_buf());
            }
            if let Some(parent) = path.parent() {
                if k::button(ui, Look::Plain, None, "Open folder", 28.0, 12.0, true).clicked() {
                    open = Some(parent.to_path_buf());
                }
            }
        });
    });
    open
}

/// A file-mode / raw-mode strip. With `slim`, it also carries the target and test name.
fn mode_band(ui: &egui::Ui, rect: Rect, raw: bool, read_only: bool, protected: bool, transfer: bool, radius: CornerRadius, slim: Option<((String, String), String)>) {
    let (fill, fg, stroke) = if raw && !read_only { (k::DANGER_TINT, k::DANGER_BAND, k::DANGER_STROKE) } else { (k::SEL, k::LINK_LIGHT, k::LINE) };
    let p = ui.painter();
    let cy = rect.center().y;
    let (label, line) = if protected {
        ("Protected drive (read only)", "Writes and file tests are blocked.")
    } else if transfer {
        match (raw, read_only) {
            (true, true) => ("Disk image backup", "Reads every sector; stores the image on another disk."),
            (true, false) => ("Disk image restore", "Overwrites the destination disk."),
            (false, true) => ("File backup", "Reads files into a ZIP archive on another disk."),
            (false, false) => ("File restore", "Restores files into a new folder."),
        }
    } else if read_only {
        ("Read only", "No test data is written to the drive.")
    } else if raw {
        ("Raw mode", "Whole-disk access. Write tests erase data.")
    } else {
        ("File mode", "Tests free space. A fake drive can overwrite existing files.")
    };
    match slim {
        None => {
            p.rect_filled(rect, radius, fill);
            let r = k::paint_medium(p, pos2(rect.left() + 13.0, cy), Align2::LEFT_CENTER, label, 14.0, fg);
            p.galley(pos2(r.right() + 10.0, cy - 8.0), k::galley(ui, line, FontId::proportional(13.0), rect.right() - r.right() - 24.0), fg);
        }
        Some(((title, sub), test)) => {
            p.rect(rect, radius, fill, Stroke::new(1.0, stroke), StrokeKind::Inside);
            let r = k::paint_medium(p, pos2(rect.left() + 12.0, cy), Align2::LEFT_CENTER, label, 14.0, fg);
            let t = k::paint_text(p, pos2(r.right() + 12.0, cy), Align2::LEFT_CENTER, &title, 13.0, k::TEXT);
            let n = k::paint_text(p, pos2(rect.right() - 12.0, cy), Align2::RIGHT_CENTER, &test, 12.0, k::TEXT2);
            let g = k::galley(ui, &sub, FontId::proportional(13.0), n.left() - 12.0 - (t.right() + 12.0));
            p.galley(pos2(t.right() + 12.0, cy - g.size().y / 2.0), g, fg);
        }
    }
}

fn info_card(ui: &mut egui::Ui, fill: Color32, stroke: Color32, head: &str, head_color: Color32, title: &str, sub: &str) {
    Frame::new().fill(fill).stroke(Stroke::new(1.0, stroke)).corner_radius(6).inner_margin(16).show(ui, |ui| {
        ui.vertical(|ui| {
            ui.set_width(ui.available_width().min(268.0));
            ui.spacing_mut().item_spacing.y = 5.0;
            ui.label(k::medium(head, 13.0, head_color));
            ui.add(Label::new(k::medium(title, 16.0, k::TEXT)).wrap());
            ui.add(Label::new(k::text(sub, 13.0, k::TEXT2)).wrap());
        });
    });
}

/// Writes `rep` as JSON and `body` as HTML into new files in their folder, for the
/// desktop user (the GUI may run elevated).
fn write_report(rep: &Report, json: &std::path::Path, html: &std::path::Path, body: &str) -> std::io::Result<()> {
    let dir = html.parent().unwrap_or(html);
    let name = |p: &std::path::Path| p.file_name().unwrap_or_default().to_string_lossy().into_owned();
    platform::write_user_files(dir, &[(&name(json), serde_json::to_string_pretty(rep)?.as_bytes()), (&name(html), body.as_bytes())])
}

/// The SMART view's content: alarms, the important counters, then every counter.
fn smart_body(ui: &mut egui::Ui, s: &Smart, raw_open: &mut bool) {
    ui.add(Label::new(k::text("Values as the drive's firmware reports them. FlashTester cannot check them, and the firmware of a fake drive can lie.", 13.0, k::TEXT2)).wrap());
    if !s.alarms.is_empty() {
        ui.horizontal_wrapped(|ui| {
            for a in &s.alarms {
                k::chip(ui, Some(Icon::Alert), a, 24.0, 12.0, k::DANGER_TINT, k::DANGER_STROKE, k::DANGER_TEXT);
            }
        });
    }
    Frame::new().fill(k::PANEL).stroke(Stroke::new(1.0, k::LINE_CARD)).corner_radius(6).inner_margin(16).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 10.0;
        ui.label(k::medium("Most important", 12.0, k::TEXT3));
        if s.key.is_empty() {
            ui.label(k::text("The drive reports none of the usual counters.", 13.0, k::TEXT2));
        }
        egui::Grid::new("smart-key").num_columns(2).spacing(vec2(40.0, 10.0)).show(ui, |ui| {
            for key in &s.key {
                let warn = key.what.bad() && key.value > 0;
                ui.label(k::text(key.what.label(), 14.0, k::TEXT2)).on_hover_text(key.what.help());
                ui.label(k::medium(key.what.show(key.value), 14.0, if warn { k::WARN_TEXT } else { k::TEXT }));
                ui.end_row();
            }
        });
    });
    let r = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            k::icon(ui, if *raw_open { Icon::ChevronDown } else { Icon::ChevronRight }, 12.0, k::TEXT2);
            ui.label(k::text(format!("All counters ({})", s.raw.len()), 13.0, k::TEXT2));
        })
        .response;
    let r = ui.interact(r.rect, egui::Id::new("smart-raw-toggle"), Sense::click());
    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::CollapsingHeader, true, "All counters"));
    k::pointer_on_hover(&r);
    if r.clicked() {
        *raw_open = !*raw_open;
    }
    if !*raw_open {
        return;
    }
    let ata = s.raw.iter().any(|r| !r.worst.is_empty());
    Frame::new().fill(k::BAR).stroke(Stroke::new(1.0, k::LINE_SUBTLE)).corner_radius(4).inner_margin(12).show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::Grid::new("smart-raw").striped(false).spacing(vec2(24.0, 6.0)).show(ui, |ui| {
            let head: &[&str] = if ata { &["ID", "Attribute", "Value", "Worst", "Threshold", "Raw"] } else { &["Field", "Value"] };
            for h in head {
                ui.label(k::medium(*h, 11.0, k::TEXT3));
            }
            ui.end_row();
            for r in &s.raw {
                if ata {
                    ui.label(k::mono(&r.id, 12.0, k::TEXT3));
                    ui.label(k::text(&r.name, 13.0, k::TEXT2));
                    for c in [&r.value, &r.worst, &r.thresh, &r.raw] {
                        ui.label(k::mono(c, 12.0, k::TEXT));
                    }
                } else {
                    ui.label(k::text(&r.name, 13.0, k::TEXT2));
                    ui.label(k::mono(&r.value, 12.0, k::TEXT));
                }
                ui.end_row();
            }
        });
        if ata {
            ui.add_space(6.0);
            ui.add(Label::new(k::text("Names are the usual meanings; vendors reuse numbers for other things. Pre-fail: the drive treats reaching the threshold as imminent failure.", 12.0, k::TEXT3)).wrap());
        }
    });
}

/// "E:" for "E:\", the mount point elsewhere.
fn vol_key(m: &str) -> String {
    let t = m.trim_end_matches(['\\', '/']);
    if t.is_empty() { m.to_string() } else { t.to_string() }
}

/// Key and label of a volume row. A Unix mount point usually ends in the label
/// already (/media/user/STICK), so the label is shown only when it adds something.
fn vol_parts(m: &str, label: &str) -> (String, String) {
    let key = vol_key(m);
    let label = match label {
        "" if key.ends_with(':') => "(no label)",
        l if key.rsplit('/').next() == Some(l) => "",
        l => l,
    };
    (key, label.to_string())
}

fn disk_icon(d: &DiskInfo) -> Icon {
    if d.protected.is_some() {
        return Icon::Lock;
    }
    let n = d.name().to_ascii_uppercase();
    if n.contains("CARD") || n.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| matches!(w, "SD" | "SDHC" | "SDXC" | "MMC" | "MICROSD")) {
        Icon::Sd
    } else if d.usb.is_some() || d.bus.eq_ignore_ascii_case("usb") {
        Icon::Usb
    } else {
        Icon::Drive
    }
}

fn usb_version(v: &str) -> String {
    v.parse::<f32>().map(|f| format!("USB {f:.1}")).unwrap_or_else(|_| format!("USB {v}"))
}

/// Under the USB ID: USB version, controller firmware release and who the vendor ID belongs to.
fn usb_sub(u: &crate::platform::UsbInfo) -> String {
    let mut s = usb_version(&u.usb_version);
    if !u.release.is_empty() {
        s += &format!(", firmware {}", u.release);
    }
    if let Some(n) = crate::ids::usb_vendor(&u.vid).or_else(|| Some(u.db_vendor.clone()).filter(|v| !v.is_empty())) {
        s += &format!(", {n}");
    }
    s
}

/// Under the device name: its controller or bridge chip, or an SD card's maker and date.
fn device_sub(d: &DiskInfo) -> String {
    match &d.card {
        Some(c) => format!("{} card by {}, made {}", c.kind, if c.maker.is_empty() { "unknown maker" } else { &c.maker }, c.date),
        None => d.controller.clone(),
    }
}

fn bus_text(d: &DiskInfo) -> String {
    d.usb.as_ref().map(|u| usb_version(&u.usb_version)).unwrap_or_else(|| d.bus.clone())
}

/// 128043712512 -> "128,043,712,512".
fn digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "88.3 / 127.9 GB": both numbers in the unit of the total.
fn pair(done: u64, total: u64) -> String {
    let (div, unit) = match total {
        t if t >= 1_000_000_000_000 => (1e12, "TB"),
        t if t >= 1_000_000_000 => (1e9, "GB"),
        _ => (1e6, "MB"),
    };
    format!("{:.1} / {:.1} {unit}", done as f64 / div, total as f64 / div)
}

fn clock(secs: f64) -> String {
    let s = secs as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

impl Gui {
    #[cfg(windows)]
    fn relaunch(&mut self, _ctx: &egui::Context) {
        if platform::relaunch_elevated("") {
            std::process::exit(0);
        }
    }

    /// pkexec refuses to serve a dead parent, so this process hides its window and
    /// waits: it exits when the root copy closes normally, and comes back otherwise
    /// (password dialog cancelled, root copy failed).
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn relaunch(&mut self, ctx: &egui::Context) {
        match platform::relaunch_root() {
            Ok(c) => {
                self.elevating = Some(c);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
            Err(e) => self.error = Some(e),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn poll_relaunch(&mut self, ctx: &egui::Context) {
        let Some(c) = &mut self.elevating else { return };
        let msg = match c.try_wait() {
            Ok(None) => return ctx.request_repaint_after(Duration::from_millis(250)),
            Ok(Some(st)) if st.success() => std::process::exit(0),
            Ok(Some(st)) if matches!(st.code(), Some(126 | 127)) => "Not authorised: still running without root.".to_string(),
            Ok(Some(st)) => format!("The root copy stopped ({st})."),
            Err(e) => e.to_string(),
        };
        self.elevating = None;
        self.error = Some(msg);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        if !self.styled {
            // New fonts apply from the next pass: draw nothing until then.
            k::style(ui.ctx());
            self.styled = true;
            ui.ctx().request_discard("fonts");
            ui.ctx().request_repaint();
            return;
        }
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        self.poll_relaunch(ui.ctx());
        self.poll();
        if self.running.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
        egui::Panel::top("top").exact_size(44.0).resizable(false).frame(Frame::new().fill(k::BAR).inner_margin(Margin::symmetric(16, 0))).show(ui, |ui| self.top_bar(ui));
        egui::Panel::left("drives").exact_size(300.0).resizable(false).frame(Frame::new().fill(k::PANEL)).show(ui, |ui| self.sidebar(ui));
        // A small top margin centres the back button on the drive list's Refresh button.
        egui::CentralPanel::default().frame(Frame::new().fill(k::WINDOW).inner_margin(Margin { left: 24, right: 24, top: 8, bottom: 20 })).show(ui, |ui| {
            self.back_bar(ui);
            egui::ScrollArea::vertical().id_salt("main").auto_shrink(false).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 16.0;
                self.main_view(ui);
            });
        });
    }
}

#[cfg(windows)]
impl eframe::App for Gui {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

#[cfg(windows)]
pub fn run() -> i32 {
    platform::hide_own_console();
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 760.0]).with_min_inner_size([960.0, 640.0]).with_title("FlashTester"),
        ..Default::default()
    };
    match eframe::run_native("FlashTester", opts, Box::new(|_cc| Ok(Box::new(Gui::new())))) {
        Ok(()) => 0,
        Err(e) => {
            // No OpenGL (e.g. remote desktop without GPU): point to the CLI.
            let msg = format!(
                "Cannot start the GUI ({e}).

Use the command line instead: flashtester --help"
            );
            eprintln!("{msg}");
            platform::error_box("FlashTester", &msg);
            3
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn run() -> i32 {
    let mut gui = Gui::new();
    match platform::x11gui::run("FlashTester", [1100.0, 760.0], [960.0, 640.0], |ui| gui.show(ui)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!(
                "Cannot start the GUI ({e}).

Use the command line instead: flashtester --help"
            );
            3
        }
    }
}
