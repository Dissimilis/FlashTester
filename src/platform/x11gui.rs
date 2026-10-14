//! Linux GUI host: an X11 window drawn by a small CPU rasterizer, so the GUI fits in
//! the static musl binary (winit and glutin would dlopen libX11, libxkbcommon and
//! libGL, which a static binary cannot). x11rb speaks the X protocol in pure Rust;
//! Wayland desktops run the window through XWayland.
//!
//! Rendering matches egui's own OpenGL painter: premultiplied gamma-space colours,
//! vertex colour times texture, blend ONE / ONE_MINUS_SRC_ALPHA.

use egui::epaint::textures::TextureFilter;
use egui::epaint::{ClippedPrimitive, ImageData, ImageDelta, Primitive, TextureId, Vertex};
use egui::{CursorIcon, Event, Key, Modifiers, OutputCommand, PointerButton, Pos2, Rect, Vec2, ViewportCommand, ViewportId};
use std::collections::HashMap;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::properties::{WmSizeHints, WmSizeHintsSpecification};
use x11rb::protocol::Event as XEvent;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        _NET_WM_NAME,
        UTF8_STRING,
        CLIPBOARD,
        TARGETS,
        INCR,
        FLASHTESTER_PASTE,
    }
}

enum Msg {
    X(XEvent),
    Repaint(Duration),
    Lost(String),
}

/// Run `app` in a window of `size` (at least `min`) until it is closed.
pub fn run(title: &str, size: [f32; 2], min: [f32; 2], mut app: impl FnMut(&mut egui::Ui)) -> Result<(), String> {
    let (conn, screen) = RustConnection::connect(None).map_err(|e| format!("cannot connect to the X display: {e}"))?;
    let conn = Arc::new(conn);
    let mut w = Win::new(conn.clone(), screen, title, size, min).map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::channel();
    let tx2 = tx.clone();
    std::thread::spawn(move || {
        loop {
            match conn.wait_for_event() {
                Ok(e) => {
                    if tx.send(Msg::X(e)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Msg::Lost(e.to_string()));
                    break;
                }
            }
        }
    });
    let ctx = egui::Context::default();
    ctx.set_request_repaint_callback(move |info| {
        let _ = tx2.send(Msg::Repaint(info.delay));
    });
    let start = Instant::now();
    let mut due = Some(Instant::now());
    loop {
        let wait = due.map_or(Duration::from_secs(3600), |d| d.saturating_duration_since(Instant::now()));
        let first = match rx.recv_timeout(wait) {
            Ok(m) => Some(m),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };
        let msgs: Vec<Msg> = first.into_iter().chain(rx.try_iter()).collect();
        let mut xev = vec![];
        for m in msgs {
            match m {
                Msg::X(e) => xev.push(e),
                Msg::Repaint(d) => due = Some(due.map_or(Instant::now() + d, |t| t.min(Instant::now() + d))),
                Msg::Lost(e) => return Err(format!("lost the X display: {e}")),
            }
        }
        if w.handle(&xev, &ctx).map_err(|e| e.to_string())? {
            due = Some(Instant::now());
        }
        if w.quit {
            return Ok(());
        }
        if due.is_some_and(|d| d <= Instant::now()) {
            due = None;
            let delay = w.frame(&ctx, start, &mut app).map_err(|e| e.to_string())?;
            if delay < Duration::from_secs(3600) {
                due = Some(Instant::now() + delay);
            }
        }
    }
}

type Res<T> = Result<T, Box<dyn std::error::Error>>;

struct Win {
    conn: Arc<RustConnection>,
    atoms: Atoms,
    win: Window,
    gc: Gcontext,
    pixmap: Pixmap,
    depth: u8,
    canvas: Canvas,
    textures: HashMap<TextureId, Texture>,
    keys: (u8, u8, Vec<u32>),
    /// State masks of NumLock and AltGr, from the modifier mapping.
    numlock: u16,
    altgr: u16,
    events: Vec<Event>,
    mods: Modifiers,
    sent_mods: Modifiers,
    focused: bool,
    mapped: bool,
    quit: bool,
    native_ppp: f32,
    clipboard: Option<String>,
    cursor_font: Option<Font>,
    cursors: HashMap<u16, Cursor>,
    cursor: u16,
}

