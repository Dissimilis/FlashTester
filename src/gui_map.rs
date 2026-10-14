//! GUI block map: the drive (or test stream) as a grid of cells, coloured by what
//! is known about each region, with offsets, speed and errors on hover. Also the
//! speed-by-position graph and the real-vs-claimed capacity bar.

use crate::ctx::Status as Live;
use crate::engine::IoStats;
use crate::gui_kit as k;
use crate::report::{Report, size};
use egui::{self, Color32, Rect, Sense, Stroke, StrokeKind, Vec2, pos2, vec2};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum State {
    Untested,
    Verified,
    Written,
    Changed,
    Stale,
    Aliased,
    Corrupted,
    Unreadable,
}

const ALL: [State; 8] = [State::Untested, State::Written, State::Verified, State::Changed, State::Stale, State::Aliased, State::Corrupted, State::Unreadable];

impl State {
    fn color(self) -> Color32 {
        match self {
            State::Untested => Color32::from_rgb(0x36, 0x3a, 0x40),
            State::Written => Color32::from_rgb(0x4f, 0x7f, 0xb8),
            State::Verified => Color32::from_rgb(0x4f, 0xa8, 0x77),
            State::Changed => Color32::from_rgb(0xd8, 0xc4, 0x4a),
            State::Stale => Color32::from_rgb(0x9a, 0x6f, 0xd0),
            State::Aliased => Color32::from_rgb(0xe0, 0x8a, 0x3c),
            State::Corrupted => Color32::from_rgb(0xe0, 0x52, 0x4a),
            State::Unreadable => Color32::from_rgb(0x7d, 0x26, 0x26),
        }
    }
    fn short(self) -> &'static str {
        match self {
            State::Untested => "Untested",
            State::Written => "Written",
            State::Verified => "Verified",
            State::Changed => "Slightly changed",
            State::Stale => "Old data",
            State::Aliased => "Aliased",
            State::Corrupted => "Corrupted",
            State::Unreadable => "Unreadable",
        }
    }
    fn name(self) -> &'static str {
        match self {
            State::Untested => "Untested",
            State::Written => "Written, not yet verified",
            State::Verified => "Verified",
            State::Changed => "Slightly changed",
            State::Stale => "Old data (write ignored)",
            State::Aliased => "Aliased (wraparound)",
            State::Corrupted => "Corrupted",
            State::Unreadable => "Unreadable",
        }
    }
    pub fn from_kind(k: &str) -> State {
        match k {
            "changed" => State::Changed,
            "stale" => State::Stale,
            "aliased" => State::Aliased,
            "corrupted" => State::Corrupted,
            _ => State::Unreadable,
        }
    }
}

pub struct Cell {
    pub start: u64,
    pub end: u64,
    pub state: State,
    pub write: Option<f64>,
    pub read: Option<f64>,
    pub errors: Vec<String>,
}

/// One speed series: (position in GB, MB/s).
pub struct Curve {
    pub read: bool,
    pub pts: Vec<[f64; 2]>,
}

const COLS: usize = 32;
const ROWS: usize = 16;
const CELLS: u64 = (COLS * ROWS) as u64;

fn grid(span: u64) -> Vec<Cell> {
    (0..CELLS).map(|i| Cell { start: span * i / CELLS, end: span * (i + 1) / CELLS, state: State::Untested, write: None, read: None, errors: vec![] }).collect()
}

/// Paint `[a, b)` with `st`; the state order makes the worst news win.
fn paint(cells: &mut [Cell], a: u64, b: u64, st: State, err: Option<&str>) {
    for c in cells.iter_mut().filter(|c| c.start < b && c.end > a) {
        c.state = c.state.max(st);
        if let Some(e) = err
            && c.errors.len() < 3
        {
            c.errors.push(e.to_string());
        }
    }
}

