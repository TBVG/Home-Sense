// SPDX-License-Identifier: AGPL-3.0-or-later
//! 3D view of the floor maps, drawn with a small software projector (orthographic, orbitable).
//!
//! Only measured or user-placed things are drawn: spots as pins, barrier zones as translucent wall slabs
//! (opacity = confidence, dashed when unsure), doors/windows as 3D openings (dashed when unknown),
//! sound echoes as rings on the floor, the computer's estimate, and a grainy 3D cloud for movement.
//! No house outline is invented: the floor plate is just the drawing canvas with its 1 m grid.

use crate::plan::{kind_color, st, PlanData, PRESENCE};
use analysis::openings::Class;
use eframe::egui::{self, epaint::Shape, Align2, Color32, FontId, Painter, Pos2, Rect, Stroke};
use storage::{ObjectKind, OpeningKind, PlacedObject};

pub const FLOOR_GAP: f32 = 4.6;
const WALL_H: f32 = 2.4;

#[derive(Clone, Copy)]
pub struct Camera {
    /// rotation about the vertical axis, radians
    pub yaw: f32,
    /// camera elevation above the horizon, radians (pi/2 = straight down)
    pub elev: f32,
    pub zoom: f32,
    /// Draw the walls nearest the viewer low, so the inside of the house can be seen.
    pub cutaway: bool,
}
impl Default for Camera {
    fn default() -> Self {
        Self { yaw: 0.55, elev: 0.82, zoom: 1.0, cutaway: true }
    }
}

#[derive(Clone, Copy)]
pub struct Proj {
    mx: f32,
    my: f32,
    yc: f32,
    ys: f32,
    ec: f32,
    es: f32,
    s: f32,
    ox: f32,
    oy: f32,
}

impl Proj {
    fn rot(&self, x: f32, y: f32) -> (f32, f32) {
        let (dx, dy) = (x - self.mx, y - self.my);
        (dx * self.yc - dy * self.ys, dx * self.ys + dy * self.yc)
    }
    pub fn p(&self, x: f32, y: f32, z: f32) -> Pos2 {
        let (rx, ry) = self.rot(x, y);
        egui::pos2(self.ox + self.s * rx, self.oy + self.s * (ry * self.es - z * self.ec))
    }
    /// larger = closer to the viewer
    fn close(&self, x: f32, y: f32, z: f32) -> f32 {
        let (_, ry) = self.rot(x, y);
        ry * self.ec + z * self.es
    }
    /// screen point -> plan coordinates on the horizontal plane at height z
    pub fn unproject(&self, pos: Pos2, z: f32) -> (f32, f32) {
        let rx = (pos.x - self.ox) / self.s;
        let ry = ((pos.y - self.oy) / self.s + z * self.ec) / self.es;
        (rx * self.yc + ry * self.ys + self.mx, -rx * self.ys + ry * self.yc + self.my)
    }
}

/// Plan region (x0, x1, y0, y1) in metres shown as the floor plate.
pub type Region = (f32, f32, f32, f32);

fn make_proj(cam: &Camera, rect: Rect, n_floors: usize, stacked: bool, reg: Region) -> Proj {
    let mut p = Proj { mx: (reg.0 + reg.1) / 2.0, my: (reg.2 + reg.3) / 2.0, yc: cam.yaw.cos(), ys: cam.yaw.sin(), ec: cam.elev.cos(), es: cam.elev.sin().max(0.2), s: 1.0, ox: 0.0, oy: 0.0 };
    let ztop = if stacked { (n_floors.max(1) - 1) as f32 * FLOOR_GAP + 3.0 } else { 3.0 };
    let mut pts = vec![];
    for (x, y) in [(reg.0, reg.2), (reg.1, reg.2), (reg.1, reg.3), (reg.0, reg.3)] {
        pts.push(p.p(x, y, -0.2));
        pts.push(p.p(x, y, ztop));
    }
    let (minx, maxx) = pts.iter().fold((f32::MAX, f32::MIN), |a, q| (a.0.min(q.x), a.1.max(q.x)));
    let (miny, maxy) = pts.iter().fold((f32::MAX, f32::MIN), |a, q| (a.0.min(q.y), a.1.max(q.y)));
    let s = (rect.width() / (maxx - minx)).min(rect.height() / (maxy - miny)) * 0.9 * cam.zoom;
    p.s = s;
    p.ox = rect.center().x - s * (minx + maxx) / 2.0;
    p.oy = rect.center().y - s * (miny + maxy) / 2.0;
    p
}

