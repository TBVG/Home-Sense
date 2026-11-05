// SPDX-License-Identifier: AGPL-3.0-or-later
//! Floor-plan drawing. Every element drawn traces back to stored measurements or to something
//! the user placed. Unknown states are drawn dashed/gray. Nothing is drawn for missing data.

use analysis::{aploc::ApFit, map::{Barrier, Fix}, openings::Class};
use eframe::egui::{self, epaint::Shape, Align2, Color32, FontId, Painter, Pos2, Stroke};
use storage::{Floor, ObjectKind, OpeningKind, PlacedObject, UserWall};

pub const GRID_W: f32 = 30.0;
pub const GRID_H: f32 = 20.0;

/// Colour of the movement/presence cloud: warm amber, calm (not an alarm colour), distinct from the red used for alerts.
pub const PRESENCE: (u8, u8, u8) = (232, 150, 40);

pub struct Blob {
    pub x: f32,
    pub y: f32,
    pub radius_m: f32,
    /// How concentrated the location estimate is (0..1).
    pub confidence: f32,
    /// What the signal was: "movement", "movement (Wi-Fi only)" or "periodic motion (possible breathing)".
    pub kind: &'static str,
    /// What the app thinks is moving, or that it does not know.
    pub who: String,
    /// Standing height used for the 3D cloud: tall for a person, low for a pet.
    pub height_m: f32,
    /// Probability over map cells (x, y, share). The cloud follows these.
    pub cells: Vec<(f32, f32, f32)>,
    /// Set when only the sound distance was known (a ring around the computer).
    pub ring: Option<f32>,
    pub sources: String,
    pub breathing: Option<(f32, f32)>,
    pub heart: Option<(f32, f32)>,
}

pub struct PlanData<'a> {
    pub floor: &'a Floor,
    pub barriers: &'a [Barrier],
    pub classes: &'a [Class],
    pub fix: Option<&'a Fix>,
    pub blob: Option<Blob>,
    pub selected_opening: Option<&'a str>,
    /// Furniture and walls the user placed (empty when hidden). Drawn as plain shapes, never as measurements.
    pub objects: &'a [PlacedObject],
    pub walls: &'a [UserWall],
    pub selected_object: Option<&'a str>,
    /// Where the user put this computer on the map.
    pub computer: Option<(f32, f32)>,
    /// Access points whose position was estimated from the mapping walk.
    pub aps: &'a [ApFit],
    pub detail: bool,
    /// Draw text labels (off in the compact stacked view).
    pub labels: bool,
}

fn dark(a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(40, 40, 40, (a.clamp(0.0, 1.0) * 255.0) as u8)
}

fn dashed(p: &Painter, pts: &[Pos2], stroke: Stroke) {
    for s in Shape::dashed_line(pts, stroke, 5.0, 4.0) {
        p.add(s);
    }
}

fn hash(a: i32, b: i32) -> f32 {
    let mut h = (a as u32).wrapping_mul(374_761_393) ^ (b as u32).wrapping_mul(668_265_263);
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    ((h ^ (h >> 16)) & 0xffff) as f32 / 65535.0
}

pub fn draw_grid(p: &Painter, map: &dyn Fn(f32, f32) -> Pos2) {
    let c = Color32::from_gray(238);
    for i in 0..=(GRID_W as i32) {
        p.line_segment([map(i as f32, 0.0), map(i as f32, GRID_H)], st(1.0, c));
    }
    for j in 0..=(GRID_H as i32) {
        p.line_segment([map(0.0, j as f32), map(GRID_W, j as f32)], st(1.0, c));
    }
}

/// Colour used for each kind of furniture (muted, opaque: it is placed by the user, not measured).
pub fn kind_color(k: ObjectKind) -> Color32 {
    let (r, g, b) = match k {
        ObjectKind::Sofa => (122, 142, 172),
        ObjectKind::Bed => (190, 172, 160),
        ObjectKind::Table => (196, 166, 124),
        ObjectKind::Chair => (168, 138, 106),
        ObjectKind::Desk => (184, 154, 118),
        ObjectKind::Tv => (58, 60, 68),
        ObjectKind::Lamp => (240, 214, 146),
        ObjectKind::Plant => (96, 154, 104),
        ObjectKind::Shelf => (172, 142, 112),
        ObjectKind::Fridge => (226, 229, 233),
        ObjectKind::Wardrobe => (162, 128, 98),
        ObjectKind::Box => (190, 190, 196),
    };
    Color32::from_rgb(r, g, b)
}