/// Speed per cell as bytes over time: each point is one equal-sized block (or region),
/// so that is the harmonic mean. A plain mean would hide stalls behind fast blocks.
fn speeds(cells: &mut [Cell], pts: impl Iterator<Item = (u64, f64)>, read: bool) {
    let mut acc = vec![(0.0, 0u32); cells.len()];
    let span = cells.last().map(|c| c.end).unwrap_or(1).max(1);
    for (pos, v) in pts {
        let i = ((pos as u128 * cells.len() as u128 / span as u128) as usize).min(cells.len() - 1);
        acc[i].0 += 1.0 / v.max(1e-6);
        acc[i].1 += 1;
    }
    for (c, (s, n)) in cells.iter_mut().zip(acc) {
        if n > 0 {
            let v = Some(n as f64 / s);
            if read { c.read = v } else { c.write = v }
        }
    }
}

fn curve_points(c: &Curve) -> impl Iterator<Item = (u64, f64)> + '_ {
    c.pts.iter().map(|p| ((p[0] * 1e9) as u64, p[1]))
}

/// Cells for a running job, from the live status and the speed curves seen so far.
pub fn live(s: &Live, curves: &[Curve]) -> Option<Vec<Cell>> {
    if s.total < 64 << 20 {
        return None;
    }
    let span = s.base + s.total;
    let mut cells = grid(span);
    let at = s.base + s.done;
    if is_read(&s.phase) {
        paint(&mut cells, 0, span, State::Written, None);
        paint(&mut cells, 0, at, State::Verified, None);
        // Verified sorts below Written so that bad news wins; here it replaces it.
        for c in cells.iter_mut().filter(|c| c.end <= at) {
            c.state = State::Verified;
        }
    } else {
        paint(&mut cells, 0, at, State::Written, None);
    }
    for (a, b, kind) in &s.marks {
        paint(&mut cells, *a, *b, State::from_kind(kind), Some(kind));
    }
    for c in curves {
        speeds(&mut cells, curve_points(c), c.read);
    }
    Some(cells)
}

/// Phases that read data back (green in the graph, verified in the map).
pub fn is_read(phase: &str) -> bool {
    let p = phase.to_ascii_lowercase();
    p.contains("verif") || p.contains("read") || p.contains("scan")
}

fn stats_curve(st: &IoStats, read: bool) -> Option<Curve> {
    let pts: Vec<[f64; 2]> = st.speed_map.iter().map(|p| [p.pos as f64 / 1e9, p.mb_s]).collect();
    (pts.len() > 1).then_some(Curve { read, pts })
}

/// Speed curves of a finished report: the last write and read pass.
pub fn report_curves(r: &Report) -> Vec<Curve> {
    let mut v = vec![];
    if let Some(b) = &r.bench {
        v.extend(b.write.as_ref().and_then(|w| stats_curve(w, false)));
        v.extend(stats_curve(&b.read, true));
    } else if let Some(sc) = &r.scan {
        v.extend(stats_curve(&sc.stats, true));
    } else {
        v.extend(r.write.last().and_then(|w| stats_curve(&w.stats, false)));
        v.extend(r.verify.last().and_then(|x| stats_curve(&x.stats, true)));
    }
    v
}

/// Cells for a finished report.
pub fn from_report(r: &Report) -> Option<Vec<Cell>> {
    if let Some(p) = &r.probe {
        let mut probes: Vec<(u64, u8)> = p.positions.iter().copied().zip(p.status.bytes()).collect();
        if probes.is_empty() || p.claimed == 0 {
            return None;
        }
        probes.sort();
        let mut cells = grid(p.claimed);
        for (i, &(pos, st)) in probes.iter().enumerate() {
            let end = probes.get(i + 1).map(|x| x.0).unwrap_or(p.claimed);
            let (state, err) = match st {
                b'o' => (State::Verified, None),
                b'a' => (State::Aliased, Some("holds another position's data")),
                b's' => (State::Stale, Some("old data: the write was ignored")),
                b'e' => (State::Unreadable, Some("I/O error")),
                _ => (State::Corrupted, Some("data lost")),
            };
            paint(&mut cells, pos, end.max(pos + 1), state, err);
        }
        return Some(cells);
    }
    if let Some(sc) = &r.scan {
        let mut cells = grid(sc.size);
        paint(&mut cells, 0, sc.stats.bytes, State::Verified, None);
        for b in &sc.bad_ranges {
            paint(&mut cells, b.start, b.end, State::Unreadable, Some("unreadable"));
        }
        speeds(&mut cells, sc.stats.speed_map.iter().map(|p| (p.pos, p.mb_s)), true);
        return Some(cells);
    }
    let v = r.verify.last()?;
    let written = r.write.last().map(|w| w.end).unwrap_or(0);
    let span = written.max(v.stats.bytes).max(r.tested).max(1);
    let mut cells = grid(span);
    paint(&mut cells, 0, written, State::Written, None);
    for c in cells.iter_mut().filter(|c| c.end <= v.stats.bytes) {
        c.state = State::Verified;
    }
    for b in &v.tally.bad_ranges {
        paint(&mut cells, b.start, b.end, State::from_kind(&b.kind), Some(&format!("{} at {}", b.kind, size(b.start))));
    }
    if let Some(w) = r.write.last() {
        speeds(&mut cells, w.stats.speed_map.iter().map(|p| (p.pos, p.mb_s)), false);
    }
    speeds(&mut cells, v.stats.speed_map.iter().map(|p| (p.pos, p.mb_s)), true);
    Some(cells)
}