impl Win {
    fn new(conn: Arc<RustConnection>, screen: usize, title: &str, size: [f32; 2], min: [f32; 2]) -> Res<Self> {
        let setup = conn.setup();
        let s = &setup.roots[screen];
        let visual = s.allowed_depths.iter().filter(|d| d.depth == s.root_depth).flat_map(|d| &d.visuals).find(|v| v.visual_id == s.root_visual);
        let bpp = setup.pixmap_formats.iter().find(|f| f.depth == s.root_depth).map(|f| f.bits_per_pixel);
        let ok = visual.is_some_and(|v| v.class == VisualClass::TRUE_COLOR && (v.red_mask, v.green_mask, v.blue_mask) == (0xff0000, 0xff00, 0xff));
        if !ok || bpp != Some(32) || setup.image_byte_order != ImageOrder::LSB_FIRST {
            return Err(format!("unsupported X display format (depth {}, {bpp:?} bits per pixel)", s.root_depth).into());
        }
        let (root, depth) = (s.root, s.root_depth);
        let atoms = Atoms::new(&*conn)?.reply()?;
        let native_ppp = xft_dpi(&conn, root).map_or(1.0, |d| (d / 96.0).clamp(0.5, 4.0));
        // Centred on the primary monitor, at most 90% of it: (0, 0) of the X screen can
        // be off every monitor when monitors are offset.
        let (mx, my, mw, mh) = monitor(&conn, root).unwrap_or((0, 0, s.width_in_pixels, s.height_in_pixels));
        let pw = ((size[0] * native_ppp) as u16).min((mw as f32 * 0.9) as u16).max(400);
        let ph = ((size[1] * native_ppp) as u16).min((mh as f32 * 0.9) as u16).max(300);
        let (px, py) = (mx + (mw.saturating_sub(pw) / 2) as i16, my + (mh.saturating_sub(ph) / 2) as i16);
        let win = conn.generate_id()?;
        let mask = EventMask::EXPOSURE
            | EventMask::STRUCTURE_NOTIFY
            | EventMask::KEY_PRESS
            | EventMask::KEY_RELEASE
            | EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE
            | EventMask::POINTER_MOTION
            | EventMask::LEAVE_WINDOW
            | EventMask::FOCUS_CHANGE;
        conn.create_window(depth, win, root, px, py, pw, ph, 0, WindowClass::INPUT_OUTPUT, s.root_visual, &CreateWindowAux::new().event_mask(mask).background_pixel(s.black_pixel))?;
        let mut hints = WmSizeHints::new();
        hints.position = Some((WmSizeHintsSpecification::ProgramSpecified, px as i32, py as i32));
        hints.size = Some((WmSizeHintsSpecification::ProgramSpecified, pw as i32, ph as i32));
        // Never larger than the window we open, which fits the monitor.
        hints.min_size = Some((((min[0] * native_ppp) as u16).min(pw) as i32, ((min[1] * native_ppp) as u16).min(ph) as i32));
        hints.set_normal_hints(&*conn, win)?;
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, title.as_bytes())?;
        conn.change_property8(PropMode::REPLACE, win, atoms._NET_WM_NAME, atoms.UTF8_STRING, title.as_bytes())?;
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_CLASS, AtomEnum::STRING, b"flashtester\0FlashTester\0")?;
        conn.change_property32(PropMode::REPLACE, win, atoms.WM_PROTOCOLS, AtomEnum::ATOM, &[atoms.WM_DELETE_WINDOW])?;
        // WM_HINTS with the input flag: some window managers only focus windows that ask.
        conn.change_property32(PropMode::REPLACE, win, AtomEnum::WM_HINTS, AtomEnum::WM_HINTS, &[1, 1, 0, 0, 0, 0, 0, 0, 0])?;
        let gc = conn.generate_id()?;
        conn.create_gc(gc, win, &CreateGCAux::new().graphics_exposures(0))?;
        let font = conn.generate_id()?;
        let cursor_font = conn.open_font(font, b"cursor")?.check().ok().map(|_| font);
        conn.map_window(win)?;
        let mut w = Win {
            conn: conn.clone(),
            atoms,
            win,
            gc,
            pixmap: NONE,
            depth,
            canvas: Canvas::default(),
            textures: HashMap::new(),
            keys: (0, 0, vec![]),
            numlock: 0,
            altgr: 0,
            events: vec![],
            mods: Modifiers::default(),
            sent_mods: Modifiers::default(),
            focused: false,
            mapped: true,
            quit: false,
            native_ppp,
            clipboard: None,
            cursor_font,
            cursors: HashMap::new(),
            cursor: 0,
        };
        w.resize(pw, ph)?;
        w.load_keymap()?;
        conn.flush()?;
        Ok(w)
    }

    fn resize(&mut self, w: u16, h: u16) -> Res<()> {
        if (w as usize, h as usize) == (self.canvas.w, self.canvas.h) {
            return Ok(());
        }
        self.canvas = Canvas { w: w as usize, h: h as usize, px: vec![0; w as usize * h as usize * 4] };
        if self.pixmap != NONE {
            self.conn.free_pixmap(self.pixmap)?;
        }
        self.pixmap = self.conn.generate_id()?;
        self.conn.create_pixmap(self.depth, self.pixmap, self.win, w.max(1), h.max(1))?;
        Ok(())
    }

    fn load_keymap(&mut self) -> Res<()> {
        let s = self.conn.setup();
        let (min, max) = (s.min_keycode, s.max_keycode);
        let r = self.conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        let per = r.keysyms_per_keycode as usize;
        let m = self.conn.get_modifier_mapping()?.reply()?;
        let mask_of = |wanted: &[u32]| {
            let n = m.keycodes.len() / 8;
            (0..8).find(|&i| m.keycodes[i * n..(i + 1) * n].iter().any(|&kc| kc >= min && r.keysyms.get((kc - min) as usize * per..(kc - min + 1) as usize * per).is_some_and(|ks| ks.iter().any(|s| wanted.contains(s))))).map_or(0, |i| 1u16 << i)
        };
        use xkeysym::key as k;
        self.numlock = mask_of(&[k::Num_Lock]);
        self.altgr = mask_of(&[k::ISO_Level3_Shift, k::Mode_switch]);
        self.keys = (min, r.keysyms_per_keycode, r.keysyms);
        Ok(())
    }

    /// Turn X events into egui events; true if egui should run a frame.
    fn handle(&mut self, evs: &[XEvent], ctx: &egui::Context) -> Res<bool> {
        let mut repaint = false;
        let ppp = ctx.pixels_per_point();
        let pos = |x: i16, y: i16| Pos2::new(x as f32 / ppp, y as f32 / ppp);
        for (i, e) in evs.iter().enumerate() {
            repaint = true;
            match e {
                XEvent::KeyPress(k) => self.key(k, true),
                XEvent::KeyRelease(k) => {
                    // Auto-repeat sends release + press with the same time: keep the key down.
                    let repeat = matches!(evs.get(i + 1), Some(XEvent::KeyPress(n)) if n.detail == k.detail && n.time == k.time);
                    if !repeat {
                        self.key(k, false);
                    }
                }
                XEvent::ButtonPress(b) | XEvent::ButtonRelease(b) => {
                    let pressed = matches!(e, XEvent::ButtonPress(_));
                    self.mods = mods(u16::from(b.state));
                    let wheel = match b.detail {
                        4 => Some(Vec2::new(0.0, 1.0)),
                        5 => Some(Vec2::new(0.0, -1.0)),
                        6 => Some(Vec2::new(1.0, 0.0)),
                        7 => Some(Vec2::new(-1.0, 0.0)),
                        _ => None,
                    };
                    let button = match b.detail {
                        1 => Some(PointerButton::Primary),
                        2 => Some(PointerButton::Middle),
                        3 => Some(PointerButton::Secondary),
                        8 => Some(PointerButton::Extra1),
                        9 => Some(PointerButton::Extra2),
                        _ => None,
                    };
                    if let (Some(delta), true) = (wheel, pressed) {
                        self.events.push(Event::MouseWheel { unit: egui::MouseWheelUnit::Line, delta, phase: egui::TouchPhase::Move, modifiers: self.mods });
                    } else if let Some(button) = button {
                        self.events.push(Event::PointerButton { pos: pos(b.event_x, b.event_y), button, pressed, modifiers: self.mods });
                    }
                }
                XEvent::MotionNotify(m) => self.events.push(Event::PointerMoved(pos(m.event_x, m.event_y))),
                XEvent::LeaveNotify(_) => self.events.push(Event::PointerGone),
                XEvent::FocusIn(_) | XEvent::FocusOut(_) => {
                    self.focused = matches!(e, XEvent::FocusIn(_));
                    self.mods = Modifiers::default();
                    self.events.push(Event::WindowFocused(self.focused));
                }
                XEvent::Expose(x) => {
                    repaint = false;
                    if x.count == 0 {
                        self.present()?;
                    }
                }
                XEvent::ConfigureNotify(c) => self.resize(c.width, c.height)?,
                XEvent::MappingNotify(_) => self.load_keymap()?,
                XEvent::ClientMessage(c) => {
                    if c.type_ == self.atoms.WM_PROTOCOLS && c.data.as_data32()[0] == self.atoms.WM_DELETE_WINDOW {
                        self.quit = true;
                    }
                }
                XEvent::SelectionRequest(r) => self.answer_selection(r)?,
                XEvent::SelectionClear(_) => self.clipboard = None,
                XEvent::SelectionNotify(n) => {
                    if n.property != NONE {
                        let r = self.conn.get_property(true, self.win, self.atoms.FLASHTESTER_PASTE, AtomEnum::ANY, 0, 1 << 24)?.reply()?;
                        // Large transfers (INCR) are not supported; nothing pasted then.
                        if r.type_ != self.atoms.INCR {
                            self.events.push(Event::Paste(String::from_utf8_lossy(&r.value).into_owned()));
                        }
                    }
                }
                _ => repaint = false,
            }
        }
        self.conn.flush()?;
        Ok(repaint)
    }

    fn key(&mut self, k: &KeyPressEvent, pressed: bool) {
        let st = u16::from(k.state);
        let (shift, lock, numlock, altgr) = (st & 1 != 0, st & 2 != 0, st & self.numlock != 0, st & self.altgr != 0);
        self.mods = mods(st);
        let (min, per, syms) = (&self.keys.0, self.keys.1, &self.keys.2);
        let sym = |col: u8| xkeysym::keysym(xkeysym::KeyCode::new(k.detail as u32), col, xkeysym::KeyCode::new(*min as u32), per, syms).filter(|s| s.raw() != 0);
        let Some(base) = sym(0) else { return };
        // The typed symbol: shift level, AltGr (columns 4 and 5), keypad digits with NumLock.
        let typed = if altgr { sym(4 + shift as u8).or(sym(0)) } else if numlock && base.is_keypad_key() { sym(1) } else { sym(shift as u8) }.unwrap_or(base);
        let mut ch = typed.key_char();
        if lock && ch.is_some_and(char::is_alphabetic) {
            ch = ch.map(|c| if shift { c.to_lowercase().next().unwrap_or(c) } else { c.to_uppercase().next().unwrap_or(c) });
        }
        let key = egui_key(typed.raw()).or_else(|| egui_key(base.raw()));
        if let Some(key) = key {
            if pressed && self.mods.ctrl && !self.mods.alt {
                match key {
                    Key::C => self.events.push(Event::Copy),
                    Key::X => self.events.push(Event::Cut),
                    Key::V => {
                        let _ = self.conn.convert_selection(self.win, self.atoms.CLIPBOARD, self.atoms.UTF8_STRING, self.atoms.FLASHTESTER_PASTE, CURRENT_TIME);
                    }
                    _ => {}
                }
            }
            self.events.push(Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: self.mods });
        }
        if let Some(c) = ch.filter(|c| pressed && !c.is_control() && !self.mods.ctrl && !self.mods.alt) {
            self.events.push(Event::Text(c.to_string()));
        }
    }

    fn answer_selection(&mut self, r: &SelectionRequestEvent) -> Res<()> {
        let prop = if r.property == NONE { r.target } else { r.property };
        let string = Atom::from(AtomEnum::STRING);
        let ok = match &self.clipboard {
            Some(t) if r.target == self.atoms.UTF8_STRING || r.target == string => {
                self.conn.change_property8(PropMode::REPLACE, r.requestor, prop, r.target, t.as_bytes())?;
                true
            }
            Some(_) if r.target == self.atoms.TARGETS => {
                self.conn.change_property32(PropMode::REPLACE, r.requestor, prop, AtomEnum::ATOM, &[self.atoms.TARGETS, self.atoms.UTF8_STRING, string])?;
                true
            }
            _ => false,
        };
        let ev = SelectionNotifyEvent { response_type: SELECTION_NOTIFY_EVENT, sequence: 0, time: r.time, requestor: r.requestor, selection: r.selection, target: r.target, property: if ok { prop } else { NONE } };
        self.conn.send_event(false, r.requestor, EventMask::NO_EVENT, ev)?;
        Ok(())
    }

    /// Run egui, paint, show; returns when egui wants the next frame.
    fn frame(&mut self, ctx: &egui::Context, start: Instant, app: &mut impl FnMut(&mut egui::Ui)) -> Res<Duration> {
        let ppp = ctx.pixels_per_point();
        if self.mods != self.sent_mods {
            self.sent_mods = self.mods;
            self.events.insert(0, Event::ModifiersChanged(self.mods));
        }
        let mut raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(self.canvas.w as f32, self.canvas.h as f32) / ppp)),
            time: Some(start.elapsed().as_secs_f64()),
            events: std::mem::take(&mut self.events),
            focused: self.focused,
            ..Default::default()
        };
        raw.viewports.entry(ViewportId::ROOT).or_default().native_pixels_per_point = Some(self.native_ppp);
        let out = ctx.run_ui(raw, |ui| app(ui));
        for (id, deltas) in &out.textures_delta.set {
            for d in deltas {
                set_texture(&mut self.textures, *id, d);
            }
        }
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        self.canvas.px.fill(0);
        for p in &prims {
            paint(&mut self.canvas, p, &self.textures, out.pixels_per_point);
        }
        for id in &out.textures_delta.free {
            self.textures.remove(id);
        }
        self.present()?;
        self.set_cursor(out.platform_output.cursor_icon)?;
        for c in out.platform_output.commands {
            match c {
                OutputCommand::CopyText(t) if !t.is_empty() => {
                    self.clipboard = Some(t);
                    self.conn.set_selection_owner(self.win, self.atoms.CLIPBOARD, CURRENT_TIME)?;
                }
                OutputCommand::OpenUrl(u) => {
                    let _ = std::process::Command::new("xdg-open").arg(u.url).spawn();
                }
                _ => {}
            }
        }
        let mut delay = Duration::MAX;
        if let Some(v) = out.viewport_output.get(&ViewportId::ROOT) {
            delay = v.repaint_delay;
            for c in &v.commands {
                match c {
                    ViewportCommand::Visible(true) if !self.mapped => {
                        self.conn.map_window(self.win)?;
                        self.mapped = true;
                    }
                    ViewportCommand::Visible(false) if self.mapped => {
                        self.conn.unmap_window(self.win)?;
                        self.mapped = false;
                    }
                    ViewportCommand::Close => self.quit = true,
                    _ => {}
                }
            }
        }
        self.conn.flush()?;
        Ok(delay)
    }

    /// Canvas to the back pixmap in request-sized bands, then one copy to the window
    /// (no half-drawn frames on screen).
    fn present(&mut self) -> Res<()> {
        let (w, h) = (self.canvas.w, self.canvas.h);
        if w == 0 || h == 0 {
            return Ok(());
        }
        let rows = ((self.conn.maximum_request_bytes() - 64) / (w * 4)).max(1);
        for y in (0..h).step_by(rows) {
            let n = rows.min(h - y);
            let band = &self.canvas.px[y * w * 4..(y + n) * w * 4];
            self.conn.put_image(ImageFormat::Z_PIXMAP, self.pixmap, self.gc, w as u16, n as u16, 0, y as i16, 0, self.depth, band)?;
        }
        self.conn.copy_area(self.pixmap, self.win, self.gc, 0, 0, 0, 0, w as u16, h as u16)?;
        Ok(())
    }

    fn set_cursor(&mut self, icon: CursorIcon) -> Res<()> {
        let Some(font) = self.cursor_font else { return Ok(()) };
        // Glyphs of the X "cursor" font.
        let glyph = match icon {
            CursorIcon::PointingHand => 60,
            CursorIcon::Text | CursorIcon::VerticalText => 152,
            CursorIcon::ResizeHorizontal | CursorIcon::ResizeColumn | CursorIcon::ResizeEast | CursorIcon::ResizeWest => 108,
            CursorIcon::ResizeVertical | CursorIcon::ResizeRow | CursorIcon::ResizeNorth | CursorIcon::ResizeSouth => 116,
            CursorIcon::Wait | CursorIcon::Progress => 150,
            CursorIcon::Move | CursorIcon::AllScroll | CursorIcon::Grab | CursorIcon::Grabbing => 52,
            CursorIcon::NotAllowed | CursorIcon::NoDrop => 0,
            _ => 68,
        };
        if glyph == self.cursor {
            return Ok(());
        }
        let cur = match self.cursors.get(&glyph) {
            Some(c) => *c,
            None => {
                let c = self.conn.generate_id()?;
                self.conn.create_glyph_cursor(c, font, font, glyph, glyph + 1, 0, 0, 0, 0xffff, 0xffff, 0xffff)?;
                self.cursors.insert(glyph, c);
                c
            }
        };
        self.conn.change_window_attributes(self.win, &ChangeWindowAttributesAux::new().cursor(cur))?;
        self.cursor = glyph;
        Ok(())
    }
}