pub fn draw_floor(p: &Painter, map: &dyn Fn(f32, f32) -> Pos2, px_per_m: f32, d: &PlanData) {
    let f = d.floor;

    // walls and furniture the user placed (underneath the measured overlays)
    for w in d.walls {
        p.line_segment([map(w.a.0, w.a.1), map(w.b.0, w.b.1)], st((0.14 * px_per_m).max(3.0), Color32::from_gray(72)));
    }
    for o in d.objects {
        let sel = d.selected_object == Some(o.id.as_str());
        let pts: Vec<Pos2> = o.corners().iter().map(|c| map(c.0, c.1)).collect();
        let fill = kind_color(o.kind);
        let stroke = if sel { st(2.0, Color32::from_rgb(40, 110, 220)) } else { st(1.0, Color32::from_gray(90)) };
        if matches!(o.kind, ObjectKind::Lamp | ObjectKind::Plant) {
            p.circle_filled(map(o.x, o.y), o.w / 2.0 * px_per_m, fill);
            p.circle_stroke(map(o.x, o.y), o.w / 2.0 * px_per_m, stroke);
        } else {
            p.add(Shape::convex_polygon(pts.clone(), fill, stroke));
        }
        if d.labels && (sel || o.label.is_some()) {
            let t = o.label.clone().unwrap_or_else(|| o.kind.name().to_string());
            p.text(map(o.x, o.y), Align2::CENTER_CENTER, t, FontId::proportional(9.0), Color32::from_gray(40));
        }
    }

    // signal barriers: opacity = confidence; dashed when low confidence
    for b in d.barriers {
        let (a, c) = (map(b.a.0, b.a.1), map(b.b.0, b.b.1));
        let col = Color32::from_rgba_unmultiplied(110, 110, 110, (40.0 + 150.0 * b.confidence) as u8);
        if b.confidence < 0.4 {
            dashed(p, &[a, c], st(5.0, col));
        } else {
            p.line_segment([a, c], st(8.0, col));
        }
        if d.labels && b.confidence >= 0.4 {
            let m = a + (c - a) * 0.5;
            p.text(m + egui::vec2(6.0, -6.0), Align2::LEFT_BOTTOM, format!("barrier {:.0}%", b.confidence * 100.0), FontId::proportional(10.0), Color32::from_gray(130));
        }
    }

    // sound echoes: dashed rings at measured distances (direction unknown)
    if d.detail {
        for rs in &f.room_scans {
            let c = map(rs.x, rs.y);
            p.circle_filled(c, 2.5, Color32::from_rgb(60, 110, 160));
            for e in &rs.echoes {
                let col = Color32::from_rgba_unmultiplied(60, 110, 160, (30.0 + 150.0 * e.confidence) as u8);
                let pts: Vec<Pos2> = (0..=72).map(|k| { let a = k as f32 / 72.0 * std::f32::consts::TAU; map(rs.x + e.distance_m * a.cos(), rs.y + e.distance_m * a.sin()) }).collect();
                dashed(p, &pts, st(1.0, col));
                if d.labels {
                    p.text(map(rs.x + e.distance_m, rs.y) + egui::vec2(3.0, 0.0), Align2::LEFT_CENTER, format!("{:.1} m", e.distance_m), FontId::proportional(9.0), col);
                }
            }
        }
        for s in &f.spots {
            p.circle_filled(map(s.x, s.y), 3.0, Color32::from_gray(150));
            if let (Some(l), true) = (&s.label, d.labels) {
                p.text(map(s.x, s.y) + egui::vec2(5.0, -5.0), Align2::LEFT_BOTTOM, l, FontId::proportional(10.0), Color32::from_gray(120));
            }
        }
    }

    // doors and windows
    for (i, o) in f.openings.iter().enumerate() {
        let class = d.classes.get(i).copied().unwrap_or(Class::Unknown);
        let len = if o.kind == OpeningKind::Door { 0.9 } else { 1.2 };
        let (ax, ay, bx, by) = if o.horizontal { (o.x - len / 2.0, o.y, o.x + len / 2.0, o.y) } else { (o.x, o.y - len / 2.0, o.x, o.y + len / 2.0) };
        let (a, b) = (map(ax, ay), map(bx, by));
        let sel = d.selected_opening == Some(o.id.as_str());
        let w = if sel { 4.0 } else { 2.5 };
        match (o.kind, class) {
            (_, Class::Unknown) => dashed(p, &[a, b], st(w, Color32::from_gray(170))),
            (OpeningKind::Door, Class::Closed(c)) => {
                p.line_segment([a, b], st(w, dark(0.4 + 0.6 * c)));
            }
            (OpeningKind::Door, Class::Open(c)) => {
                let col = dark(0.4 + 0.6 * c);
                let gap = (b - a) * 0.12;
                p.line_segment([a, a + gap], st(w, col));
                p.line_segment([b - gap, b], st(w, col));
                // swing: leaf at 90 degrees from hinge a, with the arc it sweeps
                let v = b - a;
                let perp = egui::vec2(-v.y, v.x);
                p.line_segment([a, a + perp], st(1.5, col));
                let pts: Vec<Pos2> = (0..=16).map(|k| { let t = k as f32 / 16.0 * std::f32::consts::FRAC_PI_2; a + v * t.cos() + perp * t.sin() }).collect();
                p.add(Shape::line(pts, st(1.0, col.gamma_multiply(0.6))));
            }
            (OpeningKind::Window, Class::Closed(c)) => {
                let col = Color32::from_rgba_unmultiplied(70, 120, 170, (80.0 + 175.0 * c) as u8);
                let off = if o.horizontal { egui::vec2(0.0, 2.5) } else { egui::vec2(2.5, 0.0) };
                p.line_segment([a + off, b + off], st(1.5, col));
                p.line_segment([a - off, b - off], st(1.5, col));
            }
            (OpeningKind::Window, Class::Open(c)) => {
                let col = Color32::from_rgba_unmultiplied(70, 120, 170, (80.0 + 175.0 * c) as u8);
                let (m1, m2) = (a + (b - a) * 0.35, a + (b - a) * 0.65);
                p.line_segment([a, m1], st(w, col));
                p.line_segment([m2, b], st(w, col));
            }
        }
        if d.labels {
            let txt = match class { Class::Open(c) => format!("open {:.0}%", c * 100.0), Class::Closed(c) => format!("closed {:.0}%", c * 100.0), Class::Unknown => "unknown".into() };
            let name = o.label.clone().unwrap_or_else(|| if o.kind == OpeningKind::Door { "door".into() } else { "window".into() });
            p.text(b + egui::vec2(4.0, 6.0), Align2::LEFT_TOP, format!("{name}: {txt}"), FontId::proportional(9.0), Color32::from_gray(120));
        }
    }

    // device location estimate (Wi-Fi fingerprint)
    if let (Some(fix), true) = (d.fix, d.detail) {
        let c = map(fix.x, fix.y);
        let col = Color32::from_rgba_unmultiplied(40, 130, 90, (50.0 + 180.0 * fix.confidence) as u8);
        let pts: Vec<Pos2> = (0..=48).map(|k| { let a = k as f32 / 48.0 * std::f32::consts::TAU; map(fix.x + fix.radius_m * a.cos(), fix.y + fix.radius_m * a.sin()) }).collect();
        dashed(p, &pts, st(1.5, col));
        p.circle_filled(c, 3.5, col);
        if d.labels {
            let place = fix.nearest_label.clone().map(|l| format!(" near {l}")).unwrap_or_default();
            p.text(map(fix.x, fix.y + fix.radius_m) + egui::vec2(0.0, 4.0), Align2::CENTER_TOP, format!("this computer (est.){place}, {:.0}%", fix.confidence * 100.0), FontId::proportional(10.0), col);
        }
    }

    // this computer, as placed by the user
    if let Some((cx, cy)) = d.computer {
        let q: Vec<Pos2> = [(-0.25f32, -0.17f32), (0.25, -0.17), (0.25, 0.17), (-0.25, 0.17)].iter().map(|(dx, dy)| map(cx + dx, cy + dy)).collect();
        p.add(Shape::convex_polygon(q, Color32::from_rgb(70, 78, 92), st(1.0, Color32::from_gray(30))));
        if d.labels {
            p.text(map(cx, cy) + egui::vec2(0.0, 11.0), Align2::CENTER_TOP, "this computer", FontId::proportional(9.0), Color32::from_gray(70));
        }
    }

    // estimated access points (from the signal pattern during the mapping walk)
    for a in d.aps {
        let c = map(a.x, a.y);
        let col = Color32::from_rgba_unmultiplied(80, 110, 160, 200);
        let pts: Vec<Pos2> = (0..=36).map(|k| { let t = k as f32 / 36.0 * std::f32::consts::TAU; map(a.x + a.radius_m * t.cos(), a.y + a.radius_m * t.sin()) }).collect();
        dashed(p, &pts, st(1.0, col.gamma_multiply(0.5)));
        p.add(Shape::convex_polygon(vec![c + egui::vec2(0.0, -5.0), c + egui::vec2(5.0, 0.0), c + egui::vec2(0.0, 5.0), c + egui::vec2(-5.0, 0.0)], col, Stroke::NONE));
        if d.labels {
            p.text(c + egui::vec2(7.0, -2.0), Align2::LEFT_CENTER, a.ssid.clone().unwrap_or_else(|| "access point".into()) + " (est.)", FontId::proportional(9.0), Color32::from_rgb(70, 95, 140));
        }
    }

    // movement: a grainy cloud that follows the probability map (as large as the uncertainty)
    if let (Some(b), true) = (&d.blob, d.detail) {
        let col = |a: f32| Color32::from_rgba_unmultiplied(PRESENCE.0, PRESENCE.1, PRESENCE.2, a as u8);
        let pmax = b.cells.iter().map(|c| c.2).fold(0.0f32, f32::max).max(1e-6);
        for (ci, (x, y, m)) in b.cells.iter().enumerate() {
            let n = ((m / pmax) * 9.0).ceil() as i32;
            for g in 0..n {
                let (jx, jy) = (hash(ci as i32 * 31 + g, 7), hash(ci as i32 * 17 + g, 11));
                p.circle_filled(map(x + (jx - 0.5) * 0.5, y + (jy - 0.5) * 0.5), 1.5, col(150.0 + 70.0 * (m / pmax)));
            }
        }
        if d.labels {
            let t = map(b.x, b.y) + egui::vec2(10.0, -12.0);
            let gray = Color32::from_gray(80);
            p.text(t, Align2::LEFT_TOP, format!("{} ({})", b.kind, b.who), FontId::proportional(10.0), gray);
            let breath = b.breathing.map_or("breathing: --".to_string(), |(v, cf)| format!("breathing: {v:.0}/min ({:.0}%)", cf * 100.0));
            let heart = b.heart.map_or("heart: --".to_string(), |(v, cf)| format!("heart: {v:.0} bpm, experimental ({:.0}%)", cf * 100.0));
            p.text(t + egui::vec2(0.0, 12.0), Align2::LEFT_TOP, breath, FontId::proportional(10.0), gray);
            p.text(t + egui::vec2(0.0, 24.0), Align2::LEFT_TOP, heart, FontId::proportional(10.0), gray);
            let place = if b.ring.is_some() { format!("location: ring at the measured distance only ({})", b.sources) } else { format!("location from {} ({:.0}% concentrated)", b.sources, b.confidence * 100.0) };
            p.text(t + egui::vec2(0.0, 36.0), Align2::LEFT_TOP, place, FontId::proportional(9.0), Color32::from_gray(110));
        }
    }
}

pub fn st(w: f32, c: Color32) -> Stroke {
    Stroke::new(w, c)
}