enum Prim {
    Poly { pts: Vec<Pos2>, fill: Color32, stroke: Stroke, dashed: bool },
    Line { pts: Vec<Pos2>, stroke: Stroke, dashed: bool },
    Dot { p: Pos2, r: f32, c: Color32 },
    Text { p: Pos2, al: Align2, s: String, sz: f32, c: Color32 },
}

fn draw(p: &Painter, pr: &Prim) {
    match pr {
        Prim::Poly { pts, fill, stroke, dashed } => {
            if fill.a() > 0 {
                p.add(Shape::convex_polygon(pts.clone(), *fill, Stroke::NONE));
            }
            if stroke.width > 0.0 {
                if *dashed {
                    let mut c = pts.clone();
                    c.push(pts[0]);
                    for s in Shape::dashed_line(&c, *stroke, 4.0, 3.0) {
                        p.add(s);
                    }
                } else {
                    p.add(Shape::closed_line(pts.clone(), *stroke));
                }
            }
        }
        Prim::Line { pts, stroke, dashed } => {
            if *dashed {
                for s in Shape::dashed_line(pts, *stroke, 4.0, 3.0) {
                    p.add(s);
                }
            } else {
                p.add(Shape::line(pts.clone(), *stroke));
            }
        }
        Prim::Dot { p: pos, r, c } => {
            p.circle_filled(*pos, *r, *c);
        }
        Prim::Text { p: pos, al, s, sz, c } => {
            p.text(*pos, *al, s, FontId::proportional(*sz), *c);
        }
    }
}

fn col(r: u8, g: u8, b: u8, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(r, g, b, a.clamp(0.0, 255.0) as u8)
}
fn shade(c: Color32, k: f32) -> Color32 {
    Color32::from_rgba_unmultiplied((c.r() as f32 * k) as u8, (c.g() as f32 * k) as u8, (c.b() as f32 * k) as u8, c.a())
}
fn hash3(a: i32, b: i32, c: i32) -> f32 {
    let mut h = (a as u32).wrapping_mul(374_761_393) ^ (b as u32).wrapping_mul(668_265_263) ^ (c as u32).wrapping_mul(2_147_483_647);
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    ((h ^ (h >> 16)) & 0xffff) as f32 / 65535.0
}

pub struct FloorItem<'a> {
    pub name: &'a str,
    pub data: PlanData<'a>,
}

struct Layers {
    ground: Vec<Prim>,
    upright: Vec<(f32, Prim)>,
    labels: Vec<Prim>,
}

struct Ctx<'a> {
    pr: &'a Proj,
    z0: f32,
    fade: f32,
    cutaway: bool,
}
impl Ctx<'_> {
    fn p(&self, x: f32, y: f32, z: f32) -> Pos2 {
        self.pr.p(x, y, self.z0 + z)
    }
    fn d(&self, x: f32, y: f32, z: f32) -> f32 {
        self.pr.close(x, y, self.z0 + z)
    }
    fn a(&self, base: f32) -> f32 {
        base * self.fade
    }
}

/// A prism over a plan-view quad (corners in order), from height h0 to h1. Only faces turned toward the viewer are drawn.
fn prism(cx: &Ctx, l: &mut Layers, ground: bool, quad: [(f32, f32); 4], h0: f32, h1: f32, fill: Color32, stroke: Stroke, dashed: bool) {
    let cen = (quad.iter().map(|q| q.0).sum::<f32>() / 4.0, quad.iter().map(|q| q.1).sum::<f32>() / 4.0);
    let mut faces: Vec<(f32, Prim)> = vec![];
    // sides facing the viewer
    for i in 0..4 {
        let (a, b) = (quad[i], quad[(i + 1) % 4]);
        let (mx, my) = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
        let mut n = (b.1 - a.1, -(b.0 - a.0));
        if n.0 * (mx - cen.0) + n.1 * (my - cen.1) < 0.0 {
            n = (-n.0, -n.1);
        }
        let rny = n.0 * cx.pr.ys + n.1 * cx.pr.yc; // y component of the normal after rotation
        if rny <= 0.0 && cx.pr.es < 0.98 {
            continue;
        }
        let pts = vec![cx.p(a.0, a.1, h0), cx.p(b.0, b.1, h0), cx.p(b.0, b.1, h1), cx.p(a.0, a.1, h1)];
        let k = 0.78 + 0.12 * (n.0 * cx.pr.yc - n.1 * cx.pr.ys).signum();
        faces.push((cx.d(mx, my, (h0 + h1) / 2.0), Prim::Poly { pts, fill: shade(fill, k), stroke, dashed }));
    }
    let top: Vec<Pos2> = quad.iter().map(|q| cx.p(q.0, q.1, h1)).collect();
    faces.push((cx.d(cen.0, cen.1, h1) + 0.01, Prim::Poly { pts: top, fill: shade(fill, 1.08), stroke, dashed }));
    for (d, f) in faces {
        if ground {
            l.ground.push(f);
        } else {
            l.upright.push((d, f));
        }
    }
}

