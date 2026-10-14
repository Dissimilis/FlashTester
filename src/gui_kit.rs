//! GUI look: colour tokens, the global egui style, painted icons and the few custom
//! widgets (buttons, choice rows, checkboxes) the design needs.

use egui::epaint::CornerRadius;
use egui::text::{LayoutJob, TextWrapping};
use egui::{Align2, Color32, FontId, Galley, Pos2, Rect, Response, RichText, Sense, Shape, Stroke, StrokeKind, Ui, Vec2, WidgetInfo, WidgetType, pos2, vec2};
use std::sync::Arc;

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

pub const BAR: Color32 = hex(0x191b1e);
pub const WINDOW: Color32 = hex(0x202327);
pub const PANEL: Color32 = hex(0x26292e);
pub const DISK: Color32 = hex(0x2c3035);
pub const HOVER: Color32 = hex(0x363a40);
pub const LIST_HOVER: Color32 = hex(0x30343a);
pub const HINT: Color32 = hex(0x2a2e34);
pub const LINE_SUBTLE: Color32 = hex(0x383c42);
pub const LINE_CARD: Color32 = hex(0x3a3e44);
pub const LINE: Color32 = hex(0x464b53);
pub const LINE_STRONG: Color32 = hex(0x4a4f57);
pub const LINE_HOVER: Color32 = hex(0x5d636c);
pub const RING: Color32 = hex(0x6e737a);
pub const GRID: Color32 = hex(0x32363c);
pub const TEXT: Color32 = hex(0xe8eaed);
pub const TEXT2: Color32 = hex(0xb4b8be);
pub const TEXT2B: Color32 = hex(0xd0d3d7);
pub const TEXT3: Color32 = hex(0x8b9097);
pub const ACCENT: Color32 = hex(0x5b93cc);
pub const SEL: Color32 = hex(0x2c3e52);
pub const LINK: Color32 = hex(0x8db8e8);
pub const LINK_LIGHT: Color32 = hex(0xb8d6f5);
pub const OK: Color32 = hex(0x4fa877);
pub const OK_TINT: Color32 = hex(0x13281e);
pub const OK_STROKE: Color32 = hex(0x2f6b4c);
pub const OK_TEXT: Color32 = hex(0x7fd0a6);
pub const OK_INK: Color32 = hex(0x08160f);
pub const WARN: Color32 = hex(0xe3a84b);
pub const WARN_TINT: Color32 = hex(0x2b2212);
pub const WARN_STROKE: Color32 = hex(0x6b5124);
pub const WARN_TEXT: Color32 = hex(0xf0c47c);
pub const WARN_SUB: Color32 = hex(0xc9b58f);
pub const WARN_INK: Color32 = hex(0x1b1407);
pub const DANGER: Color32 = hex(0xe0675f);
pub const DANGER_TINT: Color32 = hex(0x3a2224);
pub const DANGER_SEL: Color32 = hex(0x4a2628);
pub const DANGER_STROKE: Color32 = hex(0x7a3431);
pub const DANGER_TEXT: Color32 = hex(0xe88a83);
pub const DANGER_ERR: Color32 = hex(0xf08a82);
pub const DANGER_BAND: Color32 = hex(0xffb3ad);
pub const DANGER_LIGHT: Color32 = hex(0xffd5d1);
pub const DANGER_SUB: Color32 = hex(0xc99490);
pub const DANGER_INK: Color32 = hex(0x1d0807);
pub const DONE_INK: Color32 = hex(0x0c1117);
pub const EMPTY_RAW: Color32 = hex(0x2e2426);
pub const EMPTY_RAW_STROKE: Color32 = hex(0x6a3a3a);
pub const ORANGE: Color32 = hex(0xe08a3c);

/// The global style: dark surfaces, 3 px inputs, the tooltip frame and shadow.
pub fn style(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.all_styles_mut(apply);
}

