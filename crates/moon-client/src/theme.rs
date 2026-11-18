//! The 2moon look ("nuit lunaire"): deep black, bone-white moon, one acid accent.
//! Palette, fonts, the animated sky backdrop and the hand-drawn widgets used by the GUI.

use std::sync::Arc;
use std::time::Instant;

use eframe::egui::emath::Rot2;

use eframe::egui::{
    self, epaint::Vertex, pos2, vec2, Align2, Color32, ColorImage, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Mesh, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, TextureHandle, TextureOptions, Ui, Vec2,
};

// ---------------------------------------------------------------- palette

pub const BG: Color32 = Color32::from_rgb(0x09, 0x09, 0x0B);
pub const SURFACE: Color32 = Color32::from_rgb(0x12, 0x12, 0x15);
pub const SURFACE2: Color32 = Color32::from_rgb(0x1C, 0x1C, 0x21);
pub const BORDER: Color32 = Color32::from_rgb(0x2A, 0x2A, 0x31);
pub const DIVIDER: Color32 = Color32::from_rgb(0x22, 0x22, 0x28);
pub const BORDER_STRONG: Color32 = Color32::from_rgb(0x3F, 0x3F, 0x46);
pub const TEXT: Color32 = Color32::from_rgb(0xF5, 0xF5, 0xF2);
pub const TEXT2: Color32 = Color32::from_rgb(0xD4, 0xD4, 0xD8);
pub const MUTED: Color32 = Color32::from_rgb(0xA1, 0xA1, 0xAA);
pub const FAINT: Color32 = Color32::from_rgb(0x71, 0x71, 0x7A);
pub const ACCENT: Color32 = Color32::from_rgb(0xCB, 0xFF, 0x4D);
pub const CRATER: Color32 = Color32::from_rgb(0xCB, 0xCB, 0xC4);
/// Errors and "muted" states (the design's orange option).
pub const WARN: Color32 = Color32::from_rgb(0xFF, 0x8A, 0x3D);

/// White at `a` (0..1) opacity.
pub fn white(a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(255, 255, 255, (a * 255.0) as u8)
}

/// `c` at `a` (0..1) opacity.
pub fn alpha(c: Color32, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a * 255.0) as u8)
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    a.lerp_to_gamma(b, t.clamp(0.0, 1.0))
}

// ---------------------------------------------------------------- fonts

const INTER: &[u8] = include_bytes!("../assets/fonts/Inter-Regular.ttf");
const INTER_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/Inter-SemiBold.ttf");
const GROTESK_BOLD: &[u8] = include_bytes!("../assets/fonts/SpaceGrotesk-Bold.ttf");
const MONO: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");
const MONO_BOLD: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf");

fn install_fonts(ctx: &egui::Context) {
    let mut f = FontDefinitions::default();
    for (name, bytes) in [("inter", INTER), ("inter-semibold", INTER_SEMIBOLD), ("grotesk", GROTESK_BOLD), ("mono", MONO), ("mono-bold", MONO_BOLD)] {
        f.font_data.insert(name.into(), Arc::new(FontData::from_static(bytes)));
    }
    // egui's own fonts stay as fallbacks (symbols the subset does not cover).
    let prop = f.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mono = f.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let with = |first: &str, rest: &[String]| std::iter::once(first.to_string()).chain(rest.iter().cloned()).collect::<Vec<_>>();
    f.families.insert(FontFamily::Proportional, with("inter", &prop));
    f.families.insert(FontFamily::Monospace, with("mono", &mono));
    f.families.insert(FontFamily::Name("semibold".into()), with("inter-semibold", &prop));
    f.families.insert(FontFamily::Name("display".into()), with("grotesk", &prop));
    f.families.insert(FontFamily::Name("mono-bold".into()), with("mono-bold", &mono));
    ctx.set_fonts(f);
}

/// Inter 400.
pub fn body(size: f32) -> FontId {
    FontId::proportional(size)
}
/// Inter 600.
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}
/// Space Grotesk 700 (titles).
pub fn display(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("display".into()))
}
/// JetBrains Mono 400.
pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}
/// JetBrains Mono 700.
pub fn mono_bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("mono-bold".into()))
}

