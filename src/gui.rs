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

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Test => "Full test",
            Kind::Endurance => "Endurance",
            Kind::Verify => "Verify existing data",
            Kind::Probe => "Quick capacity probe",
            Kind::Scan => "Surface scan (read-only)",
            Kind::Iops => "Random 4K speed",
            Kind::SmallFiles => "Many small files",
            Kind::PlugPull => "Pull-the-plug test",
            Kind::Fix => "Fix: repartition to real size",
            Kind::Bench => "Sequential speed",
        }
    }
    fn start_label(self) -> &'static str {
        match self {
            Kind::Test => "Start full test",
            Kind::Endurance => "Start endurance test",
            Kind::Verify => "Start verify",
            Kind::Probe => "Start probe",
            Kind::Scan => "Start scan",
            Kind::Iops => "Start 4K test",
            Kind::SmallFiles => "Start small-files test",
            Kind::PlugPull => "Start pull-the-plug test",
            Kind::Fix => "Start fix",
            Kind::Bench => "Start speed test",
        }
    }
    fn group(self) -> &'static str {
        match self {
            Kind::Test | Kind::Probe | Kind::Verify | Kind::Scan => "Capacity & integrity",
            Kind::Bench | Kind::Iops => "Speed",
            Kind::Endurance | Kind::SmallFiles | Kind::PlugPull => "Trust",
            Kind::Fix => "Repair",
        }
    }
    fn help(self, raw: bool) -> &'static str {
        match (self, raw) {
            (Kind::Test, false) => "Fill the free space with a seeded pattern, then read it all back. Stops early if a fake drive starts overwriting itself. Keeps your files.",
            (Kind::Test, true) => "Write the whole disk with a seeded pattern, then read it all back. DESTROYS all data on the disk.",
            (Kind::Endurance, _) => "Repeat write + verify with a new seed each pass, until an error or Cancel.",
            (Kind::Verify, _) => "Read back data written by an earlier test (retention check) without rewriting it.",
            (Kind::Probe, true) => "Find the real capacity in minutes by writing small blocks across the whole disk. DESTROYS the partition table.",
            (Kind::Probe, false) => {
                if cfg!(windows) {
                    "Find the real capacity in minutes, writing only inside free space (needs administrator)."
                } else {
                    "Find the real capacity in minutes, writing only inside free space (needs root)."
                }
            }
            (Kind::Scan, _) => "Read every sector and map unreadable and slow regions. Changes nothing.",
            (Kind::Iops, _) => "Random 4K reads and writes at queue depth 1 and 32 (about 40 seconds).",
            (Kind::SmallFiles, _) => "Create, rename and delete thousands of small files, then verify them: file-system integrity and files/s.",
            (Kind::PlugPull, _) => "Write with periodic flushes; you pull the drive out mid-write. Afterwards every flushed byte must still be there.",
            (Kind::Fix, _) => "Probe the real capacity, then repartition and format the drive to that size. DESTROYS all data.",
            (Kind::Bench, false) => "Write and read a test file sequentially and graph the speed. Measures speed only: it does not check the capacity.",
            (Kind::Bench, true) => "Read the start of the disk sequentially and graph the speed (read-only). Measures speed only: it does not check the capacity.",
        }
    }
    /// In the order the test list shows them; the first one is the suggested default.
    fn for_target(raw: bool) -> &'static [Kind] {
        if raw {
            &[Kind::Probe, Kind::Test, Kind::Verify, Kind::Scan, Kind::Bench, Kind::Iops, Kind::Endurance, Kind::Fix]
        } else {
            &[Kind::Test, Kind::Probe, Kind::Verify, Kind::Bench, Kind::Iops, Kind::Endurance, Kind::SmallFiles, Kind::PlugPull]
        }
    }
    fn erases(self, raw: bool) -> bool {
        raw && matches!(self, Kind::Test | Kind::Endurance | Kind::Probe | Kind::Fix)
    }
    fn needs_admin(self, raw: bool) -> bool {
        raw || self == Kind::Probe
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
    Again,
    Rerun(Kind),
    Fix(String),
    ChooseDrive,
    Open(PathBuf),
}