const CELL: f32 = 10.0;
const GAP: f32 = 2.0;
pub const MAP_W: f32 = COLS as f32 * (CELL + GAP) - GAP;

/// Title row, the 32 x 16 grid with hover details, and the legend.
pub fn show(ui: &mut egui::Ui, cells: &[Cell], title: &str) {
    ui.spacing_mut().item_spacing.y = 8.0;
    let span = cells.last().map(|c| c.end).unwrap_or(0).saturating_sub(cells.first().map(|c| c.start).unwrap_or(0));
    let (head, _) = ui.allocate_exact_size(vec2(MAP_W, 16.0), Sense::hover());
    k::paint_medium(ui.painter(), head.left_center(), egui::Align2::LEFT_CENTER, title, 12.0, k::TEXT3);
    let scale = format!("{} cells · {} each", cells.len(), size(span / cells.len().max(1) as u64));
    k::paint_text(ui.painter(), head.right_center(), egui::Align2::RIGHT_CENTER, &scale, 11.0, k::TEXT3);

    let rows = cells.len().div_ceil(COLS).max(ROWS);
    let (rect, resp) = ui.allocate_exact_size(vec2(MAP_W, rows as f32 * (CELL + GAP) - GAP), Sense::hover());
    let p = ui.painter();
    let cell_rect = |i: usize| Rect::from_min_size(rect.min + vec2((i % COLS) as f32 * (CELL + GAP), (i / COLS) as f32 * (CELL + GAP)), Vec2::splat(CELL));
    for (i, c) in cells.iter().enumerate() {
        p.rect_filled(cell_rect(i), 0.0, c.state.color());
    }
    if let Some(pos) = resp.hover_pos() {
        let rel = pos - rect.min;
        let i = (rel.y / (CELL + GAP)) as usize * COLS + ((rel.x / (CELL + GAP)) as usize).min(COLS - 1);
        if let Some(c) = cells.get(i) {
            p.rect_stroke(cell_rect(i).expand(1.5), 0.0, Stroke::new(1.0, Color32::WHITE), StrokeKind::Middle);
            resp.on_hover_ui_at_pointer(|ui| tooltip(ui, c));
        }
    }

    egui::Grid::new(ui.id().with("legend")).num_columns(4).spacing(vec2(12.0, 4.0)).show(ui, |ui| {
        for (n, st) in ALL.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 5.0;
                let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                ui.painter().rect_filled(r, 0.0, st.color());
                ui.label(k::text(st.short(), 11.0, k::TEXT2));
            });
            if n % 4 == 3 {
                ui.end_row();
            }
        }
    });
}