/// Text with extra tracking (letter-spacing in points), as a galley.
pub fn spaced(ui: &Ui, text: &str, font: FontId, color: Color32, tracking: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::default();
    job.append(text, 0.0, egui::TextFormat { font_id: font, color, extra_letter_spacing: tracking, ..Default::default() });
    ui.painter().layout_job(job)
}

/// Uppercase mono "eyebrow" label (as a widget).
pub fn eyebrow(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let g = spaced(ui, &text.to_uppercase(), mono(12.0), color, 1.4);
    let (r, resp) = ui.allocate_exact_size(g.size(), Sense::hover());
    ui.painter().galley(r.min, g, color);
    resp
}

/// Plain text with a given font and colour, wrapped to the available width.
pub fn text(ui: &mut Ui, s: impl Into<String>, font: FontId, color: Color32) -> Response {
    ui.add(egui::Label::new(egui::RichText::new(s).font(font).color(color)).wrap())
}

// ---------------------------------------------------------------- style + textures

pub struct Look {
    moon: TextureHandle,
    stars: TextureHandle,
    start: Instant,
}

fn load_png(ctx: &egui::Context, name: &str, bytes: &[u8], opts: TextureOptions) -> TextureHandle {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png).expect("bundled image").to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    ctx.load_texture(name, ColorImage::from_rgba_unmultiplied(size, img.as_raw()), opts)
}

impl Look {
    pub fn new(ctx: &egui::Context) -> Self {
        install_fonts(ctx);
        ctx.set_theme(egui::ThemePreference::Dark);
        ctx.all_styles_mut(|style| {
            style.spacing.item_spacing = vec2(10.0, 8.0);
            style.spacing.button_padding = vec2(12.0, 6.0);
            style.spacing.scroll.floating = true;
            let v = &mut style.visuals;
            *v = egui::Visuals::dark();
            v.panel_fill = BG;
            v.window_fill = SURFACE;
            v.window_stroke = Stroke::new(1.0, BORDER);
            v.extreme_bg_color = BG;
            v.faint_bg_color = SURFACE;
            v.override_text_color = Some(TEXT);
            v.hyperlink_color = TEXT;
            v.selection.bg_fill = alpha(ACCENT, 0.35);
            v.selection.stroke = Stroke::new(1.0, ACCENT);
            v.text_cursor.stroke = Stroke::new(2.0, ACCENT);
            v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, DIVIDER);
            v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT2);
            v.widgets.inactive.bg_fill = SURFACE2;
            v.widgets.inactive.weak_bg_fill = SURFACE2;
            v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT2);
            v.widgets.hovered.bg_fill = BORDER;
            v.widgets.hovered.weak_bg_fill = BORDER;
            v.widgets.hovered.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
            v.widgets.active.bg_fill = BORDER_STRONG;
            v.widgets.active.weak_bg_fill = BORDER_STRONG;
        });
        Self {
            moon: load_png(ctx, "moon", include_bytes!("../assets/moon.png"), TextureOptions::LINEAR),
            stars: load_png(ctx, "stars", include_bytes!("../assets/stars.png"), TextureOptions::LINEAR_REPEAT),
            start: Instant::now(),
        }
    }

    /// Seconds since start (drives every animation).
    pub fn t(&self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }
}

// ---------------------------------------------------------------- backdrop

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scene {
    /// Big moon top right (host list).
    Home,
    /// Smaller moon top right (settings, apps).
    Side,
    /// Moon centred above a dialog (pairing, connecting).
    Centered,
}

/// 0 → 1 → 0 over `period` seconds, eased (CSS `ease-in-out ... alternate`).
fn swing(t: f32, period: f32) -> f32 {
    let x = (t / period).rem_euclid(2.0);
    let x = if x > 1.0 { 2.0 - x } else { x };
    x * x * (3.0 - 2.0 * x)
}

