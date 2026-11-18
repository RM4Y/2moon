//! A controller drawn in the Xbox layout: shows live what the host receives,
//! and highlights the control the setup is asking for.

use eframe::egui::{self, pos2, vec2, Color32, Painter, Pos2, Rect, Sense, Shape, Stroke};
use moon_proto::control::input::{pad, Gamepad};

use crate::padmap::Control;
use crate::theme as th;

/// Draw `g` at the current width (at most 560 px), highlighting `ask` (pulsing with `time`).
pub fn paint(ui: &mut egui::Ui, g: &Gamepad, ask: Option<Control>, time: f64) {
    let w = ui.available_width().min(560.0);
    let (r, _) = ui.allocate_exact_size(vec2(w, w * 0.62), Sense::hover());
    draw(ui.painter(), r, g, ask, time);
}

pub fn draw(p: &Painter, r: Rect, g: &Gamepad, ask: Option<Control>, time: f64) {
    let (w, h) = (r.width(), r.height());
    let at = |x: f32, y: f32| r.min + vec2(x * w, y * h);
    let u = w / 100.0; // size unit
    let on = |flag: u32| g.buttons & flag != 0;
    let pulse = 0.55 + 0.45 * ((time * 4.0).sin() as f32 * 0.5 + 0.5);
    let body = th::SURFACE2;
    let idle = th::BORDER;
    let lit = th::ACCENT;
    let glow = |p: &Painter, c: Pos2, rad: f32| {
        p.circle_filled(c, rad + 1.2 * u, th::alpha(lit, 0.18 * pulse));
        p.circle_stroke(c, rad + 0.6 * u, Stroke::new(0.5 * u, th::alpha(lit, pulse)));
    };
    let asked = |c: Control| ask == Some(c);

    // Triggers (LT / RT), behind the body: they fill as they are pressed.
    for (left, trig, ct) in [(true, g.left_trigger, Control::LT), (false, g.right_trigger, Control::RT)] {
        let tr = Rect::from_min_size(at(if left { 0.20 } else { 0.66 }, 0.02), vec2(0.14 * w, 0.13 * h));
        p.rect_filled(tr, 2.0 * u, idle);
        let fill = trig as f32 / 255.0;
        if fill > 0.0 {
            p.rect_filled(Rect::from_min_max(pos2(tr.left(), tr.bottom() - tr.height() * fill), tr.max), 2.0 * u, lit);
        }
        if asked(ct) {
            p.rect_filled(tr.expand(1.2 * u), 2.8 * u, th::alpha(lit, 0.18 * pulse));
            p.rect_stroke(tr.expand(0.6 * u), 2.6 * u, Stroke::new(0.5 * u, th::alpha(lit, pulse)), egui::StrokeKind::Outside);
        }
        let label_at = pos2(tr.center().x, tr.top() + 0.045 * h);
        p.text(label_at, egui::Align2::CENTER_CENTER, if left { "LT" } else { "RT" }, th::mono_bold(2.6 * u), if fill > 0.6 { th::BG } else { th::MUTED });
    }

    // Body: a wide rounded top and two grips, as a union of discs (same colour).
    let mut shapes = Vec::new();
    let mut disc = |c: Pos2, rad: f32| shapes.push(Shape::circle_filled(c, rad, body));
    for i in 0..=24 {
        let t = i as f32 / 24.0;
        // Grips: from the upper corners down and outwards.
        for side in [-1.0f32, 1.0] {
            let x = 0.5 + side * (0.25 + 0.10 * t);
            let y = 0.36 + 0.50 * t;
            disc(at(x, y), (13.5 - 3.5 * t) * u);
        }
        // Top bar between the grips.
        disc(at(0.25 + 0.5 * t, 0.33), 13.0 * u);
        // Lower middle (the bridge between the grips), slightly raised.
        disc(at(0.32 + 0.36 * t, 0.52), 11.0 * u);
    }
    p.extend(shapes);

    // Bumpers (LB / RB) on the top edge of the body.
    for (left, bump, cb) in [(true, on(pad::LB), Control::LB), (false, on(pad::RB), Control::RB)] {
        let br = Rect::from_min_size(at(if left { 0.17 } else { 0.66 }, 0.135), vec2(0.17 * w, 0.075 * h));
        if asked(cb) {
            p.rect_filled(br.expand(1.2 * u), 4.0 * u, th::alpha(lit, 0.18 * pulse));
            p.rect_stroke(br.expand(0.6 * u), 3.6 * u, Stroke::new(0.5 * u, th::alpha(lit, pulse)), egui::StrokeKind::Outside);
        }
        p.rect_filled(br, 3.0 * u, if bump { lit } else { idle });
        p.text(br.center(), egui::Align2::CENTER_CENTER, if left { "LB" } else { "RB" }, th::mono_bold(2.4 * u), if bump { th::BG } else { th::MUTED });
    }

    // Center buttons: Back (two windows) and Start (three lines), drawn.
    let small = 2.6 * u;
    for (c, flag, start, ctl) in [(at(0.42, 0.37), pad::BACK, false, Control::Back), (at(0.58, 0.37), pad::START, true, Control::Start)] {
        if asked(ctl) {
            glow(p, c, small);
        }
        let pressed = on(flag);
        p.circle_filled(c, small, if pressed { lit } else { idle });
        let ink = if pressed { th::BG } else { th::MUTED };
        let st = Stroke::new(0.32 * u, ink);
        if start {
            for dy in [-0.8, 0.0, 0.8] {
                p.line_segment([c + vec2(-1.1 * u, dy * u), c + vec2(1.1 * u, dy * u)], st);
            }
        } else {
            for o in [vec2(-0.4, -0.4), vec2(0.4, 0.4)] {
                p.rect_stroke(Rect::from_center_size(c + o * u, vec2(1.3 * u, 1.1 * u)), 0.2 * u, st, egui::StrokeKind::Middle);
            }
        }
    }
    let home = at(0.5, 0.25);
    if asked(Control::Home) {
        glow(p, home, 4.2 * u);
    }
    p.circle_filled(home, 4.2 * u, if on(pad::HOME) { lit } else { idle });
    p.circle_stroke(home, 2.4 * u, Stroke::new(0.5 * u, if on(pad::HOME) { th::BG } else { th::MUTED }));

    // Sticks: left upper-left, right lower-right (Xbox layout).
    for (c, x, y, click, cx, cy, cc) in [
        (at(0.29, 0.38), g.ls_x, g.ls_y, on(pad::LS), Control::LeftX, Control::LeftY, Control::LS),
        (at(0.62, 0.60), g.rs_x, g.rs_y, on(pad::RS), Control::RightX, Control::RightY, Control::RS),
    ] {
        let well = 7.5 * u;
        if asked(cx) || asked(cy) || asked(cc) {
            glow(p, c, well);
        }
        p.circle_filled(c, well, th::BG);
        p.circle_stroke(c, well, Stroke::new(0.4 * u, idle));
        let d = c + vec2(x as f32 / 32767.0, -(y as f32) / 32767.0) * (well - 4.0 * u);
        p.circle_filled(d, 4.6 * u, if click { lit } else { th::TEXT2 });
        p.circle_stroke(d, 4.6 * u, Stroke::new(0.4 * u, th::alpha(Color32::BLACK, 0.4)));
        // Which way to push.
        let arrow = if asked(cx) { Some(vec2(1.0, 0.0)) } else if asked(cy) { Some(vec2(0.0, -1.0)) } else { None };
        if let Some(dir) = arrow {
            let a = c + dir * (well + 2.2 * u);
            let b = a + dir * 3.0 * u;
            let st = Stroke::new(0.8 * u, th::alpha(lit, pulse));
            p.line_segment([a, b], st);
            let n = vec2(-dir.y, dir.x) * 1.6 * u;
            p.add(Shape::convex_polygon(vec![b + dir * 2.0 * u, b + n, b - n], th::alpha(lit, pulse), Stroke::NONE));
        }
    }

    // D-pad, lower left.
    let dc = at(0.38, 0.60);
    let arm = 3.2 * u;
    p.rect_filled(Rect::from_center_size(dc, vec2(arm * 3.0, arm)), 0.6 * u, idle);
    p.rect_filled(Rect::from_center_size(dc, vec2(arm, arm * 3.0)), 0.6 * u, idle);
    for (off, flag, ctl) in [(vec2(0.0, -1.0), pad::UP, Control::Up), (vec2(0.0, 1.0), pad::DOWN, Control::Down), (vec2(-1.0, 0.0), pad::LEFT, Control::Left), (vec2(1.0, 0.0), pad::RIGHT, Control::Right)] {
        let cell = Rect::from_center_size(dc + off * arm, vec2(arm, arm));
        if on(flag) {
            p.rect_filled(cell, 0.6 * u, lit);
        }
        if asked(ctl) {
            p.rect_filled(cell.expand(0.9 * u), 1.0 * u, th::alpha(lit, 0.18 * pulse));
            p.rect_stroke(cell.expand(0.5 * u), 1.0 * u, Stroke::new(0.5 * u, th::alpha(lit, pulse)), egui::StrokeKind::Outside);
        }
    }

    // Face buttons, upper right, in their Xbox colours (dimmed until pressed).
    let fc = at(0.71, 0.38);
    let gap = 5.2 * u;
    let fb = 3.2 * u;
    for (off, flag, label, col, ctl) in [
        (vec2(0.0, -1.0), pad::Y, "Y", Color32::from_rgb(0xF5, 0xC2, 0x42), Control::Y),
        (vec2(0.0, 1.0), pad::A, "A", Color32::from_rgb(0x5D, 0xD3, 0x6B), Control::A),
        (vec2(-1.0, 0.0), pad::X, "X", Color32::from_rgb(0x4A, 0x9B, 0xF5), Control::X),
        (vec2(1.0, 0.0), pad::B, "B", Color32::from_rgb(0xF0, 0x5A, 0x5A), Control::B),
    ] {
        let c = fc + off * gap;
        if asked(ctl) {
            glow(p, c, fb);
        }
        p.circle_filled(c, fb, if on(flag) { lit } else { idle });
        p.text(c, egui::Align2::CENTER_CENTER, label, th::mono_bold(3.0 * u), if on(flag) { th::BG } else { col });
    }
}