fn apply(s: &mut egui::Style) {
    use egui::{FontFamily, TextStyle};
    s.text_styles = [
        (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(13.0, FontFamily::Proportional)),
        (TextStyle::Heading, FontId::new(16.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
    ]
    .into();
    s.interaction.selectable_labels = false;
    s.interaction.tooltip_delay = 0.0;
    s.interaction.show_tooltips_only_when_still = false;
    s.spacing.item_spacing = vec2(8.0, 4.0);
    let v = &mut s.visuals;
    v.dark_mode = true;
    v.panel_fill = WINDOW;
    v.window_fill = BAR;
    v.window_stroke = Stroke::new(1.0, LINE_HOVER);
    v.window_corner_radius = CornerRadius::same(4);
    v.menu_corner_radius = CornerRadius::same(4);
    v.popup_shadow = egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(128) };
    v.extreme_bg_color = BAR;
    v.faint_bg_color = PANEL;
    v.hyperlink_color = LINK;
    v.selection.bg_fill = SEL;
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    let w = &mut v.widgets;
    w.noninteractive.fg_stroke.color = TEXT;
    w.noninteractive.bg_stroke = Stroke::new(1.0, LINE_SUBTLE);
    for (st, fill, stroke) in [(&mut w.inactive, BAR, LINE), (&mut w.hovered, PANEL, LINE_HOVER), (&mut w.active, PANEL, ACCENT)] {
        st.bg_fill = fill;
        st.weak_bg_fill = fill;
        st.bg_stroke = Stroke::new(1.0, stroke);
        st.fg_stroke.color = TEXT;
        st.corner_radius = CornerRadius::same(3);
        st.expansion = 0.0;
    }
}

pub fn text(s: impl Into<String>, size: f32, color: Color32) -> RichText {
    RichText::new(s).size(size).color(color)
}

pub fn mono(s: impl Into<String>, size: f32, color: Color32) -> RichText {
    RichText::new(s).monospace().size(size).color(color)
}

/// One line of text cut with an ellipsis at `max_w`. Colour comes at paint time.
pub fn galley(ui: &Ui, s: &str, font: FontId, max_w: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(s.to_string(), font, Color32::PLACEHOLDER);
    job.wrap = TextWrapping::truncate_at_width(max_w.max(10.0));
    ui.painter().layout_job(job)
}

pub fn pointer_on_hover(r: &Response) {
    if r.hovered() {
        r.ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
}

// ---- icons: Lucide shapes (24 x 24 grid, stroke 1.5), redrawn as lines ----

#[derive(Clone, Copy)]
pub enum Icon {
    Usb,
    Sd,
    Drive,
    Lock,
    Refresh,
    Shield,
    ShieldAlert,
    ShieldCheck,
    Alert,
    Unplug,
    Check,
    ChevronRight,
    ChevronDown,
    Download,
    FileText,
    Play,
    Stop,
}

fn arc(c: Pos2, r: f32, a0: f32, a1: f32) -> Vec<Pos2> {
    let n = 12;
    (0..=n)
        .map(|i| {
            let a = (a0 + (a1 - a0) * i as f32 / n as f32).to_radians();
            pos2(c.x + r * a.cos(), c.y + r * a.sin())
        })
        .collect()
}

fn cubic(p: [Pos2; 4]) -> Vec<Pos2> {
    (1..=8)
        .map(|i| {
            let t = i as f32 / 8.0;
            let u = 1.0 - t;
            let w = [u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t];
            pos2((0..4).map(|k| w[k] * p[k].x).sum(), (0..4).map(|k| w[k] * p[k].y).sum())
        })
        .collect()
}

pub fn paint_icon(p: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    let s = rect.width().min(rect.height()) / 24.0;
    let o = rect.center() - vec2(12.0 * s, 12.0 * s);
    let pt = |x: f32, y: f32| o + vec2(x * s, y * s);
    let pts = |v: &[(f32, f32)]| v.iter().map(|&(x, y)| pt(x, y)).collect::<Vec<_>>();
    let st = Stroke::new((1.5 * s).max(1.0), color);
    let line = |v: &[(f32, f32)]| p.add(Shape::line(pts(v), st));
    let closed = |v: Vec<Pos2>| p.add(Shape::closed_line(v, st));
    let dot = |x: f32, y: f32| p.circle_filled(pt(x, y), (1.0 * s).max(0.8), color);
    let shield = || {
        let mut v = vec![pt(20.0, 13.0)];
        v.extend(cubic([pt(20.0, 13.0), pt(20.0, 18.0), pt(16.5, 20.5), pt(12.34, 21.95)]));
        v.push(pt(11.67, 21.94));
        v.extend(cubic([pt(11.67, 21.94), pt(7.5, 20.5), pt(4.0, 18.0), pt(4.0, 13.0)]));
        v.extend([pt(4.0, 6.0), pt(5.0, 5.0)]);
        v.extend(cubic([pt(5.0, 5.0), pt(7.0, 5.0), pt(9.5, 3.8), pt(11.24, 2.28)]));
        v.push(pt(12.76, 2.28));
        v.extend(cubic([pt(12.76, 2.28), pt(14.51, 3.81), pt(17.0, 5.0), pt(19.0, 5.0)]));
        v.push(pt(20.0, 6.0));
        closed(v);
    };
    match icon {
        Icon::Usb => {
            p.circle_stroke(pt(10.0, 7.0), s, st);
            p.circle_stroke(pt(4.0, 20.0), s, st);
            line(&[(4.7, 19.3), (19.0, 5.0)]);
            closed(pts(&[(21.0, 3.0), (18.0, 4.0), (20.0, 6.0)]));
            line(&[(9.26, 7.68), (5.0, 12.0), (7.0, 17.0)]);
            line(&[(10.0, 14.0), (15.0, 16.0), (18.5, 12.5)]);
            closed(pts(&[(18.0, 12.0), (19.0, 11.0), (20.0, 12.0), (19.0, 13.0)]));
        }
        Icon::Sd => {
            closed(pts(&[(6.0, 22.0), (4.0, 20.0), (4.0, 6.0), (8.0, 2.0), (18.0, 2.0), (20.0, 4.0), (20.0, 20.0), (18.0, 22.0)]));
            line(&[(8.0, 10.0), (8.0, 7.5)]);
            line(&[(12.0, 6.0), (12.0, 10.0)]);
            line(&[(16.0, 6.0), (16.0, 10.0)]);
        }
        Icon::Drive => {
            line(&[(22.0, 12.0), (2.0, 12.0)]);
            closed(pts(&[(5.45, 5.11), (2.0, 12.0), (2.0, 18.0), (4.0, 20.0), (20.0, 20.0), (22.0, 18.0), (22.0, 12.0), (18.55, 5.11), (16.76, 4.0), (7.24, 4.0)]));
            dot(6.0, 16.0);
        }
        Icon::Lock => {
            p.rect_stroke(Rect::from_min_max(pt(3.0, 11.0), pt(21.0, 22.0)), 2.0 * s, st, StrokeKind::Middle);
            let mut v = vec![pt(7.0, 11.0)];
            v.extend(arc(pt(12.0, 7.0), 5.0 * s, 180.0, 360.0));
            v.push(pt(17.0, 11.0));
            p.add(Shape::line(v, st));
        }
        Icon::Refresh => {
            let mut a = arc(pt(12.0, 12.0), 9.0 * s, 180.0, 315.0);
            a.push(pt(21.0, 8.0));
            p.add(Shape::line(a, st));
            line(&[(21.0, 3.0), (21.0, 8.0), (16.0, 8.0)]);
            let mut b = arc(pt(12.0, 12.0), 9.0 * s, 0.0, 135.0);
            b.push(pt(3.0, 16.0));
            p.add(Shape::line(b, st));
            line(&[(8.0, 16.0), (3.0, 16.0), (3.0, 21.0)]);
        }
        Icon::Shield => shield(),
        Icon::ShieldAlert => {
            shield();
            line(&[(12.0, 8.0), (12.0, 12.0)]);
            dot(12.0, 16.0);
        }
        Icon::ShieldCheck => {
            shield();
            line(&[(9.0, 12.0), (11.0, 14.0), (15.0, 10.0)]);
        }
        Icon::Alert => {
            closed(pts(&[(21.73, 18.0), (13.74, 4.0), (12.0, 3.0), (10.26, 4.0), (2.27, 18.0), (2.6, 20.2), (4.0, 21.0), (20.0, 21.0), (21.4, 20.2)]));
            line(&[(12.0, 9.0), (12.0, 13.0)]);
            dot(12.0, 17.0);
        }
        Icon::Unplug => {
            line(&[(19.0, 5.0), (22.0, 2.0)]);
            line(&[(2.0, 22.0), (5.0, 19.0)]);
            closed(pts(&[(6.3, 20.3), (8.0, 21.0), (9.7, 20.3), (12.0, 18.0), (6.0, 12.0), (3.7, 14.3), (3.0, 16.0), (3.7, 17.7)]));
            line(&[(7.5, 13.5), (10.0, 11.0)]);
            line(&[(10.5, 16.5), (13.0, 14.0)]);
            closed(pts(&[(12.0, 6.0), (18.0, 12.0), (20.3, 9.7), (21.0, 8.0), (20.3, 6.3), (17.7, 3.7), (16.0, 3.0), (14.3, 3.7)]));
        }
        Icon::Check => {
            p.add(Shape::line(pts(&[(20.0, 6.0), (9.0, 17.0), (4.0, 12.0)]), Stroke::new((2.0 * s).max(1.2), color)));
        }
        Icon::ChevronRight => {
            line(&[(9.0, 18.0), (15.0, 12.0), (9.0, 6.0)]);
        }
        Icon::ChevronDown => {
            line(&[(6.0, 9.0), (12.0, 15.0), (18.0, 9.0)]);
        }
        Icon::Download => {
            line(&[(21.0, 15.0), (21.0, 19.0), (19.0, 21.0), (5.0, 21.0), (3.0, 19.0), (3.0, 15.0)]);
            line(&[(7.0, 10.0), (12.0, 15.0), (17.0, 10.0)]);
            line(&[(12.0, 15.0), (12.0, 3.0)]);
        }
        Icon::FileText => {
            closed(pts(&[(15.0, 2.0), (6.0, 2.0), (4.0, 4.0), (4.0, 20.0), (6.0, 22.0), (18.0, 22.0), (20.0, 20.0), (20.0, 7.0)]));
            line(&[(14.0, 2.0), (14.0, 6.0), (16.0, 8.0), (20.0, 8.0)]);
            line(&[(10.0, 9.0), (8.0, 9.0)]);
            line(&[(16.0, 13.0), (8.0, 13.0)]);
            line(&[(16.0, 17.0), (8.0, 17.0)]);
        }
        Icon::Play => {
            p.add(Shape::convex_polygon(pts(&[(6.0, 4.0), (20.0, 12.0), (6.0, 20.0)]), color, Stroke::NONE));
        }
        Icon::Stop => {
            p.rect_filled(Rect::from_min_max(pt(5.0, 5.0), pt(19.0, 19.0)), 0.0, color);
        }
    }
}

pub fn icon(ui: &mut Ui, icon: Icon, size: f32, color: Color32) -> Response {
    let (r, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    paint_icon(ui.painter(), r, icon, color);
    resp
}

// ---- buttons ----

#[derive(Clone, Copy)]
pub enum Look {
    Primary,
    Danger,
    Secondary,
    Amber,
    /// Solid fill with dark ink (verdict actions).
    Solid(Color32, Color32),
    /// Transparent with a coloured outline.
    Outline(Color32),
    /// Text only.
    Plain,
}

/// A design button: `h` tall, `size` text, optional leading icon; 45 % opacity when disabled.
pub fn button(ui: &mut Ui, look: Look, icon: Option<Icon>, label: &str, h: f32, size: f32, enabled: bool) -> Response {
    let white = Color32::WHITE;
    let (fill, hover, press, stroke, fg) = match look {
        Look::Primary => (hex(0x3f73a6), hex(0x4a80b5), hex(0x355f8a), hex(0x5287bb), white),
        Look::Danger => (hex(0xb8433d), hex(0xc44c46), hex(0xa03a35), hex(0xcc5a53), white),
        Look::Secondary => (hex(0x2f3338), hex(0x3a3f45), hex(0x2a2d32), LINE_STRONG, TEXT),
        Look::Amber => (WARN, hex(0xeab35c), hex(0xcf973f), WARN, WARN_INK),
        Look::Solid(c, ink) => (c, c.gamma_multiply(0.9), c.gamma_multiply(0.8), c, ink),
        Look::Outline(c) => (Color32::TRANSPARENT, hex(0x3a3f45), hex(0x2a2d32), c, TEXT),
        Look::Plain => (Color32::TRANSPARENT, Color32::TRANSPARENT, Color32::TRANSPARENT, Color32::TRANSPARENT, TEXT2),
    };
    let pad = if h >= 36.0 { 18.0 } else { 14.0 };
    let g = ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(size), Color32::PLACEHOLDER);
    let isz = (size + 1.0).round();
    let iw = if icon.is_some() { isz + 8.0 } else { 0.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(pad * 2.0 + iw + g.size().x, h), if enabled { Sense::click() } else { Sense::hover() });
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    let a = if enabled { 1.0 } else { 0.45 };
    let bg = if !enabled {
        fill
    } else if resp.is_pointer_button_down_on() {
        press
    } else if resp.hovered() {
        hover
    } else {
        fill
    };
    let fg = if matches!(look, Look::Plain) && enabled && resp.hovered() { TEXT } else { fg };
    let p = ui.painter();
    p.rect(rect, 4.0, bg.gamma_multiply(a), Stroke::new(1.0, stroke.gamma_multiply(a)), StrokeKind::Inside);
    let mut x = rect.left() + pad;
    if let Some(i) = icon {
        paint_icon(p, Rect::from_center_size(pos2(x + isz / 2.0, rect.center().y), Vec2::splat(isz)), i, fg.gamma_multiply(a));
        x += iw;
    }
    p.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, fg.gamma_multiply(a));
    if enabled {
        pointer_on_hover(&resp);
    }
    resp
}

/// The width [`button`] takes for this label.
pub fn button_width(ui: &Ui, label: &str, h: f32, size: f32, icon: bool) -> f32 {
    let pad = if h >= 36.0 { 18.0 } else { 14.0 };
    let w = ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(size), Color32::PLACEHOLDER).size().x;
    pad * 2.0 + w + if icon { (size + 1.0).round() + 8.0 } else { 0.0 }
}