fn tooltip(ui: &mut egui::Ui, c: &Cell) {
    ui.set_width(200.0);
    ui.spacing_mut().item_spacing.y = 2.0;
    ui.label(k::mono(format!("{} – {}", size(c.start), size(c.end)), 12.0, k::TEXT));
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
        ui.painter().rect_filled(r, 0.0, c.state.color());
        ui.label(k::text(c.state.name(), 12.0, k::TEXT));
    });
    let speed = match (c.read, c.write) {
        (Some(r), Some(w)) => format!("Read {r:.1} MB/s · write {w:.1} MB/s"),
        (Some(r), None) => format!("Read {r:.1} MB/s"),
        (None, Some(w)) => format!("Write {w:.1} MB/s"),
        _ => String::new(),
    };
    if !speed.is_empty() {
        ui.label(k::text(speed, 12.0, k::TEXT2));
    }
    if !c.errors.is_empty() {
        ui.label(k::text(c.errors.join(", "), 12.0, k::DANGER_ERR));
    } else if c.state != State::Untested {
        ui.label(k::text("No errors", 12.0, k::OK));
    }
}

/// Speed per slice of the position axis, as bytes over time (the harmonic mean of
/// equal-sized blocks), so a stall counts for the time it took. One slow block is
/// smoothed out; a slow region that spans slices still shows. Display only.
fn smooth(pts: &[[f64; 2]], max_slices: usize) -> Vec<[f64; 2]> {
    // At least 4 points per slice: cached and flushed blocks alternate on many sticks.
    let slices = (pts.len() / 4).clamp(1, max_slices);
    let (lo, hi) = pts.iter().fold((f64::MAX, f64::MIN), |(lo, hi), p| (lo.min(p[0]), hi.max(p[0])));
    if hi <= lo {
        return pts.to_vec();
    }
    let mut acc = vec![(0.0, 0.0, 0u32); slices];
    for p in pts {
        let i = (((p[0] - lo) / (hi - lo) * slices as f64) as usize).min(slices - 1);
        acc[i] = (acc[i].0 + p[0], acc[i].1 + 1.0 / p[1].max(1e-6), acc[i].2 + 1);
    }
    acc.into_iter().filter(|a| a.2 > 0).map(|(x, inv, n)| [x / n as f64, n as f64 / inv]).collect()
}