impl Look {
    /// Night sky, drifting stars, floating moon with its halo, and the veil that keeps text readable.
    pub fn paint_backdrop(&self, painter: &Painter, rect: Rect, scene: Scene) {
        let t = self.t();
        painter.rect_filled(rect, 0.0, BG);
        // Two star layers drifting diagonally; the near one twinkles.
        self.stars(painter, rect, vec2(1200.0, 800.0), vec2(-1200.0, 800.0) * (t / 220.0).fract(), 0.75);
        self.stars(painter, rect, vec2(520.0, 347.0), vec2(-1040.0, 347.0) * (t / 140.0).fract(), 0.12 + 0.33 * swing(t, 5.0));

        let w = rect.width();
        let (size, center, drift) = match scene {
            Scene::Home => {
                let s = (w * 0.72).clamp(560.0, 1040.0);
                let k = s / 1040.0;
                (s, pos2(rect.right() - 220.0 * k, rect.top() + 190.0 * k), vec2(-24.0, 18.0))
            }
            Scene::Side => {
                let s = (w * 0.54).clamp(420.0, 780.0);
                let k = s / 780.0;
                (s, pos2(rect.right() - 150.0 * k, rect.top() + 90.0 * k), vec2(-24.0, 18.0))
            }
            Scene::Centered => {
                let s = (w * 0.7).min(rect.height() * 1.0).clamp(360.0, 720.0);
                (s, pos2(rect.center().x, rect.top() + s * 0.25), vec2(0.0, 22.0))
            }
        };
        let center = center + drift * swing(t, 16.0);
        // Halo (radial gradient), breathing.
        let breathe = 0.7 + 0.3 * swing(t, 8.0);
        radial(painter, center, &[(0.0, 0.13 * breathe), (0.43 * size, 0.05 * breathe), (0.65 * size, 0.0)], TEXT);
        // The moon, slowly rocking (libration).
        let angle = (-5.0 + 10.0 * swing(t, 40.0)).to_radians();
        let mut m = Mesh::with_texture(self.moon.id());
        m.add_rect_with_uv(Rect::from_center_size(center, Vec2::splat(size)), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        m.rotate(Rot2::from_angle(angle), center);
        painter.add(Shape::mesh(m));

        // Veil.
        let sky = Rect::from_min_size(rect.min, vec2(rect.width(), rect.height().max(980.0)));
        match scene {
            Scene::Home | Scene::Side => {
                let (a, b) = if scene == Scene::Home { ((0.34, 0.82), (0.66, 0.25)) } else { ((0.38, 0.8), (0.72, 0.2)) };
                gradient(painter, sky, true, &[(0.0, 1.0), a, b, (1.0, 0.0)], BG);
                let from = if scene == Scene::Home { 0.55 } else { 0.45 };
                let sky = Rect::from_min_size(rect.min, vec2(rect.width(), 980.0));
                gradient(painter, sky, false, &[(from, 0.0), (1.0, 1.0)], BG);
                if rect.bottom() > sky.bottom() {
                    painter.rect_filled(Rect::from_min_max(pos2(rect.left(), sky.bottom()), rect.max), 0.0, BG);
                }
            }
            Scene::Centered => gradient(painter, rect, false, &[(0.2, 0.0), (0.55, 0.7), (0.85, 1.0), (1.0, 1.0)], BG),
        }
        painter.ctx().request_repaint_after(std::time::Duration::from_millis(33));
    }

    fn stars(&self, painter: &Painter, rect: Rect, tile: Vec2, offset: Vec2, opacity: f32) {
        // background-position moves the pattern: uv = (pos - offset) / tile.
        let uv0 = (rect.min.to_vec2() - offset) / tile;
        let uv1 = uv0 + rect.size() / tile;
        let mut m = Mesh::with_texture(self.stars.id());
        m.add_rect_with_uv(rect, Rect::from_min_max(uv0.to_pos2(), uv1.to_pos2()), white(opacity));
        painter.add(Shape::mesh(m));
    }
}

/// Linear gradient of `color` at the given (position 0..1, opacity) stops; `horizontal` = left → right.
pub fn gradient(painter: &Painter, rect: Rect, horizontal: bool, stops: &[(f32, f32)], color: Color32) {
    let mut m = Mesh::default();
    for (i, &(p, a)) in stops.iter().enumerate() {
        let c = alpha(color, a);
        let (p0, p1) = if horizontal {
            let x = rect.left() + rect.width() * p;
            (pos2(x, rect.top()), pos2(x, rect.bottom()))
        } else {
            let y = rect.top() + rect.height() * p;
            (pos2(rect.left(), y), pos2(rect.right(), y))
        };
        m.colored_vertex(p0, c);
        m.colored_vertex(p1, c);
        if i > 0 {
            let b = (i as u32) * 2;
            m.add_triangle(b - 2, b - 1, b);
            m.add_triangle(b - 1, b, b + 1);
        }
    }
    painter.add(Shape::mesh(m));
}

/// Radial gradient of `color` at the given (radius, opacity) stops.
fn radial(painter: &Painter, c: Pos2, stops: &[(f32, f32)], color: Color32) {
    const N: u32 = 96;
    let mut m = Mesh::default();
    for &(r, a) in stops {
        let col = alpha(color, a);
        for i in 0..N {
            let th = i as f32 / N as f32 * std::f32::consts::TAU;
            m.vertices.push(Vertex { pos: c + vec2(th.cos(), th.sin()) * r, uv: egui::epaint::WHITE_UV, color: col });
        }
    }
    for ring in 1..stops.len() as u32 {
        let (a, b) = ((ring - 1) * N, ring * N);
        for i in 0..N {
            let j = (i + 1) % N;
            m.add_triangle(a + i, a + j, b + i);
            m.add_triangle(a + j, b + j, b + i);
        }
    }
    painter.add(Shape::mesh(m));
}

// ---------------------------------------------------------------- icons

#[derive(Clone, Copy)]
pub enum Icon {
    Play,
    Plus(f32),
    Check,
    Lock,
    Back,
}

pub fn paint_icon(p: &Painter, icon: Icon, r: Rect, color: Color32) {
    // 16×16 design grid.
    let s = r.width() / 16.0;
    let at = |x: f32, y: f32| r.min + vec2(x * s, y * s);
    let st = Stroke::new(1.6 * s, color);
    match icon {
        Icon::Play => {
            p.add(Shape::convex_polygon(vec![at(3.5, 2.0), at(13.5, 8.0), at(3.5, 14.0)], color, Stroke::NONE));
        }
        Icon::Plus(angle) => {
            let rot = Rot2::from_angle(angle);
            let c = r.center();
            for (a, b) in [(vec2(0.0, -5.0), vec2(0.0, 5.0)), (vec2(-5.0, 0.0), vec2(5.0, 0.0))] {
                p.line_segment([c + rot * (a * s), c + rot * (b * s)], st);
            }
        }
        Icon::Check => {
            p.add(Shape::line(vec![at(3.0, 8.5), at(6.0, 11.5), at(13.0, 4.5)], st));
        }
        Icon::Lock => {
            p.rect_stroke(Rect::from_min_max(at(3.0, 7.0), at(13.0, 14.0)), 1.5 * s, st, StrokeKind::Middle);
            let mut pts = vec![at(5.5, 7.0)];
            for i in 0..=12 {
                let th = std::f32::consts::PI * (1.0 + i as f32 / 12.0);
                pts.push(at(8.0 + 2.5 * th.cos(), 5.0 + 2.5 * th.sin()));
            }
            pts.push(at(10.5, 7.0));
            p.add(Shape::line(pts, st));
        }
        Icon::Back => {
            p.add(Shape::line(vec![at(9.5, 3.5), at(5.0, 8.0), at(9.5, 12.5)], st));
        }
    }
}

/// Moon phase glyph: 0 new (outline), 1 waxing crescent, 2 first quarter, 3 full.
pub fn paint_phase(p: &Painter, c: Pos2, radius: f32, phase: u8, color: Color32) {
    let st = Stroke::new((radius / 5.0).max(1.0), color);
    if phase >= 3 {
        p.circle_filled(c, radius, color);
        return;
    }
    p.circle_stroke(c, radius, st);
    // Lit part: right half-disc minus an ellipse of horizontal radius k·r.
    let k = match phase {
        1 => 0.5,
        2 => 0.0,
        _ => return,
    };
    let mut m = Mesh::default();
    const N: u32 = 24;
    for i in 0..=N {
        let th = -std::f32::consts::FRAC_PI_2 + std::f32::consts::PI * i as f32 / N as f32;
        m.colored_vertex(c + vec2(th.cos(), th.sin()) * radius, color);
        m.colored_vertex(c + vec2(th.cos() * radius * k, th.sin() * radius), color);
        if i > 0 {
            let b = i * 2;
            m.add_triangle(b - 2, b - 1, b);
            m.add_triangle(b - 1, b, b + 1);
        }
    }
    p.add(Shape::mesh(m));
}

/// Phase glyph as a widget.
pub fn phase(ui: &mut Ui, size: f32, phase: u8, color: Color32) -> Response {
    let (r, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    paint_phase(ui.painter(), r.center(), size * 0.43, phase, color);
    resp
}

/// The four phases in a row, one lit at a time (a moon-shaped spinner).
pub fn phases_spinner(ui: &mut Ui, t: f32) {
    let lit = ((t * 2.0) as usize) % 4;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for i in 0..4u8 {
            phase(ui, 14.0, i, if i as usize == lit { ACCENT } else { FAINT });
        }
    });
}