/// Square 28 x 28 icon button (Refresh); `label` is for screen readers.
pub fn icon_button(ui: &mut Ui, icon: Icon, label: &str, enabled: bool, opacity: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(28.0), if enabled { Sense::click() } else { Sense::hover() });
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    let bg = if enabled && resp.hovered() { hex(0x3a3f45) } else { hex(0x2f3338) };
    let p = ui.painter();
    p.rect(rect, 4.0, bg.gamma_multiply(opacity), Stroke::new(1.0, LINE_STRONG.gamma_multiply(opacity)), StrokeKind::Inside);
    paint_icon(p, Rect::from_center_size(rect.center(), Vec2::splat(14.0)), icon, TEXT2.gamma_multiply(opacity));
    if enabled {
        pointer_on_hover(&resp);
    }
    resp
}

// ---- radio rows and checkboxes ----

pub fn radio(p: &egui::Painter, c: Pos2, ring: Color32, dot: Option<Color32>) {
    p.circle(c, 7.0, BAR, Stroke::new(1.0, ring));
    if let Some(d) = dot {
        p.circle_filled(c, 3.0, d);
    }
}

/// Colours of a selectable row: (fill, stroke, ring, dot).
pub fn row_colors(selected: bool, raw: bool, hovered: bool, idle_fill: Color32, idle_stroke: Color32) -> (Color32, Color32, Color32, Option<Color32>) {
    if selected {
        let c = if raw { DANGER } else { ACCENT };
        (if raw { DANGER_SEL } else { SEL }, c, c, Some(c))
    } else if hovered {
        (if idle_fill == Color32::TRANSPARENT { LIST_HOVER } else { HOVER }, if idle_stroke == Color32::TRANSPARENT { Color32::TRANSPARENT } else { LINE_HOVER }, RING, None)
    } else {
        (idle_fill, idle_stroke, if idle_fill == Color32::TRANSPARENT { TEXT3 } else { RING }, None)
    }
}