fn mods(st: u16) -> Modifiers {
    let (shift, ctrl, alt) = (st & 1 != 0, st & 4 != 0, st & 8 != 0);
    Modifiers { alt, ctrl, shift, mac_cmd: false, command: ctrl }
}

/// The monitor to open on, as x, y, width, height: the primary, else the top-most
/// (XWayland may flag none, and its pointer position is stale while the pointer is
/// over other apps).
fn monitor(conn: &RustConnection, root: Window) -> Option<(i16, i16, u16, u16)> {
    let mons = conn.randr_get_monitors(root, true).ok()?.reply().ok()?.monitors;
    let m = mons.iter().min_by_key(|m| (!m.primary, m.y, m.x))?;
    Some((m.x, m.y, m.width, m.height))
}

/// Desktop scale: Xft.dpi in the root window's resource database (GNOME and KDE set it).
fn xft_dpi(conn: &RustConnection, root: Window) -> Option<f32> {
    let r = conn.get_property(false, root, AtomEnum::RESOURCE_MANAGER, AtomEnum::STRING, 0, 1 << 16).ok()?.reply().ok()?;
    let db = String::from_utf8_lossy(&r.value).into_owned();
    db.lines().find_map(|l| l.strip_prefix("Xft.dpi:")).and_then(|v| v.trim().parse().ok())
}