/// Speed by position: write blue, read green; the drive's temperature in orange
/// on its own right-hand scale (`temps`: position in GB, °C).
pub fn graph(ui: &mut egui::Ui, curves: &[Curve], temps: &[[f64; 2]], w: f32) {
    // Jobs that don't move along the disk (IOPS) give every reading the same position.
    let temps = if temps.iter().any(|t| t[0] != temps[0][0]) { temps } else { &[] };
    ui.spacing_mut().item_spacing.y = 8.0;
    let (head, _) = ui.allocate_exact_size(vec2(w, 16.0), Sense::hover());
    let p = ui.painter();
    k::paint_medium(p, head.left_center(), egui::Align2::LEFT_CENTER, "Speed (MB/s) by position", 12.0, k::TEXT3);
    let mut x = head.right();
    for (read, name, color) in [(None, "Temp", k::ORANGE), (Some(true), "Read", k::OK), (Some(false), "Write", k::ACCENT)] {
        let shown = match read {
            Some(r) => curves.iter().any(|c| c.read == r && c.pts.len() > 1),
            None => temps.len() > 1,
        };
        if shown {
            let r = k::paint_text(p, pos2(x, head.center().y), egui::Align2::RIGHT_CENTER, name, 11.0, k::TEXT2);
            p.line_segment([pos2(r.left() - 17.0, r.center().y), pos2(r.left() - 5.0, r.center().y)], Stroke::new(2.0, color));
            x = r.left() - 27.0;
        }
    }
    let lines: Vec<(bool, Vec<[f64; 2]>)> = curves.iter().filter(|c| c.pts.len() > 1).map(|c| (c.read, smooth(&c.pts, 60))).collect();
    // 100 °C sits at the top of the speed range.
    let scale = lines.iter().flat_map(|l| l.1.iter().map(|p| p[1])).fold(1.0, f64::max) * 1.1 / 100.0;
    let mut plot = egui_plot::Plot::new(ui.id().with("speed"));
    if temps.len() > 1 {
        plot = plot.custom_y_axes(vec![
            egui_plot::AxisHints::new_y(),
            egui_plot::AxisHints::new_y().placement(egui_plot::HPlacement::Right).formatter(move |m, _| format!("{:.0} °C", m.value / scale)),
        ]);
    }
    plot
        .width(w)
        .height(190.0)
        .show_background(false)
        .allow_zoom(false)
        .allow_drag(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .allow_double_click_reset(false)
        .show_x(false)
        .show_y(false)
        .include_x(0.0)
        .include_y(0.0)
        .x_grid_spacer(egui_plot::log_grid_spacer(5))
        .y_grid_spacer(egui_plot::log_grid_spacer(5))
        .x_axis_label(k::text("position (GB)", 10.0, k::TEXT3))
        .show(ui, |pl| {
            for (read, pts) in lines {
                let (name, color) = if read { ("Read", k::OK) } else { ("Write", k::ACCENT) };
                pl.line(egui_plot::Line::new(name, pts).color(color).width(1.5));
            }
            // A new pass starts again at position 0: one line per run of rising positions.
            let mut seg: Vec<[f64; 2]> = vec![];
            for t in temps.iter().chain([&[f64::NEG_INFINITY, 0.0]]) {
                if seg.last().is_some_and(|l| t[0] < l[0]) {
                    if seg.len() > 1 {
                        pl.line(egui_plot::Line::new("Temp", std::mem::take(&mut seg)).color(k::ORANGE).width(1.5));
                    }
                    seg.clear();
                }
                seg.push([t[0], t[1] * scale]);
            }
        });
}

/// Real vs claimed capacity (capacity probe results).
pub fn capacity(ui: &mut egui::Ui, real: u64, claimed: u64, w: f32) {
    ui.spacing_mut().item_spacing.y = 6.0;
    ui.label(k::medium("Real vs claimed capacity", 12.0, k::TEXT3));
    let (bar, _) = ui.allocate_exact_size(vec2(w, 28.0), Sense::hover());
    let frac = if claimed > 0 { (real as f64 / claimed as f64).clamp(0.0, 1.0) as f32 } else { 0.0 };
    let gw = (bar.width() * frac).max(5.0).min(bar.width());
    let p = ui.painter();
    p.rect_filled(bar, 0.0, k::ORANGE.gamma_multiply(0.85));
    p.rect_filled(Rect::from_min_size(bar.min, vec2(gw, bar.height())), 0.0, k::OK);
    p.rect_stroke(bar, 0.0, Stroke::new(1.0, k::LINE), StrokeKind::Inside);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(k::medium(size(real), 22.0, k::OK));
        ui.label(k::text("stores data", 12.0, k::TEXT2));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(k::text("claimed", 12.0, k::TEXT2));
            ui.label(k::medium(size(claimed), 22.0, k::TEXT));
        });
    });
    let note = if real >= claimed {
        "Every probed position kept its data.".to_string()
    } else {
        format!("{:.1} % of the claimed size is real. Data written beyond {} is lost or lands on earlier data (orange in the map).", frac * 100.0, size(real))
    };
    ui.add(egui::Label::new(k::text(note, 12.0, k::TEXT2)).wrap());
}

#[cfg(test)]
mod tests {
    use super::smooth;

    #[test]
    fn slice_speed_is_bytes_over_time() {
        // Three equal blocks, one of them stalled: 3 blocks in 1/100 + 1 + 1/100 time units.
        let s = smooth(&[[0.0, 100.0], [0.5, 1.0], [1.0, 100.0]], 1);
        assert_eq!(s.len(), 1);
        assert!((s[0][1] - 3.0 / 1.02).abs() < 1e-9, "{}", s[0][1]);
    }

    #[test]
    fn alternating_blocks_become_one_steady_line() {
        // 60 blocks alternating 110 and 15 MB/s, as a stick writing through a small cache does.
        let pts: Vec<[f64; 2]> = (0..60).map(|i| [i as f64, if i % 2 == 0 { 110.0 } else { 15.0 }]).collect();
        let s = smooth(&pts, 60);
        assert_eq!(s.len(), 15);
        let truth = 2.0 / (1.0 / 110.0 + 1.0 / 15.0);
        assert!(s.iter().all(|p| (p[1] - truth).abs() < 1e-9), "{s:?}");
    }
}
