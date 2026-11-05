// SPDX-License-Identifier: AGPL-3.0-or-later
//! Walk-around mapping: signal barriers, room fingerprinting, floor-to-floor loss.

use crate::{median, wifi::rms_distance};
use storage::{ApStat, Floor};

#[derive(Debug, Clone)]
pub struct Barrier {
    /// End points of the drawn zone (metres, floor grid).
    pub a: (f32, f32),
    pub b: (f32, f32),
    pub confidence: f32,
    pub median_drop_db: f32,
    pub shared_aps: usize,
}

const MIN_SHARED: usize = 3;
const MIN_PAIRS: usize = 6;
const MIN_DROP_DB: f32 = 4.0;
const Z_THRESHOLD: f32 = 2.5;

/// Find neighbouring spot pairs whose signal changes much more per metre than is typical *in this house*.
/// Returns `Err(reason)` when there are too few measurements to say anything.
pub fn detect_barriers(f: &Floor) -> Result<Vec<Barrier>, String> {
    struct P {
        i: usize,
        j: usize,
        drop: f32,
        rate: f32,
        shared: usize,
    }
    let mut pairs: Vec<P> = vec![];
    for i in 0..f.spots.len() {
        for j in (i + 1)..f.spots.len() {
            let (a, b) = (&f.spots[i], &f.spots[j]);
            let d = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
            if !(0.5..=4.0).contains(&d) {
                continue;
            }
            let mut diffs: Vec<f32> = a.aps.iter().filter_map(|x| b.aps.iter().find(|y| y.bssid == x.bssid).map(|y| (x.mean_dbm - y.mean_dbm).abs())).collect();
            if diffs.len() < MIN_SHARED {
                continue;
            }
            let shared = diffs.len();
            let drop = median(&mut diffs).unwrap();
            pairs.push(P { i, j, drop, rate: drop / d, shared });
        }
    }
    if pairs.len() < MIN_PAIRS {
        return Err(format!("need at least {MIN_PAIRS} neighbouring spot pairs (0.5-4 m apart, sharing {MIN_SHARED}+ access points); have {}", pairs.len()));
    }
    let mut rates: Vec<f32> = pairs.iter().map(|p| p.rate).collect();
    let m = median(&mut rates).unwrap();
    let mut dev: Vec<f32> = pairs.iter().map(|p| (p.rate - m).abs()).collect();
    let mad = median(&mut dev).unwrap();
    let scale = 1.4826 * mad.max(0.25);
    let mut out = vec![];
    for p in &pairs {
        let z = (p.rate - m) / scale;
        if z >= Z_THRESHOLD && p.drop >= MIN_DROP_DB {
            let (a, b) = (&f.spots[p.i], &f.spots[p.j]);
            let (mx, my) = ((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
            let (dx, dy) = (b.x - a.x, b.y - a.y);
            let len = (dx * dx + dy * dy).sqrt();
            let (px, py) = (-dy / len, dx / len);
            let half = len.max(1.0) / 2.0;
            let conf = (1.0 - (-(z - Z_THRESHOLD) / 2.0).exp()).clamp(0.05, 1.0) * (p.shared as f32 / 6.0).min(1.0);
            out.push(Barrier { a: (mx - px * half, my - py * half), b: (mx + px * half, my + py * half), confidence: conf, median_drop_db: p.drop, shared_aps: p.shared });
        }
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct Fix {
    pub floor_index: usize,
    pub floor_confidence: Option<f32>,
    pub x: f32,
    pub y: f32,
    pub radius_m: f32,
    pub confidence: f32,
    pub nearest_label: Option<String>,
}

/// Fingerprint the current Wi-Fi vector against all mapped spots (shared access points only; no gap filling).
pub fn locate(current: &[ApStat], floors: &[Floor]) -> Option<Fix> {
    struct C {
        fl: usize,
        d: f32,
        x: f32,
        y: f32,
        label: Option<String>,
    }
    let mut cands: Vec<C> = vec![];
    let mut all_spots: Vec<&storage::Spot> = vec![];
    for (fi, f) in floors.iter().enumerate() {
        for s in &f.spots {
            all_spots.push(s);
            if let Some(d) = rms_distance(current, &s.aps, MIN_SHARED) {
                cands.push(C { fl: fi, d, x: s.x, y: s.y, label: s.label.clone() });
            }
        }
    }
    if all_spots.len() < 3 || cands.is_empty() {
        return None;
    }
    cands.sort_by(|a, b| a.d.total_cmp(&b.d));
    // typical fingerprint distance between mapped spots (data-derived scale)
    let mut pd: Vec<f32> = vec![];
    for i in 0..all_spots.len() {
        for j in (i + 1)..all_spots.len() {
            if let Some(d) = rms_distance(&all_spots[i].aps, &all_spots[j].aps, MIN_SHARED) {
                pd.push(d);
            }
        }
    }
    let d_typ = median(&mut pd)?;
    let best_floor = cands[0].fl;
    let top: Vec<&C> = cands.iter().filter(|c| c.fl == best_floor).take(3).collect();
    let wsum: f32 = top.iter().map(|c| 1.0 / (c.d + 1.0)).sum();
    let x = top.iter().map(|c| c.x / (c.d + 1.0)).sum::<f32>() / wsum;
    let y = top.iter().map(|c| c.y / (c.d + 1.0)).sum::<f32>() / wsum;
    let spread = (top.iter().map(|c| ((c.x - x).powi(2) + (c.y - y).powi(2)) / (c.d + 1.0)).sum::<f32>() / wsum).sqrt();
    let mut spacing: Vec<f32> = vec![];
    for (i, a) in all_spots.iter().enumerate() {
        let nn = all_spots.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, b)| ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()).fold(f32::MAX, f32::min);
        spacing.push(nn);
    }
    let radius = spread.max(median(&mut spacing).unwrap_or(1.0));
    let confidence = (1.0 - cands[0].d / d_typ).clamp(0.0, 1.0);
    let other = cands.iter().find(|c| c.fl != best_floor).map(|c| c.d);
    let floor_confidence = other.map(|o| (1.0 - cands[0].d / o).clamp(0.0, 1.0));
    Some(Fix { floor_index: best_floor, floor_confidence, x, y, radius_m: radius, confidence, nearest_label: cands[0].label.clone() })
}

/// Median RSSI difference (floor A minus floor B) over APs both floors observed, using each floor's mean per AP.
pub fn floor_loss_db(a: &Floor, b: &Floor) -> Option<(f32, usize)> {
    let avg = |f: &Floor| -> std::collections::HashMap<String, f32> {
        let mut acc: std::collections::HashMap<String, (f32, u32)> = Default::default();
        for s in &f.spots {
            for ap in &s.aps {
                let e = acc.entry(ap.bssid.clone()).or_default();
                e.0 += ap.mean_dbm;
                e.1 += 1;
            }
        }
        acc.into_iter().map(|(k, (s, n))| (k, s / n as f32)).collect()
    };
    let (ma, mb) = (avg(a), avg(b));
    let mut d: Vec<f32> = ma.iter().filter_map(|(k, v)| mb.get(k).map(|w| v - w)).collect();
    let n = d.len();
    if n < MIN_SHARED { None } else { Some((median(&mut d)?, n)) }
}