fn egui_key(sym: u32) -> Option<Key> {
    use xkeysym::key as k;
    Some(match sym {
        k::BackSpace => Key::Backspace,
        k::Tab | k::ISO_Left_Tab | k::KP_Tab => Key::Tab,
        k::Return | k::KP_Enter => Key::Enter,
        k::Escape => Key::Escape,
        k::Delete | k::KP_Delete => Key::Delete,
        k::Insert | k::KP_Insert => Key::Insert,
        k::Home | k::KP_Home => Key::Home,
        k::End | k::KP_End => Key::End,
        k::Page_Up | k::KP_Page_Up => Key::PageUp,
        k::Page_Down | k::KP_Page_Down => Key::PageDown,
        k::Left | k::KP_Left => Key::ArrowLeft,
        k::Right | k::KP_Right => Key::ArrowRight,
        k::Up | k::KP_Up => Key::ArrowUp,
        k::Down | k::KP_Down => Key::ArrowDown,
        k::space | k::KP_Space => Key::Space,
        k::F1..=k::F12 => Key::from_name(&format!("F{}", sym - k::F1 + 1))?,
        _ => {
            let c = xkeysym::Keysym::new(sym).key_char()?;
            Key::from_name(&c.to_ascii_uppercase().to_string())?
        }
    })
}