/// The 2moon logo: an accent moon behind a bone-white one, with craters.
pub fn paint_logo(p: &Painter, r: Rect) {
    let s = r.height() / 32.0;
    let at = |x: f32, y: f32| r.min + vec2(x * s, y * s);
    p.circle_filled(at(31.5, 16.0), 12.0 * s, ACCENT);
    for (x, y, rr) in [(36.0, 11.0, 2.4), (38.2, 19.5, 1.6), (32.5, 23.0, 1.9)] {
        p.circle_filled(at(x, y), rr * s, alpha(BG, 0.16));
    }
    p.circle(at(17.0, 16.0), 12.5 * s, TEXT, Stroke::new(2.6 * s, BG));
    for (x, y, rr) in [(12.5, 12.0, 2.8), (20.5, 20.5, 3.3), (21.5, 10.0, 1.6), (11.5, 21.0, 1.3), (25.0, 15.0, 1.0)] {
        p.circle_filled(at(x, y), rr * s, CRATER);
    }
}

/// Monitor outline (24×24 design grid).
pub fn paint_monitor(p: &Painter, r: Rect, color: Color32) {
    let s = r.width() / 24.0;
    let at = |x: f32, y: f32| r.min + vec2(x * s, y * s);
    let st = Stroke::new(1.6 * s, color);
    p.rect_stroke(Rect::from_min_max(at(2.5, 4.0), at(21.5, 16.5)), 2.0 * s, st, StrokeKind::Middle);
    p.line_segment([at(8.0, 20.5), at(16.0, 20.5)], st);
    p.line_segment([at(12.0, 16.5), at(12.0, 20.5)], st);
}