pub struct Gui {
    disks: Vec<DiskInfo>,
    sel: Option<Sel>,
    kind: Kind,
    size_limit: String,
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
    error: Option<String>,
    admin: bool,
    styled: bool,
    /// Height of the guards + Start block in the last frame.
    foot_h: f32,
    #[cfg(target_os = "linux")]
    elevating: Option<std::process::Child>,
}

const ADMIN: &str = if cfg!(windows) { "administrator" } else { "root" };
const ADMIN_TITLE: &str = if cfg!(windows) { "Administrator" } else { "Root" };

impl Gui {
    fn new() -> Self {
        let mut g = Gui {
            disks: vec![],
            sel: None,
            kind: Kind::Test,
            size_limit: String::new(),
            passes: 0,
            replug: false,
            count: 5000,
            fs: "exfat".into(),
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
            summary_open: false,
            error: None,
            admin: platform::is_admin(),
            styled: false,
            foot_h: 36.0,
            #[cfg(target_os = "linux")]
            elevating: None,
        };
        g.refresh();
        g
    }

    fn refresh(&mut self) {
        self.disks = platform::list_disks();
        // Removable first; the order within a group stays as the OS lists it.
        self.disks.sort_by_key(|d| !d.removable);
        if self.sel.is_none() {
            // Preselect the first testable removable volume.
            self.sel = self
                .disks
                .iter()
                .filter(|d| d.protected.is_none() && d.removable)
                .flat_map(|d| d.volumes.iter().flat_map(|v| v.mount_points.first().cloned()))
                .next()
                .map(Sel::Volume);
        }
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
        let raw = matches!(s, Sel::Disk(_));
        self.sel = Some(s);
        self.kind = Kind::for_target(raw)[0];
        self.confirm = false;
        self.smart = None;
        self.result = None;
        self.error = None;
        self.saved.clear();
    }