/// A checkbox with a title and an optional second line; the whole block is clickable.
pub fn checkbox(ui: &mut Ui, on: &mut bool, box_size: f32, ring: Color32, fill_on: Color32, title: RichText, sub: Option<RichText>) -> Response {
    let r = ui
        .horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let (b, _) = ui.allocate_exact_size(Vec2::splat(box_size), Sense::hover());
            let b = b.translate(vec2(0.0, 1.0));
            let p = ui.painter();
            if *on {
                p.rect(b, 3.0, fill_on, Stroke::new(1.0, fill_on), StrokeKind::Inside);
                paint_icon(p, b.shrink(2.0), Icon::Check, if fill_on == DANGER { DANGER_INK } else { Color32::WHITE });
            } else {
                p.rect(b, 3.0, BAR, Stroke::new(1.0, ring), StrokeKind::Inside);
            }
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(title.clone());
                if let Some(s) = sub {
                    ui.label(s);
                }
            });
        })
        .response;
    let resp = ui.interact(r.rect, r.id.with("check"), Sense::click());
    if resp.clicked() {
        *on = !*on;
    }
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, *on, title.text()));
    pointer_on_hover(&resp);
    resp
}

/// Small radio choice with a text label (file system picker).
pub fn radio_choice(ui: &mut Ui, selected: bool, label: &str) -> Response {
    let g = ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(14.0), Color32::PLACEHOLDER);
    let (rect, resp) = ui.allocate_exact_size(vec2(14.0 + 8.0 + g.size().x, 22.0), Sense::click());
    resp.widget_info(|| WidgetInfo::selected(WidgetType::RadioButton, true, selected, label));
    let p = ui.painter();
    let ring = if selected {
        ACCENT
    } else if resp.hovered() {
        LINE_HOVER
    } else {
        TEXT3
    };
    radio(p, pos2(rect.left() + 7.0, rect.center().y), ring, selected.then_some(ACCENT));
    p.galley(pos2(rect.left() + 22.0, rect.center().y - g.size().y / 2.0), g, TEXT);
    pointer_on_hover(&resp);
    resp
}