fn seg_quad(a: (f32, f32), b: (f32, f32), half: f32) -> [(f32, f32); 4] {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt().max(1e-3);
    let (nx, ny) = (-dy / len * half, dx / len * half);
    [(a.0 + nx, a.1 + ny), (b.0 + nx, b.1 + ny), (b.0 - nx, b.1 - ny), (a.0 - nx, a.1 - ny)]
}

/// Furniture as a few boxes in the object's local frame: ((x0, x1), (y0, y1), (z0, z1), colour).
type Part = ((f32, f32), (f32, f32), (f32, f32), Color32);

fn object_parts(o: &PlacedObject) -> Vec<Part> {
    let (hw, hd, h) = (o.w / 2.0, o.d / 2.0, o.h);
    let base = kind_color(o.kind);
    let lighter = shade(base, 1.12);
    let dark = shade(base, 0.72);
    let mut v: Vec<Part> = vec![];
    let legs = |v: &mut Vec<Part>, top: f32, c: Color32| {
        let lw = 0.06f32.min(hw).min(hd);
        for (sx, sy) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let (x0, y0) = (sx * (hw - 0.04 - lw / 2.0), sy * (hd - 0.04 - lw / 2.0));
            v.push(((x0 - lw / 2.0, x0 + lw / 2.0), (y0 - lw / 2.0, y0 + lw / 2.0), (0.0, top), c));
        }
    };
    match o.kind {
        ObjectKind::Sofa => {
            let (bt, aw) = ((hd * 0.5).min(0.22), (hw * 0.18).min(0.2));
            v.push(((-hw, hw), (-hd, -hd + bt), (0.0, h), base)); // back
            v.push(((-hw, hw), (-hd + bt, hd), (0.0, h * 0.42), dark)); // base
            v.push(((-hw, -hw + aw), (-hd + bt, hd), (0.0, h * 0.72), base)); // arms
            v.push(((hw - aw, hw), (-hd + bt, hd), (0.0, h * 0.72), base));
            v.push(((-hw + aw, hw - aw), (-hd + bt, hd), (h * 0.42, h * 0.54), lighter)); // seat cushion
        }
        ObjectKind::Bed => {
            v.push(((-hw, hw), (-hd, hd), (0.0, h * 0.6), dark)); // frame
            v.push(((-hw + 0.04, hw - 0.04), (-hd + 0.06, hd - 0.04), (h * 0.6, h), Color32::from_rgb(238, 238, 240))); // mattress
            v.push(((-hw, hw), (-hd, -hd + 0.08), (0.0, h + 0.4), base)); // headboard
            v.push(((-hw + 0.1, -0.05), (-hd + 0.14, -hd + 0.5), (h, h + 0.12), Color32::from_rgb(250, 250, 252)));
            v.push(((0.05, hw - 0.1), (-hd + 0.14, -hd + 0.5), (h, h + 0.12), Color32::from_rgb(250, 250, 252)));
        }
        ObjectKind::Table | ObjectKind::Desk => {
            v.push(((-hw, hw), (-hd, hd), (h - 0.05, h), lighter));
            legs(&mut v, h - 0.05, dark);
        }
        ObjectKind::Chair => {
            v.push(((-hw, hw), (-hd, hd), (0.40, 0.46), lighter));
            v.push(((-hw, hw), (-hd, -hd + 0.05), (0.46, h), base));
            legs(&mut v, 0.40, dark);
        }
        ObjectKind::Tv => {
            v.push(((-hw * 0.3, hw * 0.3), (-hd, hd), (0.0, 0.08), Color32::from_rgb(70, 72, 80)));
            v.push(((-hw, hw), (-0.03, 0.03), (0.25, h), base));
        }
        ObjectKind::Lamp => {
            v.push(((-hw * 0.5, hw * 0.5), (-hd * 0.5, hd * 0.5), (0.0, 0.03), Color32::from_rgb(90, 90, 96)));
            v.push(((-0.02, 0.02), (-0.02, 0.02), (0.0, h * 0.82), Color32::from_rgb(90, 90, 96)));
            v.push(((-hw, hw), (-hd, hd), (h * 0.82, h), base));
        }
        ObjectKind::Plant => {
            v.push(((-hw * 0.55, hw * 0.55), (-hd * 0.55, hd * 0.55), (0.0, 0.3), Color32::from_rgb(190, 120, 90)));
            v.push(((-hw, hw), (-hd, hd), (0.3, h), base));
        }
        _ => v.push(((-hw, hw), (-hd, hd), (0.0, h), base)),
    }
    v
}