    fn build_job(&self) -> Result<Job, String> {
        let w: Where = match &self.sel {
            Some(Sel::Volume(m)) => app::resolve_path(m)?,
            Some(Sel::Disk(n)) => app::resolve_disk(n)?,
            None => return Err("Select a drive first.".into()),
        };
        let size = if self.size_limit.trim().is_empty() { None } else { Some(crate::cli::parse_size(&self.size_limit)?) };
        let opts = |passes| TestOpts { size, passes, ..Default::default() };
        Ok(match self.kind {
            Kind::Test => Job::Test { w, o: TestOpts { replug: self.replug, ..opts(1) } },
            Kind::Endurance => Job::Test { w, o: opts(self.passes) },
            Kind::Verify => Job::Verify { w, h2w: false, manifest: None, loops: 1 },
            Kind::Probe => Job::Probe { w },
            Kind::Scan => Job::Scan { w, size: None },
            Kind::Iops => Job::Iops { w, secs: 8.0, write: false },
            Kind::SmallFiles => Job::SmallFiles { w, count: self.count, keep: false },
            Kind::PlugPull => Job::PlugPull { w, checkpoint: 256 << 20 },
            Kind::Bench => Job::Bench { w, size: size.unwrap_or(1 << 30), write: false },
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
        matches!(self.kind, Kind::Test | Kind::Endurance | Kind::Bench)
    }

    fn can_start(&self) -> bool {
        self.sel.is_some() && (!self.uses_size() || self.size_error().is_none()) && (!self.destructive() || self.confirm) && (!self.needs_admin() || self.admin)
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
        let confirmed = self.sel_disk().map(|d| d.instance_id.clone());
        let resolved = match &job {
            Job::Test { w, .. } | Job::Probe { w } | Job::Iops { w, .. } => w.disk().map(|d| d.instance_id.clone()),
            Job::Fix { disk, .. } => Some(disk.instance_id.clone()),
            _ => confirmed.clone(),
        };
        if job.destructive() && (!self.confirm || resolved != confirmed) {
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
        self.summary_open = false;
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
        let Some(rep) = &self.result else { return };
        let dir = platform::reports_dir();
        let _ = std::fs::create_dir_all(&dir);
        platform::give_to_user(&dir);
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let (name, body) = if evidence { (format!("evidence-{stamp}.html"), rep.evidence_html()) } else { (format!("report-{}-{stamp}.html", rep.command), rep.html()) };
        let p = dir.join(name);
        let json = dir.join(format!("report-{}-{stamp}.json", rep.command));
        let _ = std::fs::write(&json, serde_json::to_string_pretty(rep).unwrap());
        platform::give_to_user(&json);
        match std::fs::write(&p, body) {
            Ok(()) => {
                platform::give_to_user(&p);
                platform::open_file(&p);
                self.saved.push(p);
                self.saved.push(json);
            }
            Err(e) => self.error = Some(format!("Cannot save: {e}")),
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
                    Some(v) => format!("Volume on {disk} · {name} · {} · {} free", v.fs, size(v.free)),
                    None => format!("Volume on {disk}"),
                };
                (if label.is_empty() { key } else { format!("{key}  {label}") }, sub)
            }
            Some(Sel::Disk(id)) => (format!("Disk {id} · whole disk"), format!("{name} · {} · all partitions", d.map(|d| size(d.size)).unwrap_or_default())),
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
                    ui.label(k::text(format!("Not elevated: raw-mode tests need {ADMIN} rights"), 12.0, k::WARN));
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
                    ui.label(k::text("Locked while testing", 12.0, k::TEXT3));
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
            if d.protected.is_some() {
                ui.multiply_opacity(0.55);
            }
            Frame::new()
                .fill(k::DISK)
                .stroke(Stroke::new(1.0, if mine { k::LINE_STRONG } else { k::LINE_CARD }))
                .corner_radius(6)
                .inner_margin(Margin { left: 6, right: 6, top: 0, bottom: 6 })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 4.0;
                    let (h, _) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::hover());
                    let p = ui.painter();
                    let ix = h.left() + 4.0;
                    k::paint_icon(p, Rect::from_center_size(pos2(ix + 10.0, h.center().y), Vec2::splat(16.0)), disk_icon(d), k::TEXT2);
                    let sz = k::paint_medium(p, pos2(h.right() - 4.0, h.center().y), Align2::RIGHT_CENTER, &size(d.size), 16.0, k::TEXT);
                    let tx = ix + 28.0;
                    let max_w = sz.left() - 8.0 - tx;
                    p.galley(pos2(tx, h.center().y - 17.0), k::galley(ui, &d.name(), k::medium_font(14.0), max_w), k::TEXT);
                    p.galley(pos2(tx, h.center().y + 1.0), k::galley(ui, &format!("Disk {} · {}", d.id, bus_text(d)), FontId::proportional(12.0), max_w), k::TEXT3);
                    if let Some(why) = &d.protected {
                        ui.horizontal(|ui| {
                            ui.add_space(32.0);
                            ui.add(Label::new(k::text(format!("Protected: {why}. Can't be selected."), 12.0, k::TEXT2)).wrap());
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
                    if self.choice_row(ui, &s, "", "Whole disk", "erases all data", locked).clicked() {
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
        let raw = matches!(s, Sel::Disk(_));
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), if locked { Sense::hover() } else { Sense::click() });
        let on = self.sel.as_ref() == Some(s);
        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, !locked, on, format!("{key} {title}, {right}").trim()));
        let (fill, stroke, ring, dot) = k::row_colors(on, raw, resp.hovered() && !locked, k::PANEL, k::LINE_CARD);
        let p = ui.painter();
        let cy = rect.center().y;
        p.rect(rect, 4.0, fill, Stroke::new(1.0, stroke), StrokeKind::Inside);
        k::radio(p, pos2(rect.left() + 15.0, cy), ring, dot);
        let rr = k::paint_text(p, pos2(rect.right() - 8.0, cy), Align2::RIGHT_CENTER, right, 12.0, if raw { k::DANGER_TEXT } else { k::TEXT2 });
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
                ui.set_max_width(640.0);
                ui.spacing_mut().item_spacing.y = 8.0;
                ui.label(k::light("Select a drive to test", 40.0, k::TEXT));
                ui.add(Label::new(k::text("Pick a volume or a whole disk in the list on the left. What you pick decides what the test may touch.", 15.0, k::TEXT2)).wrap());
                ui.add_space(20.0);
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 20.0;
                    info_card(
                        ui,
                        k::PANEL,
                        k::LINE_CARD,
                        "Volume: file mode",
                        k::LINK,
                        "Tests free space only",
                        if cfg!(windows) {
                            "Pick a drive letter, like E:. On a fake drive, writes can reach your files."
                        } else {
                            "Pick a mounted volume, like /media/stick. On a fake drive, writes can reach your files."
                        },
                    );
                    info_card(
                        ui,
                        k::EMPTY_RAW,
                        k::EMPTY_RAW_STROKE,
                        "Whole disk: raw mode",
                        k::DANGER,
                        "Tests full capacity, destroys all data",
                        &format!("Pick “Whole disk” under a drive. Needs {ADMIN} rights."),
                    );
                });
                ui.add_space(20.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    for (t, c) in [
                        ("Not sure? Pick the volume of your new stick and run ", k::TEXT2),
                        ("Quick capacity probe", k::TEXT),
                        (", then a ", k::TEXT2),
                        ("Full test", k::TEXT),
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
        self.test_list(&mut ui.new_child(UiBuilder::new().max_rect(left)), raw);
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
            .stroke(Stroke::new(1.0, if raw { k::DANGER_STROKE } else { k::LINE }))
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
        mode_band(ui, Rect::from_min_size(r.min, vec2(r.width(), 30.0)).shrink(1.0), raw, CornerRadius { nw: 5, ne: 5, sw: 0, se: 0 }, None);
        smart
    }

    /// True when the SMART button was clicked.
    fn identity(&self, ui: &mut egui::Ui) -> bool {
        let Some(d) = self.sel_disk() else { return false };
        let mut smart = false;
        let button = if smart::possible(d) { k::button_width(ui, "SMART data", 28.0, 12.0, false) + 12.0 } else { 0.0 };
        let usb = d.usb.as_ref();
        let items = [
            ("Device", d.name(), String::new(), false),
            ("Serial", if d.serial.is_empty() { "(none)".into() } else { d.serial.clone() }, String::new(), d.placeholder_serial.is_some()),
            ("Capacity", size(d.size), format!("{} bytes", digits(d.size)), false),
            ("USB", usb.map(|u| format!("{}:{}", u.vid, u.pid)).unwrap_or_else(|| "-".into()), usb.map(|u| usb_version(&u.usb_version)).unwrap_or_default(), false),
            ("Link", usb.map(|u| u.link.clone()).filter(|l| !l.is_empty()).unwrap_or_else(|| d.bus.clone()), String::new(), usb.is_some_and(|u| u.slow_link)),
        ];
        let fr = [1.0, 1.3, 1.6, 1.4, 0.8];
        let unit = (ui.available_width() - 4.0 * 12.0 - button) / fr.iter().sum::<f32>();
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            for ((label, value, sub, warn), f) in items.into_iter().zip(fr) {
                ui.allocate_ui_with_layout(vec2(unit * f, 40.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(unit * f);
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(k::medium(label, 11.0, k::TEXT3));
                    ui.add(Label::new(k::text(value, 13.0, if warn { k::WARN_TEXT } else { k::TEXT })).truncate());
                    if !sub.is_empty() {
                        ui.label(k::mono(sub, 11.0, k::TEXT3));
                    }
                });
            }
            if button > 0.0 {
                ui.add_space(-12.0 + (ui.available_width() - button).max(0.0));
                smart = k::button(ui, Look::Secondary, None, "SMART data", 28.0, 12.0, true)
                    .on_hover_text("The drive's own counters: errors, temperature, hours, data written")
                    .clicked();
            }
        });
        let mut warns = vec![];
        if let Some(p) = &d.placeholder_serial {
            warns.push(format!("Suspicious identity: {p}"));
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
                if raw { "Not sure? Run the Quick capacity probe first. If it passes, run a Full test." } else { "Not sure? Run the Quick capacity probe, then a Full test." };
            ui.add(Label::new(k::text(hint, 12.0, k::TEXT2)).wrap());
        });
        ui.add_space(4.0);
        let mut group = "";
        for &kd in Kind::for_target(raw) {
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
                self.confirm = false;
                self.error = None;
            }
        }
    }

    fn test_row(&self, ui: &mut egui::Ui, kd: Kind, raw: bool) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
        let on = self.kind == kd;
        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, on, kd.name()));
        let (fill, stroke, ring, dot) = k::row_colors(on, raw, resp.hovered(), Color32::TRANSPARENT, Color32::TRANSPARENT);
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
        if kd == Kind::for_target(raw)[0] {
            x = k::paint_medium(p, pos2(x, cy), Align2::RIGHT_CENTER, "Suggested", 11.0, k::LINK).left() - 8.0;
        }
        let left = rect.left() + 34.0;
        let g = k::galley(ui, kd.name(), FontId::proportional(14.0), x - left);
        p.galley(pos2(left, cy - g.size().y / 2.0), g, if on { k::TEXT } else { k::TEXT2B });
        k::pointer_on_hover(&resp);
        resp
    }

    fn test_detail(&mut self, ui: &mut egui::Ui, raw: bool) {
        let inner = ui.max_rect();
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(k::medium(self.kind.group(), 12.0, k::TEXT3));
        ui.add_space(2.0);
        ui.label(k::medium(self.kind.name(), 30.0, k::TEXT));
        ui.add_space(6.0);
        ui.scope(|ui| {
            ui.set_max_width(ui.available_width().min(440.0));
            ui.add(Label::new(k::text(self.kind.help(raw), 14.0, k::TEXT2)).wrap());
        });
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
                        ui.label(k::medium(format!("This test needs {ADMIN} rights"), 14.0, k::WARN_TEXT));
                        ui.add(Label::new(k::text(format!("FlashTester restarts with {ADMIN} rights; pick the drive and test again there."), 12.0, k::WARN_SUB)).wrap());
                    });
                    relaunch = k::button(ui, Look::Amber, None, &label, 32.0, 13.0, true).clicked();
                });
            });
        }
        if destructive {
            Frame::new().fill(k::DANGER_TINT).stroke(Stroke::new(1.0, k::DANGER_STROKE)).corner_radius(4).inner_margin(14).show(&mut ui, |ui| {
                ui.set_width(ui.available_width());
                let title = k::medium(format!("I understand that ALL data on {} will be destroyed", self.disk_desc()), 14.0, k::DANGER_LIGHT);
                let sub = k::text("Partitions, files and the file system. This cannot be undone.", 12.0, k::DANGER_SUB);
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
            Kind::Test | Kind::Endurance => {
                self.size_field(ui, "Size limit", "Empty = all. Examples: 8G, 500M");
                if self.kind == Kind::Endurance {
                    ui.horizontal(|ui| {
                        ui.label(k::text("Passes", 12.0, k::TEXT2));
                        ui.add(egui::DragValue::new(&mut self.passes).range(0..=1000));
                        ui.label(k::text("0 = until an error or Cancel", 12.0, k::TEXT3));
                    });
                } else {
                    k::checkbox(
                        ui,
                        &mut self.replug,
                        16.0,
                        k::TEXT3,
                        k::ACCENT,
                        k::text("Unplug/replug before verifying", 14.0, k::TEXT),
                        Some(k::text("Catches drives that answer reads from a cache instead of the flash.", 12.0, k::TEXT3)),
                    );
                }
            }
            Kind::Bench => self.size_field(ui, "Test size", "Empty = 1 GiB. Examples: 4G, 500M"),
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
                        for f in if cfg!(windows) { &["exfat", "fat32", "ntfs"][..] } else { &["exfat", "fat32", "ntfs", "ext4"] } {
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
                    .hint_text(k::mono("all", 13.0, k::TEXT3));
                ui.new_child(UiBuilder::new().max_rect(r.shrink2(vec2(10.0, 6.0)))).add(edit);
                ui.label(k::text(help, 12.0, k::TEXT3));
            });
            if let Some(e) = err {
                ui.label(k::text(e, 12.0, k::DANGER_ERR));
            }
        });
    }

    fn foot_note(&self, raw: bool) -> (String, Color32) {
        if self.destructive() && !self.confirm {
            return ("Tick the box above to enable Start.".into(), k::DANGER_ERR);
        }
        if self.needs_admin() && !self.admin {
            return (String::new(), k::TEXT2);
        }
        let free = self.sel_volume().map(|v| v.free).unwrap_or(0);
        let limit = crate::cli::parse_size(&self.size_limit).ok().map_or(free, |l| l.min(free));
        let fake = "On a fake drive these writes can land on existing files: back them up first.";
        let id = self.sel_disk().map(|d| d.id.clone()).unwrap_or_default();
        let s = match (self.kind, raw) {
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
            (Kind::Iops, false) => "Works inside one test file in free space.".into(),
            (Kind::Verify | Kind::Scan | Kind::Bench | Kind::Iops, _) => "Reads only. Changes nothing.".into(),
            _ => format!("All data on disk {id} will be destroyed."),
        };
        (s, k::TEXT2)
    }

    /// The mode-coloured strip above the run and result views.
    fn slim_bar(&self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
        let mut test = self.kind.name().to_string();
        if self.kind == Kind::Test && self.replug {
            test += " · replug before verify";
        }
        mode_band(ui, rect, self.raw(), CornerRadius::same(4), Some((self.target_text(), test)));
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
        let cells = gui_map::live(&s, &self.curves);
        self.viz_row(ui, cells.as_deref(), "Block map", None);
        self.live = s;
        self.log_bar(ui, true);
    }

    fn prompt_banner(&self, ui: &mut egui::Ui, prompt: &str) {
        let (head, sub) = match prompt.find(". ") {
            Some(i) => (&prompt[..=i], prompt[i + 2..].trim()),
            None => (prompt, ""),
        };
        let at = if prompt.starts_with("Drive removed") { 1 } else { 0 };
        let steps = if self.kind == Kind::PlugPull { ["Pull the drive out", "Plug back in", "Check flushed data"] } else { ["Unplug", "Plug back in", "Verify"] };
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
                        ui.label(k::medium("Action needed: the test is waiting for you", 13.0, k::WARN_INK));
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
        let stats = [(done_label, done), (speed_label, speed), ("Time left", eta)];
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
        self.viz_row(ui, cells.as_deref(), "Final block map", capacity);

        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                if k::button(ui, Look::Secondary, Some(Icon::Download), "Save detailed report", 32.0, 13.0, true).clicked() {
                    act = Some(Act::Save(false));
                }
                if k::button(ui, Look::Secondary, Some(Icon::FileText), "Save evidence sheet", 32.0, 13.0, true).clicked() {
                    act = Some(Act::Save(true));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let r = ui
                        .horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            k::icon(ui, if self.summary_open { Icon::ChevronDown } else { Icon::ChevronRight }, 12.0, k::TEXT2);
                            ui.label(k::text("Full text summary", 12.0, k::TEXT2));
                        })
                        .response;
                    let r = ui.interact(r.rect, egui::Id::new("summary-toggle"), Sense::click());
                    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::CollapsingHeader, true, "Full text summary"));
                    k::pointer_on_hover(&r);
                    if r.clicked() {
                        self.summary_open = !self.summary_open;
                    }
                });
            });
            if let Some(e) = &self.error {
                ui.label(k::text(e, 13.0, k::DANGER_ERR));
            }
            for p in &self.saved {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(k::mono("Saved", 12.0, k::OK));
                    ui.label(k::mono(p.display().to_string(), 12.0, k::TEXT2));
                    if p.extension().is_some_and(|e| e == "html") {
                        if ui.link(k::mono("Open", 12.0, k::LINK)).clicked() {
                            act = Some(Act::Open(p.clone()));
                        }
                        if ui.link(k::mono("Show folder", 12.0, k::LINK)).clicked() {
                            act = Some(Act::Open(p.parent().map(|d| d.to_path_buf()).unwrap_or_default()));
                        }
                    }
                });
            }
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
            Some(Act::Open(p)) => platform::open_file(&p),
            Some(Act::Again) => self.result = None,
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
        let target = match &self.sel {
            Some(Sel::Volume(m)) => vol_key(m),
            Some(Sel::Disk(id)) => format!("disk {id}"),
            None => String::new(),
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
                                    b(Look::Outline(k::DANGER_STROKE), "Fix: repartition to real size", Act::Fix(id));
                                }
                            }
                            Status::Defective | Status::ReadOnly => {
                                b(evidence, "Save evidence sheet", Act::Save(true));
                                b(Look::Outline(k::DANGER_STROKE), "Run Full test again", Act::Rerun(Kind::Test));
                            }
                            Status::Ok => {
                                b(Look::Outline(stroke), &format!("Run another test on {target}"), Act::Again);
                                b(Look::Plain, "Choose another drive", Act::ChooseDrive);
                            }
                            _ => {
                                b(Look::Outline(stroke), "Run another test", Act::Again);
                                b(Look::Plain, "Choose another drive", Act::ChooseDrive);
                            }
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