/// Rounded chip: icon + text (identity warnings, error count).
pub fn chip(ui: &mut Ui, icon: Option<Icon>, label: &str, h: f32, size: f32, fill: Color32, stroke: Color32, fg: Color32) -> Response {
    let g = ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(size), Color32::PLACEHOLDER);
    let isz = (size + 1.0).round();
    let pad = if h > 30.0 { 12.0 } else { 8.0 };
    let iw = if icon.is_some() { isz + 6.0 } else { 0.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(pad * 2.0 + iw + g.size().x, h), Sense::hover());
    let p = ui.painter();
    p.rect(rect, 4.0, fill, Stroke::new(1.0, stroke), StrokeKind::Inside);
    let mut x = rect.left() + pad;
    if let Some(i) = icon {
        paint_icon(p, Rect::from_center_size(pos2(x + isz / 2.0, rect.center().y), Vec2::splat(isz)), i, fg);
        x += iw;
    }
    p.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, fg);
    resp
}

/// The 3 x 3 logo in the top bar.
pub fn logo(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(17.0), Sense::hover());
    let colors = [OK, OK, OK, OK, OK, hex(0x4f7fb8), hex(0x4f7fb8), hex(0x363a40), hex(0x363a40)];
    for (i, c) in colors.into_iter().enumerate() {
        let min = rect.min + vec2((i % 3) as f32 * 6.0, (i / 3) as f32 * 6.0);
        ui.painter().rect_filled(Rect::from_min_size(min, Vec2::splat(5.0)), 0.0, c);
    }
}

/// Text at a position, anchored like `align`.
pub fn paint_text(p: &egui::Painter, pos: Pos2, align: Align2, s: &str, size: f32, color: Color32) -> Rect {
    p.text(pos, align, s, FontId::proportional(size), color)
}