#[derive(Default)]
struct Canvas {
    w: usize,
    h: usize,
    /// BGRX, 4 bytes per pixel: the X server's 32-bit LSB-first format.
    px: Vec<u8>,
}

struct Texture {
    w: usize,
    h: usize,
    /// Premultiplied RGBA.
    px: Vec<[u8; 4]>,
    linear: bool,
}

fn set_texture(textures: &mut HashMap<TextureId, Texture>, id: TextureId, d: &ImageDelta) {
    let ImageData::Color(img) = &d.image;
    let [w, h] = img.size;
    let px = img.pixels.iter().map(|c| c.to_array());
    match d.pos {
        None => {
            textures.insert(id, Texture { w, h, px: px.collect(), linear: d.options.magnification == TextureFilter::Linear });
        }
        Some([x0, y0]) => {
            if let Some(t) = textures.get_mut(&id) {
                for (i, p) in px.enumerate() {
                    t.px[(y0 + i / w) * t.w + x0 + i % w] = p;
                }
            }
        }
    }
}

impl Texture {
    fn sample(&self, u: f32, v: f32) -> [f32; 4] {
        let at = |x: isize, y: isize| {
            let p = self.px[y.clamp(0, self.h as isize - 1) as usize * self.w + x.clamp(0, self.w as isize - 1) as usize];
            [p[0] as f32, p[1] as f32, p[2] as f32, p[3] as f32]
        };
        let (x, y) = (u * self.w as f32, v * self.h as f32);
        if !self.linear {
            return at(x.floor() as isize, y.floor() as isize);
        }
        let (x, y) = (x - 0.5, y - 0.5);
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let (x0, y0) = (x0 as isize, y0 as isize);
        let (a, b, c, d) = (at(x0, y0), at(x0 + 1, y0), at(x0, y0 + 1), at(x0 + 1, y0 + 1));
        std::array::from_fn(|i| (a[i] * (1.0 - fx) + b[i] * fx) * (1.0 - fy) + (c[i] * (1.0 - fx) + d[i] * fx) * fy)
    }
}