fn floor_content(cx: &Ctx, d: &PlanData, selected: bool, l: &mut Layers) {
    let f = d.floor;
    let labels = selected && d.labels;

    // walls and furniture the user placed: opaque plain shapes, clearly different from the translucent measured slabs
    // Walls on the viewer's side of the drawing are drawn low (cutaway) so the inside stays visible.
    let n_w = d.walls.len().max(1) as f32;
    let (mx, my) = (d.walls.iter().map(|w| (w.a.0 + w.b.0) / 2.0).sum::<f32>() / n_w, d.walls.iter().map(|w| (w.a.1 + w.b.1) / 2.0).sum::<f32>() / n_w);
    let ref_close = cx.d(mx, my, 0.0);
    for w in d.walls {
        let mid = ((w.a.0 + w.b.0) / 2.0, (w.a.1 + w.b.1) / 2.0);
        let front = cx.d(mid.0, mid.1, 0.0) > ref_close + 0.3;
        let height = if cx.cutaway && front { 0.45 } else { 2.5 };
        prism(cx, l, false, seg_quad(w.a, w.b, 0.07), 0.0, height, col(236, 236, 240, cx.a(255.0)), st(0.8, col(150, 152, 160, cx.a(255.0))), false);
    }
    for o in d.objects {
        let sel = d.selected_object == Some(o.id.as_str());
        for (xr, yr, zr, c) in object_parts(o) {
            let quad = [o.to_world(xr.0, yr.0), o.to_world(xr.1, yr.0), o.to_world(xr.1, yr.1), o.to_world(xr.0, yr.1)];
            let edge = if sel { st(1.4, col(40, 110, 220, cx.a(255.0))) } else { st(0.7, shade(c, 0.55)) };
            let fill = Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), cx.a(c.a() as f32).clamp(0.0, 255.0) as u8);
            prism(cx, l, false, quad, zr.0, zr.1, fill, edge, false);
        }
        if sel {
            let pts: Vec<Pos2> = o.corners().iter().map(|c| cx.p(c.0, c.1, 0.0)).collect();
            l.ground.push(Prim::Poly { pts, fill: Color32::TRANSPARENT, stroke: st(1.5, col(40, 110, 220, 255.0)), dashed: true });
        }
        if labels && (sel || o.label.is_some()) {
            let t = o.label.clone().unwrap_or_else(|| o.kind.name().to_string());
            l.labels.push(Prim::Text { p: cx.p(o.x, o.y, o.h + 0.5), al: Align2::CENTER_BOTTOM, s: t, sz: 10.0, c: col(70, 74, 84, 240.0) });
        }
    }

    // barrier zones as translucent wall slabs
    for b in d.barriers {
        let c = b.confidence;
        let fill = col(120, 126, 138, cx.a(50.0 + 120.0 * c));
        let stroke = st(1.0, col(60, 66, 78, cx.a(70.0 + 150.0 * c)));
        prism(cx, l, false, seg_quad(b.a, b.b, 0.18), 0.0, WALL_H, fill, stroke, c < 0.4);
        if labels && c >= 0.5 {
            let m = ((b.a.0 + b.b.0) / 2.0, (b.a.1 + b.b.1) / 2.0);
            l.labels.push(Prim::Text { p: cx.p(m.0, m.1, WALL_H + 0.25), al: Align2::CENTER_BOTTOM, s: format!("barrier {:.0}%", c * 100.0), sz: 10.0, c: col(90, 94, 104, 230.0) });
        }
    }

    // doors and windows
    for (i, o) in f.openings.iter().enumerate() {
        let class = d.classes.get(i).copied().unwrap_or(Class::Unknown);
        let len = if o.kind == OpeningKind::Door { 0.9 } else { 1.2 };
        let (a, b) = if o.horizontal { ((o.x - len / 2.0, o.y), (o.x + len / 2.0, o.y)) } else { ((o.x, o.y - len / 2.0), (o.x, o.y + len / 2.0)) };
        let sel = d.selected_opening == Some(o.id.as_str());
        let w: f32 = if sel { 2.4 } else { 1.2 };
        let is_door = o.kind == OpeningKind::Door;
        let (z0, z1) = if is_door { (0.0, 2.05) } else { (0.9, 2.1) };
        let (conf, name_state) = match class {
            Class::Open(c) => (c, format!("open {:.0}%", c * 100.0)),
            Class::Closed(c) => (c, format!("closed {:.0}%", c * 100.0)),
            Class::Unknown => (0.0, "unknown".to_string()),
        };
        let mid = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
        match (is_door, class) {
            (_, Class::Unknown) => {
                let pts = vec![cx.p(a.0, a.1, z0), cx.p(b.0, b.1, z0), cx.p(b.0, b.1, z1), cx.p(a.0, a.1, z1)];
                l.upright.push((cx.d(mid.0, mid.1, 1.0), Prim::Poly { pts, fill: Color32::TRANSPARENT, stroke: st(w, col(150, 154, 162, cx.a(230.0))), dashed: true }));
            }
            (true, Class::Closed(_)) => {
                prism(cx, l, false, seg_quad(a, b, 0.05), z0, z1, col(70, 56, 48, cx.a(150.0 + 90.0 * conf)), st(w, col(40, 32, 28, cx.a(230.0))), false);
            }
            (true, Class::Open(_)) => {
                let col_d = col(70, 56, 48, cx.a(150.0 + 90.0 * conf));
                let stk = st(w, col(40, 32, 28, cx.a(230.0)));
                let v = (b.0 - a.0, b.1 - a.1);
                let perp = (-v.1, v.0);
                // door leaf swung 90 degrees about the hinge at a, plus posts at both ends
                prism(cx, l, false, seg_quad(a, (a.0 + perp.0, a.1 + perp.1), 0.04), z0, z1, col_d, stk, false);
                for e in [a, b] {
                    prism(cx, l, false, seg_quad((e.0, e.1), (e.0 + 0.001, e.1 + 0.001), 0.06), z0, z1, col_d, stk, false);
                }
                let arc: Vec<Pos2> = (0..=16).map(|k| { let t = k as f32 / 16.0 * std::f32::consts::FRAC_PI_2; cx.p(a.0 + v.0 * t.cos() + perp.0 * t.sin(), a.1 + v.1 * t.cos() + perp.1 * t.sin(), 0.0) }).collect();
                l.ground.push(Prim::Line { pts: arc, stroke: st(1.0, col(40, 32, 28, cx.a(150.0))), dashed: false });
            }
            (false, Class::Closed(_)) | (false, Class::Open(_)) => {
                let open = matches!(class, Class::Open(_));
                let tint = col(90, 150, 210, cx.a(55.0 + 90.0 * conf));
                let stk = st(w, col(60, 110, 165, cx.a(220.0)));
                let lerp = |t: f32| (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
                let spans: Vec<(f32, f32)> = if open { vec![(0.0, 0.32), (0.68, 1.0)] } else { vec![(0.0, 1.0)] };
                for (t0, t1) in spans {
                    let (p0, p1) = (lerp(t0), lerp(t1));
                    let pts = vec![cx.p(p0.0, p0.1, z0), cx.p(p1.0, p1.1, z0), cx.p(p1.0, p1.1, z1), cx.p(p0.0, p0.1, z1)];
                    l.upright.push((cx.d(mid.0, mid.1, 1.5), Prim::Poly { pts, fill: tint, stroke: stk, dashed: false }));
                }
            }
        }
        if labels {
            let nm = o.label.clone().unwrap_or_else(|| if is_door { "door".into() } else { "window".into() });
            l.labels.push(Prim::Text { p: cx.p(mid.0, mid.1, z1 + 0.3), al: Align2::CENTER_BOTTOM, s: format!("{nm}: {name_state}"), sz: 10.0, c: col(80, 84, 94, 235.0) });
        }
    }

    // spots as pins; sound echo rings on the floor
    for rs in &f.room_scans {
        for e in &rs.echoes {
            let c = col(60, 110, 160, cx.a(40.0 + 150.0 * e.confidence));
            let pts: Vec<Pos2> = (0..=72).map(|k| { let a = k as f32 / 72.0 * std::f32::consts::TAU; cx.p(rs.x + e.distance_m * a.cos(), rs.y + e.distance_m * a.sin(), 0.0) }).collect();
            l.ground.push(Prim::Line { pts, stroke: st(1.0, c), dashed: true });
            if labels {
                l.labels.push(Prim::Text { p: cx.p(rs.x + e.distance_m, rs.y, 0.0), al: Align2::LEFT_CENTER, s: format!("{:.1} m", e.distance_m), sz: 9.0, c });
            }
        }
        l.ground.push(Prim::Dot { p: cx.p(rs.x, rs.y, 0.0), r: 2.5, c: col(60, 110, 160, cx.a(230.0)) });
    }
    for s in &f.spots {
        let top = cx.p(s.x, s.y, 0.45);
        l.ground.push(Prim::Dot { p: cx.p(s.x, s.y, 0.0), r: 1.6, c: col(140, 144, 152, cx.a(200.0)) });
        l.upright.push((cx.d(s.x, s.y, 0.2), Prim::Line { pts: vec![cx.p(s.x, s.y, 0.0), top], stroke: st(1.0, col(140, 144, 152, cx.a(220.0))), dashed: false }));
        l.upright.push((cx.d(s.x, s.y, 0.45), Prim::Dot { p: top, r: 2.8, c: col(110, 115, 125, cx.a(240.0)) }));
        if let (Some(t), true) = (&s.label, labels) {
            l.labels.push(Prim::Text { p: top + egui::vec2(5.0, -3.0), al: Align2::LEFT_BOTTOM, s: t.clone(), sz: 10.0, c: col(100, 104, 114, 230.0) });
        }
    }

    // this computer (Wi-Fi fingerprint estimate)
    if let (Some(fix), true) = (d.fix, selected && d.detail) {
        let c = col(40, 130, 90, cx.a(60.0 + 190.0 * fix.confidence));
        let pts: Vec<Pos2> = (0..=48).map(|k| { let a = k as f32 / 48.0 * std::f32::consts::TAU; cx.p(fix.x + fix.radius_m * a.cos(), fix.y + fix.radius_m * a.sin(), 0.0) }).collect();
        l.ground.push(Prim::Line { pts, stroke: st(1.5, c), dashed: true });
        let top = cx.p(fix.x, fix.y, 1.0);
        l.upright.push((cx.d(fix.x, fix.y, 0.5), Prim::Line { pts: vec![cx.p(fix.x, fix.y, 0.0), top], stroke: st(1.5, c), dashed: false }));
        l.upright.push((cx.d(fix.x, fix.y, 1.0), Prim::Dot { p: top, r: 4.0, c }));
        if labels {
            let place = fix.nearest_label.clone().map(|t| format!(" near {t}")).unwrap_or_default();
            l.labels.push(Prim::Text { p: cx.p(fix.x, fix.y + fix.radius_m, 0.0) + egui::vec2(0.0, 6.0), al: Align2::CENTER_TOP, s: format!("this computer (est.){place}, {:.0}%", fix.confidence * 100.0), sz: 10.0, c });
        }
    }

    // this computer, as placed by the user: a small laptop
    if let Some((px, py)) = d.computer {
        let base = [(-0.17f32, -0.12f32), (0.17, -0.12), (0.17, 0.12), (-0.17, 0.12)].map(|(dx, dy)| (px + dx, py + dy));
        let screen = [(-0.17f32, -0.12f32), (0.17, -0.12), (0.17, -0.10), (-0.17, -0.10)].map(|(dx, dy)| (px + dx, py + dy));
        prism(cx, l, false, base, 0.0, 0.03, col(70, 78, 92, cx.a(255.0)), st(0.7, col(30, 34, 40, cx.a(255.0))), false);
        prism(cx, l, false, screen, 0.03, 0.25, col(50, 56, 68, cx.a(255.0)), st(0.7, col(30, 34, 40, cx.a(255.0))), false);
        if labels {
            l.labels.push(Prim::Text { p: cx.p(px, py, 0.5), al: Align2::CENTER_BOTTOM, s: "this computer".into(), sz: 10.0, c: col(60, 66, 80, 240.0) });
        }
    }

    // access points whose position was estimated from the mapping walk
    for a in d.aps {
        let c = col(80, 110, 160, cx.a(210.0));
        let ring: Vec<Pos2> = (0..=36).map(|k| { let t = k as f32 / 36.0 * std::f32::consts::TAU; cx.p(a.x + a.radius_m * t.cos(), a.y + a.radius_m * t.sin(), 0.0) }).collect();
        l.ground.push(Prim::Line { pts: ring, stroke: st(1.0, col(80, 110, 160, cx.a(110.0))), dashed: true });
        let top = cx.p(a.x, a.y, 2.0);
        l.upright.push((cx.d(a.x, a.y, 1.0), Prim::Line { pts: vec![cx.p(a.x, a.y, 0.0), top], stroke: st(1.0, c), dashed: false }));
        l.upright.push((cx.d(a.x, a.y, 2.0), Prim::Dot { p: top, r: 4.0, c }));
        if labels {
            l.labels.push(Prim::Text { p: top + egui::vec2(6.0, -2.0), al: Align2::LEFT_CENTER, s: a.ssid.clone().unwrap_or_else(|| "access point".into()) + " (est.)", sz: 9.0, c: col(70, 95, 140, 235.0) });
        }
    }

    // movement: a grainy 3D cloud that follows the probability map; as tall as what it is thought to be
    if let (Some(b), true) = (&d.blob, selected && d.detail) {
        let pmax = b.cells.iter().map(|c| c.2).fold(0.0f32, f32::max).max(1e-6);
        let shadow_r = b.radius_m.clamp(0.4, 4.0);
        let shadow: Vec<Pos2> = (0..24).map(|k| { let a = k as f32 / 24.0 * std::f32::consts::TAU; cx.p(b.x + shadow_r * 0.9 * a.cos(), b.y + shadow_r * 0.9 * a.sin(), 0.0) }).collect();
        l.ground.push(Prim::Poly { pts: shadow, fill: col(PRESENCE.0, PRESENCE.1, PRESENCE.2, 18.0), stroke: Stroke::NONE, dashed: false });
        for (ci, (x, y, m)) in b.cells.iter().enumerate() {
            let n = ((m / pmax) * 12.0).ceil() as i32;
            for g in 0..n {
                let (jx, jy, jz) = (hash3(ci as i32, g, 1), hash3(ci as i32, g, 2), hash3(ci as i32, g, 3));
                let (gx, gy, gz) = (x + (jx - 0.5) * 0.5, y + (jy - 0.5) * 0.5, jz * b.height_m);
                l.upright.push((cx.d(gx, gy, gz), Prim::Dot { p: cx.p(gx, gy, gz), r: 1.6, c: col(PRESENCE.0, PRESENCE.1, PRESENCE.2, 150.0 + 70.0 * (m / pmax)) }));
            }
        }
        let t = cx.p(b.x, b.y, b.height_m + 0.5);
        let breath = b.breathing.map_or("breathing: --".to_string(), |(v, cf)| format!("breathing: {v:.0}/min ({:.0}%)", cf * 100.0));
        let heart = b.heart.map_or("heart: --".to_string(), |(v, cf)| format!("heart: {v:.0} bpm, experimental ({:.0}%)", cf * 100.0));
        let g = col(80, 84, 94, 235.0);
        let place = if b.ring.is_some() { format!("location: ring at the measured distance only ({})", b.sources) } else { format!("location from {} ({:.0}% concentrated)", b.sources, b.confidence * 100.0) };
        l.labels.push(Prim::Text { p: t, al: Align2::CENTER_BOTTOM, s: place, sz: 9.0, c: col(110, 114, 124, 235.0) });
        l.labels.push(Prim::Text { p: t + egui::vec2(0.0, -11.0), al: Align2::CENTER_BOTTOM, s: heart, sz: 10.0, c: g });
        l.labels.push(Prim::Text { p: t + egui::vec2(0.0, -23.0), al: Align2::CENTER_BOTTOM, s: breath, sz: 10.0, c: g });
        l.labels.push(Prim::Text { p: t + egui::vec2(0.0, -35.0), al: Align2::CENTER_BOTTOM, s: format!("{} ({})", b.kind, b.who), sz: 10.0, c: g });
    }
}