/// Gamepad outline (24×24 design grid).
pub fn paint_gamepad(p: &Painter, r: Rect, color: Color32) {
    let s = r.width() / 24.0;
    let at = |x: f32, y: f32| r.min + vec2(x * s, y * s);
    let st = Stroke::new(1.6 * s, color);
    p.rect_stroke(Rect::from_min_max(at(2.0, 6.5), at(22.0, 17.5)), 5.5 * s, st, StrokeKind::Middle);
    p.line_segment([at(5.5, 12.0), at(10.5, 12.0)], st);
    p.line_segment([at(8.0, 9.5), at(8.0, 14.5)], st);
    p.circle_filled(at(15.5, 13.2), 1.1 * s, color);
    p.circle_filled(at(18.0, 10.8), 1.1 * s, color);
}

/// Rounded square tile holding an icon (host and app cards).
pub fn icon_tile(ui: &mut Ui, draw: impl FnOnce(&Painter, Rect)) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(52.0), Sense::hover());
    ui.painter().rect_filled(r, 14.0, white(0.06));
    draw(ui.painter(), Rect::from_center_size(r.center(), Vec2::splat(26.0)));
}

// ---------------------------------------------------------------- widgets

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Btn {
    /// Accent fill, dark text (primary action).
    Accent,
    /// Bone-white fill, dark text.
    Light,
    /// Thin light border.
    Outline,
    /// Bone-white border, white text.
    OutlineStrong,
    /// No border: a text link with an icon.
    Ghost,
}