fn paint(c: &mut Canvas, p: &ClippedPrimitive, textures: &HashMap<TextureId, Texture>, ppp: f32) {
    let Primitive::Mesh(mesh) = &p.primitive else { return };
    let Some(tex) = textures.get(&mesh.texture_id) else { return };
    let r = p.clip_rect;
    let clip = [
        ((r.min.x * ppp).round().max(0.0) as usize).min(c.w),
        ((r.min.y * ppp).round().max(0.0) as usize).min(c.h),
        ((r.max.x * ppp).round().max(0.0) as usize).min(c.w),
        ((r.max.y * ppp).round().max(0.0) as usize).min(c.h),
    ];
    if clip[0] >= clip[2] || clip[1] >= clip[3] {
        return;
    }
    for t in mesh.indices.as_chunks::<3>().0 {
        let v = [&mesh.vertices[t[0] as usize], &mesh.vertices[t[1] as usize], &mesh.vertices[t[2] as usize]];
        triangle(c, v, tex, clip, ppp);
    }
}

/// Fill one triangle, sampling pixel centres. Each edge function is anchored at the
/// edge's lower endpoint, so two triangles sharing an edge compute exactly opposite
/// values there and the tie rule gives each pixel on it to exactly one of them (no
/// double-blended seams on translucent shapes).
fn triangle(c: &mut Canvas, v: [&Vertex; 3], tex: &Texture, clip: [usize; 4], ppp: f32) {
    let p = v.map(|v| (v.pos.x * ppp, v.pos.y * ppp));
    let area = (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[1].1 - p[0].1) * (p[2].0 - p[0].0);
    if area == 0.0 || !area.is_finite() {
        return;
    }
    let s = area.signum();
    // Edge i is opposite vertex i: positive inside, |area| at vertex i.
    let e: [(f32, f32, f32); 3] = std::array::from_fn(|i| {
        let (a, b) = (p[(i + 1) % 3], p[(i + 2) % 3]);
        let (ea, eb) = (-s * (b.1 - a.1), s * (b.0 - a.0));
        let o = if (a.0, a.1) < (b.0, b.1) { a } else { b };
        (ea, eb, -(ea * o.0 + eb * o.1))
    });
    let take = |w: f32, (a, b, _): (f32, f32, f32)| w > 0.0 || (w == 0.0 && (a > 0.0 || (a == 0.0 && b > 0.0)));
    let (xs, ys) = (p.map(|q| q.0), p.map(|q| q.1));
    let x0 = (xs.iter().fold(f32::MAX, |a, &b| a.min(b)).floor().max(0.0) as usize).max(clip[0]);
    let x1 = (xs.iter().fold(f32::MIN, |a, &b| a.max(b)).ceil().max(0.0) as usize).min(clip[2]);
    let y0 = (ys.iter().fold(f32::MAX, |a, &b| a.min(b)).floor().max(0.0) as usize).max(clip[1]);
    let y1 = (ys.iter().fold(f32::MIN, |a, &b| a.max(b)).ceil().max(0.0) as usize).min(clip[3]);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let inv = 1.0 / area.abs();
    let col = v.map(|v| v.color.to_array().map(|x| x as f32));
    let uv = v.map(|v| (v.uv.x, v.uv.y));
    // Solid shapes (one colour, one texel) cover most pixels: their colour is worked out once.
    let solid = (col[0] == col[1] && col[1] == col[2] && uv[0] == uv[1] && uv[1] == uv[2]).then(|| {
        let t = tex.sample(uv[0].0, uv[0].1);
        std::array::from_fn::<f32, 4, _>(|i| col[0][i] * t[i] / 255.0)
    });
    for y in y0..y1 {
        let py = y as f32 + 0.5;
        // The row's span from the edge equations, widened for rounding; the exact
        // test below decides the pixels at its ends.
        let (mut lo, mut hi) = (x0 as f32, x1 as f32);
        for &(a, b, k) in &e {
            let base = b * py + k;
            if a > 0.0 {
                lo = lo.max(-base / a - 1.5);
            } else if a < 0.0 {
                hi = hi.min(-base / a + 1.5);
            } else if base < 0.0 {
                hi = lo;
            }
        }
        if lo >= hi {
            continue;
        }
        // Inside pixels form one run (the triangle is convex and each edge test is
        // monotonic along the row): find its ends, then fill without testing.
        let inside = |x: usize| {
            let px = x as f32 + 0.5;
            (0..3).all(|i| take(e[i].0 * px + e[i].1 * py + e[i].2, e[i]))
        };
        let (mut xa, mut xb) = (lo.floor() as usize, (hi.ceil() as usize).min(x1));
        while xa < xb && !inside(xa) {
            xa += 1;
        }
        while xb > xa && !inside(xb - 1) {
            xb -= 1;
        }
        let row = &mut c.px[y * c.w * 4..(y + 1) * c.w * 4];
        if let Some(src) = solid.filter(|s| s[3] >= 254.5) {
            let bgr = [src[2], src[1], src[0]].map(|v| v.round().clamp(0.0, 255.0) as u8);
            for d in row[xa * 4..xb * 4].as_chunks_mut::<4>().0 {
                d[..3].copy_from_slice(&bgr);
            }
            continue;
        }
        for x in xa..xb {
            let src = solid.unwrap_or_else(|| {
                let px = x as f32 + 0.5;
                let l = e.map(|(a, b, k)| (a * px + b * py + k) * inv);
                let t = tex.sample(l[0] * uv[0].0 + l[1] * uv[1].0 + l[2] * uv[2].0, l[0] * uv[0].1 + l[1] * uv[1].1 + l[2] * uv[2].1);
                std::array::from_fn(|i| (l[0] * col[0][i] + l[1] * col[1][i] + l[2] * col[2][i]) * t[i] / 255.0)
            });
            let keep = 1.0 - src[3] / 255.0;
            let d = &mut row[x * 4..x * 4 + 3];
            // BGRX: d[0] blue, d[1] green, d[2] red.
            for (j, i) in [2usize, 1, 0].into_iter().enumerate() {
                d[j] = (src[i] + d[j] as f32 * keep).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}