/// Draw the floors (one, or all stacked from bottom to top). `floors` must be ordered bottom to top.
/// Returns the projection used, for picking.
pub fn render(painter: &Painter, rect: Rect, cam: &Camera, floors: &[FloorItem], stacked: bool, selected: usize, reg: Region) -> Proj {
    let painter = painter.with_clip_rect(rect);
    let proj = make_proj(cam, rect, floors.len(), stacked, reg);
    let mut all_labels: Vec<Prim> = vec![];
    // ghost floors first, the selected floor last so it is never washed out
    let mut order: Vec<usize> = (0..floors.len()).filter(|r| *r != selected).collect();
    order.push(selected.min(floors.len().saturating_sub(1)));
    for rank in order {
        let fl = &floors[rank];
        let z0 = if stacked { rank as f32 * FLOOR_GAP } else { 0.0 };
        let sel = rank == selected || !stacked;
        let cx = Ctx { pr: &proj, z0, fade: if sel { 1.0 } else { 0.4 }, cutaway: cam.cutaway };
        let mut l = Layers { ground: vec![], upright: vec![], labels: vec![] };

        // floor plate: the drawing canvas with a 1 m grid (not a house outline)
        let corners = [(reg.0, reg.2), (reg.1, reg.2), (reg.1, reg.3), (reg.0, reg.3)];
        if sel {
            prism(&cx, &mut l, true, corners, -0.18, 0.0, col(244, 245, 248, 255.0), st(1.0, col(190, 194, 202, 255.0)), false);
            let gc = st(1.0, col(226, 229, 234, 255.0));
            for i in (reg.0.ceil() as i32)..=(reg.1.floor() as i32) {
                l.ground.push(Prim::Line { pts: vec![cx.p(i as f32, reg.2, 0.0), cx.p(i as f32, reg.3, 0.0)], stroke: gc, dashed: false });
            }
            for j in (reg.2.ceil() as i32)..=(reg.3.floor() as i32) {
                l.ground.push(Prim::Line { pts: vec![cx.p(reg.0, j as f32, 0.0), cx.p(reg.1, j as f32, 0.0)], stroke: gc, dashed: false });
            }
        } else {
            // ghost floor: faint tinted slab with a clear outline
            prism(&cx, &mut l, true, corners, -0.18, 0.0, col(150, 160, 178, 38.0), st(1.2, col(140, 146, 160, 210.0)), false);
        }
        floor_content(&cx, &fl.data, sel, &mut l);
        for pr in &l.ground {
            draw(&painter, pr);
        }
        l.upright.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (_, pr) in &l.upright {
            draw(&painter, pr);
        }
        all_labels.extend(l.labels);
        all_labels.push(Prim::Text { p: cx.p(reg.0, reg.3, 0.0) + egui::vec2(-10.0, 0.0), al: Align2::RIGHT_CENTER, s: fl.name.to_string(), sz: 13.0, c: if sel { col(50, 54, 64, 255.0) } else { col(110, 116, 128, 255.0) } });
        if stacked && rank + 1 < floors.len() {
            for (x, y) in corners {
                painter.add(Shape::dashed_line(&[cx.p(x, y, 0.0), cx.p(x, y, FLOOR_GAP)], st(1.0, col(200, 204, 212, 255.0)), 3.0, 3.0));
            }
        }
    }
    for pr in &all_labels {
        draw(&painter, pr);
    }
    proj
}

/// Which floor (rank in the drawn list) a click on a stacked view lands on: the topmost plate under the pointer.
pub fn hit_floor(proj: &Proj, n: usize, pos: Pos2, reg: Region) -> Option<usize> {
    (0..n).rev().find(|rank| {
        let (x, y) = proj.unproject(pos, *rank as f32 * FLOOR_GAP);
        (reg.0..=reg.1).contains(&x) && (reg.2..=reg.3).contains(&y)
    })
}