/// A button from the design. `width`: `None` hugs the label, `Some(w)` fills `w`.
pub fn button(ui: &mut Ui, kind: Btn, icon: Option<Icon>, label: &str, width: Option<f32>, enabled: bool) -> Response {
    let font = match kind {
        Btn::Accent | Btn::Light | Btn::OutlineStrong => semibold(15.0),
        _ => body(15.0),
    };
    let fg = match kind {
        Btn::Accent | Btn::Light => BG,
        Btn::Outline => TEXT2,
        Btn::OutlineStrong | Btn::Ghost => TEXT,
    };
    let g = ui.painter().layout_no_wrap(label.to_string(), font, Color32::PLACEHOLDER);
    let icon_w = if icon.is_some() { 14.0 + 8.0 } else { 0.0 };
    let pad = if kind == Btn::Ghost { 8.0 } else { 20.0 };
    let content_w = g.size().x + icon_w;
    let w = width.unwrap_or(content_w + pad * 2.0);
    let h = 48.0;
    let (r, resp) = ui.allocate_exact_size(vec2(w, h), if enabled { Sense::click() } else { Sense::hover() });
    let hover = enabled && resp.hovered();
    let p = ui.painter();
    let fade = if enabled { 1.0 } else { 0.4 };
    match kind {
        Btn::Accent => {
            p.rect_filled(r, 12.0, alpha(mix(ACCENT, Color32::WHITE, if hover { 0.25 } else { 0.0 }), fade));
        }
        Btn::Light => {
            p.rect_filled(r, 12.0, alpha(if hover { Color32::WHITE } else { TEXT }, fade));
        }
        Btn::Outline => {
            p.rect(r, 12.0, if hover { white(0.06) } else { Color32::TRANSPARENT }, Stroke::new(1.0, white(0.2 * fade)), StrokeKind::Inside);
        }
        Btn::OutlineStrong => {
            p.rect(r, 12.0, if hover { white(0.08) } else { Color32::TRANSPARENT }, Stroke::new(1.0, alpha(TEXT, fade)), StrokeKind::Inside);
        }
        Btn::Ghost => {
            if hover {
                p.rect_filled(r, 10.0, white(0.06));
            }
        }
    }
    let fg = alpha(fg, fade);
    let x0 = r.center().x - content_w / 2.0;
    if let Some(i) = icon {
        paint_icon(p, i, Rect::from_center_size(pos2(x0 + 7.0, r.center().y), Vec2::splat(14.0)), fg);
    }
    p.galley(pos2(x0 + icon_w, r.center().y - g.size().y / 2.0), g, fg);
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

#[derive(Clone, Copy)]
pub enum PillKind {
    Filled(Color32),
    Outline(Color32),
}

/// Status pill: phase glyph + uppercase mono label.
pub fn pill(ui: &mut Ui, kind: PillKind, ph: u8, label: &str) -> Response {
    let (fg, fill, stroke) = match kind {
        PillKind::Filled(c) => (BG, c, Stroke::NONE),
        PillKind::Outline(c) => (c, Color32::TRANSPARENT, Stroke::new(1.0, c)),
    };
    let g = spaced(ui, label, mono_bold(12.0), fg, 1.0);
    let size = vec2(12.0 + 12.0 + 8.0 + g.size().x + 12.0, 28.0);
    let (r, resp) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    p.rect(r, CornerRadius::same(255), fill, stroke, StrokeKind::Inside);
    paint_phase(p, pos2(r.left() + 18.0, r.center().y), 5.2, ph, fg);
    p.galley(pos2(r.left() + 32.0, r.center().y - g.size().y / 2.0), g, fg);
    resp
}

/// Keyboard key chip; `accent` highlights the decisive key.
pub fn kbd(ui: &mut Ui, key: &str, accent: bool) -> Response {
    let (font, fg) = if accent { (mono_bold(12.0), BG) } else { (mono(12.0), if key == "…" { MUTED } else { TEXT }) };
    let g = ui.painter().layout_no_wrap(key.to_string(), font, fg);
    let (r, resp) = ui.allocate_exact_size(vec2(g.size().x + 14.0, 24.0), Sense::hover());
    let p = ui.painter();
    if accent {
        p.rect_filled(r, 6.0, ACCENT);
    } else {
        p.rect(r, 6.0, SURFACE, Stroke::new(1.0, BORDER_STRONG), StrokeKind::Inside);
        p.line_segment([pos2(r.left() + 3.0, r.bottom() - 1.0), pos2(r.right() - 3.0, r.bottom() - 1.0)], Stroke::new(1.0, BORDER_STRONG));
    }
    p.galley(r.center() - g.size() / 2.0, g, fg);
    resp
}

/// iOS-like switch (52×30): accent track when on.
pub fn switch(ui: &mut Ui, on: &mut bool) -> Response {
    let (r, mut resp) = ui.allocate_exact_size(vec2(52.0, 30.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(resp.id, *on, 0.15);
    let p = ui.painter();
    let track = mix(SURFACE2, ACCENT, t);
    let border = mix(BORDER_STRONG, ACCENT, t);
    p.rect(r, CornerRadius::same(255), track, Stroke::new(1.0, border), StrokeKind::Inside);
    let x = egui::lerp((r.left() + 15.0)..=(r.right() - 15.0), t);
    let knob = mix(MUTED, BG, t);
    p.circle_filled(pos2(x, r.center().y), 11.0, if resp.hovered() && t < 0.5 { TEXT2 } else { knob });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn seg_layout(ui: &Ui, labels: &[&str], font: &FontId, min_w: f32) -> Vec<(Arc<egui::Galley>, f32)> {
    labels
        .iter()
        .map(|l| {
            let g = ui.painter().layout_no_wrap(l.to_string(), font.clone(), Color32::PLACEHOLDER);
            let w = (g.size().x + 28.0).max(min_w);
            (g, w)
        })
        .collect()
}

/// Natural width of a segmented control.
pub fn segmented_width(ui: &Ui, labels: &[&str], mono_font: bool, min_w: f32) -> f32 {
    let font = if mono_font { mono(14.0) } else { body(14.0) };
    seg_layout(ui, labels, &font, min_w).iter().map(|(_, w)| w + 4.0).sum::<f32>() + 4.0 + 2.0
}

/// Segmented control: one choice among a few, the selected one bone-white. Wraps when narrow.
pub fn segmented<T: PartialEq + Clone>(ui: &mut Ui, value: &mut T, opts: &[(T, &str)], mono_font: bool, min_w: f32) -> bool {
    let font = if mono_font { mono(14.0) } else { body(14.0) };
    let labels: Vec<&str> = opts.iter().map(|o| o.1).collect();
    let items = seg_layout(ui, &labels, &font, min_w);
    let max_w = ui.available_width();
    // Lay out in rows of at most `max_w`.
    let (pad, gap, h) = (4.0, 4.0, 40.0);
    let mut pos = Vec::new();
    let (mut x, mut y, mut row_w, mut total_w) = (pad, pad, 0.0f32, 0.0f32);
    for (_, w) in &items {
        if x > pad && x + w + pad > max_w {
            x = pad;
            y += h + gap;
        }
        pos.push(vec2(x, y));
        x += w + gap;
        row_w = row_w.max(x - gap + pad);
        total_w = total_w.max(row_w);
    }
    let size = vec2(total_w + 2.0, y + h + pad + 2.0);
    let (r, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect(r, 12.0, BG, Stroke::new(1.0, BORDER), StrokeKind::Inside);
    let mut changed = false;
    for (i, ((g, w), o)) in items.into_iter().zip(opts).enumerate() {
        let br = Rect::from_min_size(r.min + vec2(1.0, 1.0) + pos[i], vec2(w, h));
        let resp = ui.interact(br, ui.scope_id().with(("seg", i, o.1)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
        let sel = *value == o.0;
        if resp.clicked() && !sel {
            *value = o.0.clone();
            changed = true;
        }
        let p = ui.painter();
        let fg = if sel {
            p.rect_filled(br, 8.0, TEXT);
            BG
        } else if resp.hovered() {
            p.rect_filled(br, 8.0, white(0.05));
            TEXT
        } else {
            MUTED
        };
        p.galley(br.center() - g.size() / 2.0, g, fg);
    }
    changed
}

/// Horizontal slider on a logarithmic scale (accent fill + knob).
pub fn slider_log(ui: &mut Ui, value: &mut f32, min: f32, max: f32) -> Response {
    let w = ui.available_width();
    let (r, mut resp) = ui.allocate_exact_size(vec2(w, 24.0), Sense::click_and_drag());
    let to_t = |v: f32| ((v / min).ln() / (max / min).ln()).clamp(0.0, 1.0);
    if let Some(pos) = resp.interact_pointer_pos() {
        let t = ((pos.x - r.left() - 9.0) / (r.width() - 18.0)).clamp(0.0, 1.0);
        let v = (min * (max / min).powf(t)).round();
        if v != *value {
            *value = v;
            resp.mark_changed();
        }
    }
    let t = to_t(*value);
    let p = ui.painter();
    let track = Rect::from_center_size(r.center(), vec2(r.width(), 6.0));
    p.rect_filled(track, 3.0, SURFACE2);
    let x = r.left() + 9.0 + (r.width() - 18.0) * t;
    p.rect_filled(Rect::from_min_max(track.min, pos2(x, track.max.y)), 3.0, ACCENT);
    let rad = if resp.hovered() || resp.dragged() { 10.0 } else { 9.0 };
    p.circle(pos2(x, r.center().y), rad, ACCENT, Stroke::new(2.0, BG));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Single-line text field in the design's style (48 px, mono).
pub fn text_input(ui: &mut Ui, value: &mut String, hint: &str, width: f32) -> Response {
    let (r, _) = ui.allocate_exact_size(vec2(width, 48.0), Sense::hover());
    let id = ui.scope_id().with(("input", hint));
    let focused = ui.memory(|m| m.has_focus(id));
    ui.painter().rect(r, 12.0, BG, Stroke::new(1.0, if focused { ACCENT } else { BORDER_STRONG }), StrokeKind::Inside);
    let edit = egui::TextEdit::singleline(value)
        .id(id)
        .frame(egui::Frame::NONE)
        .font(mono(15.0))
        .text_color(TEXT)
        .hint_text(egui::RichText::new(hint).font(mono(15.0)).color(FAINT))
        .align(Align2::LEFT_CENTER)
        .desired_width(r.width() - 32.0);
    ui.place(r.shrink2(vec2(16.0, 0.0)), edit)
}

/// Rounded-rectangle outline path (for dashed borders).
pub fn rounded_path(r: Rect, rad: f32) -> Vec<Pos2> {
    let mut pts = Vec::new();
    let corners = [(r.right() - rad, r.top() + rad, -90.0f32), (r.right() - rad, r.bottom() - rad, 0.0), (r.left() + rad, r.bottom() - rad, 90.0), (r.left() + rad, r.top() + rad, 180.0)];
    for (cx, cy, start) in corners {
        for i in 0..=6 {
            let th = (start + 90.0 * i as f32 / 6.0).to_radians();
            pts.push(pos2(cx + rad * th.cos(), cy + rad * th.sin()));
        }
    }
    pts.push(pts[0]);
    pts
}

/// Card surface: translucent dark glass, 18 px radius.
pub fn card(border: Color32) -> egui::Frame {
    egui::Frame::new().fill(Color32::from_rgba_unmultiplied(18, 18, 21, 214)).stroke(Stroke::new(1.0, border)).corner_radius(18).inner_margin(28)
}

/// Opaque section panel (settings).
pub fn panel() -> egui::Frame {
    egui::Frame::new().fill(SURFACE).stroke(Stroke::new(1.0, BORDER)).corner_radius(18)
}

/// Thin horizontal rule across the available width.
pub fn rule(ui: &mut Ui, color: Color32) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().line_segment([r.left_center(), r.right_center()], Stroke::new(1.0, color));
}

/// Centre `text` in `r`.
pub fn centered_text(p: &Painter, r: Rect, text: &str, font: FontId, color: Color32) {
    p.text(r.center(), Align2::CENTER_CENTER, text, font, color);
}