/// A file-mode / raw-mode strip. With `slim`, it also carries the target and test name.
fn mode_band(ui: &egui::Ui, rect: Rect, raw: bool, radius: CornerRadius, slim: Option<((String, String), String)>) {
    let (fill, fg, stroke) = if raw { (k::DANGER_TINT, k::DANGER_BAND, k::DANGER_STROKE) } else { (k::SEL, k::LINK_LIGHT, k::LINE) };
    let p = ui.painter();
    let cy = rect.center().y;
    let (label, line) = if raw {
        ("Raw mode", "Tests the whole disk. Destroys all data on it.")
    } else {
        ("File mode", "Tests free space only. On a fake drive, writes can still reach your files.")
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
            ui.set_width(268.0);
            ui.spacing_mut().item_spacing.y = 5.0;
            ui.label(k::medium(head, 13.0, head_color));
            ui.add(Label::new(k::medium(title, 16.0, k::TEXT)).wrap());
            ui.add(Label::new(k::text(sub, 13.0, k::TEXT2)).wrap());
        });
    });
}

/// "E:" for "E:\", the mount point elsewhere.
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
    #[cfg(target_os = "linux")]
    fn relaunch(&mut self, ctx: &egui::Context) {
        match platform::relaunch_root() {
            Ok(c) => {
                self.elevating = Some(c);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
            Err(e) => self.error = Some(e),
        }
    }

    #[cfg(target_os = "linux")]
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
        #[cfg(target_os = "linux")]
        self.poll_relaunch(ui.ctx());
        self.poll();
        if self.running.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
        egui::Panel::top("top").exact_size(44.0).resizable(false).frame(Frame::new().fill(k::BAR).inner_margin(Margin::symmetric(16, 0))).show(ui, |ui| self.top_bar(ui));
        egui::Panel::left("drives").exact_size(300.0).resizable(false).frame(Frame::new().fill(k::PANEL)).show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::default().frame(Frame::new().fill(k::WINDOW).inner_margin(Margin::symmetric(24, 20))).show(ui, |ui| {
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
            let (t, m): (Vec<u16>, Vec<u16>) = ("FlashTester".encode_utf16().chain(Some(0)).collect(), msg.encode_utf16().chain(Some(0)).collect());
            unsafe { windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(std::ptr::null_mut(), m.as_ptr(), t.as_ptr(), 0x10) };
            3
        }
    }
}

#[cfg(target_os = "linux")]
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
