// SPDX-License-Identifier: AGPL-3.0-or-later
//! Estimate where each access point is on the map from the mapping walk.
//!
//! For one access point the signal at every spot follows rssi = a - 10 n log10(distance). A search over
//! candidate positions finds the one where that curve fits the readings best. Walls add errors, so each fit
//! carries its residual error and an uncertainty radius, and poor fits (other floors, strong obstruction)
//! are rejected. Nothing here is guessed: with too few spots or a bad fit, no position is returned.

use std::collections::HashMap;
use storage::Spot;

#[derive(Debug, Clone)]
pub struct ApFit {
    pub bssid: String,
    pub ssid: Option<String>,
    pub x: f32,
    pub y: f32,
    pub exponent: f32,
    pub rmse_db: f32,
    /// How far from the best position other positions still fit nearly as well (metres).
    pub radius_m: f32,
    pub spots: usize,
    /// Positions that fit nearly as well as the best one (at most about 40). A router that is well localised has a
    /// tight set; a neighbour's router outside the flat has a long streak pointing in its direction.
    pub cands: Vec<(f32, f32)>,
}

const MIN_SPOTS: usize = 8;
const MAX_RMSE_DB: f32 = 5.0;

fn cost_at(pts: &[(f32, f32, f32)], px: f32, py: f32) -> Option<(f32, f32)> {
    let n = pts.len() as f32;
    let (mut su, mut sr, mut suu, mut sur) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for (x, y, r) in pts {
        let u = ((x - px).powi(2) + (y - py).powi(2)).sqrt().max(1.0).log10();
        su += u;
        sr += r;
        suu += u * u;
        sur += u * r;
    }
    let var = suu / n - (su / n).powi(2);
    if var < 1e-4 {
        return None;
    }
    let b = (sur / n - su / n * sr / n) / var;
    if !(-45.0..=-12.0).contains(&b) {
        return None; // path-loss exponent outside 1.2 .. 4.5
    }
    let a = sr / n - b * su / n;
    let cost = pts.iter().map(|(x, y, r)| {
        let u = ((x - px).powi(2) + (y - py).powi(2)).sqrt().max(1.0).log10();
        (r - a - b * u).powi(2)
    }).sum::<f32>();
    Some((cost, b))
}

fn fit_one(pts: &[(f32, f32, f32)]) -> Option<(f32, f32, f32, f32, f32, Vec<(f32, f32)>)> {
    let (mut x0, mut x1, mut y0, mut y1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for (x, y, _) in pts {
        x0 = x0.min(*x);
        x1 = x1.max(*x);
        y0 = y0.min(*y);
        y1 = y1.max(*y);
    }
    let step = 0.5f32;
    let mut cands: Vec<(f32, f32, f32)> = vec![];
    let mut best: Option<(f32, f32, f32, f32)> = None; // cost, x, y, b
    let mut px = x0 - 10.0;
    while px <= x1 + 10.0 {
        let mut py = y0 - 10.0;
        while py <= y1 + 10.0 {
            if let Some((c, b)) = cost_at(pts, px, py) {
                cands.push((c, px, py));
                if best.map_or(true, |bb| c < bb.0) {
                    best = Some((c, px, py, b));
                }
            }
            py += step;
        }
        px += step;
    }
    let (mut bc, mut bx, mut by, mut bb) = best?;
    let coarse_min = bc;
    for i in -5..=5 {
        for j in -5..=5 {
            let (qx, qy) = (bx + i as f32 * 0.1, by + j as f32 * 0.1);
            if let Some((c, b)) = cost_at(pts, qx, qy) {
                if c < bc {
                    bc = c;
                    bb = b;
                    bx = qx;
                    by = qy;
                }
            }
        }
    }
    let n = pts.len() as f32;
    let limit = coarse_min * 1.2 + n * 0.25; // within about 0.5 dB rms of the best
    let radius = cands.iter().filter(|c| c.0 <= limit).map(|c| ((c.1 - bx).powi(2) + (c.2 - by).powi(2)).sqrt()).fold(0.5f32, f32::max);
    let rmse = (bc / n).sqrt();
    if rmse > MAX_RMSE_DB {
        return None;
    }
    let accepted: Vec<(f32, f32)> = cands.iter().filter(|c| c.0 <= limit).map(|c| (c.1, c.2)).collect();
    let stride = (accepted.len() / 40).max(1);
    let mut set: Vec<(f32, f32)> = accepted.iter().step_by(stride).copied().collect();
    set.push((bx, by));
    Some((bx, by, -bb / 10.0, rmse, radius, set))
}

pub fn fit_aps(spots: &[Spot]) -> Vec<ApFit> {
    let mut by: HashMap<&str, (Option<String>, Vec<(f32, f32, f32)>)> = HashMap::new();
    for s in spots {
        for ap in &s.aps {
            let e = by.entry(ap.bssid.as_str()).or_default();
            if ap.ssid.is_some() {
                e.0 = ap.ssid.clone();
            }
            e.1.push((s.x, s.y, ap.mean_dbm));
        }
    }
    let mut out = vec![];
    for (bssid, (ssid, pts)) in by {
        if pts.len() < MIN_SPOTS {
            continue;
        }
        if let Some((x, y, exponent, rmse_db, radius_m, cands)) = fit_one(&pts) {
            out.push(ApFit { bssid: bssid.to_string(), ssid, x, y, exponent, rmse_db, radius_m, spots: pts.len(), cands });
        }
    }
    out.sort_by(|a, b| a.bssid.cmp(&b.bssid));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MATH CHECK (exact formula, no noise, never reaches the UI): readings generated from rssi = -40 - 20 log10(d)
    /// for an access point at (10, 5) are fitted back to that position and exponent 2.
    #[test]
    fn math_check_recovers_exact_ap_position() {
        let mut pts = vec![];
        for xi in 0..7 {
            for yi in 0..4 {
                let (x, y) = (4.0 + 2.0 * xi as f32, 4.0 + 2.0 * yi as f32);
                let d = ((x - 10.0).powi(2) + (y - 5.0).powi(2)).sqrt().max(1.0);
                pts.push((x, y, -40.0 - 20.0 * d.log10()));
            }
        }
        let (x, y, n, rmse, _, _) = fit_one(&pts).expect("fit");
        assert!((x - 10.0).abs() < 0.3 && (y - 5.0).abs() < 0.3, "pos {x},{y}");
        assert!((n - 2.0).abs() < 0.1, "n {n}");
        assert!(rmse < 0.3, "rmse {rmse}");
    }
}
