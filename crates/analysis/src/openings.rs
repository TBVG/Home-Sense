// SPDX-License-Identifier: AGPL-3.0-or-later
//! Door/window state from trained Wi-Fi patterns. Three outputs: Open, Closed, Unknown.

use crate::mean_std;
use storage::{ApStat, OpenState, Opening};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Class {
    Open(f32),
    Closed(f32),
    Unknown,
}

pub const MIN_SEGMENTS_PER_STATE: usize = 2;
pub const MIN_CONFIDENCE: f32 = 0.4;
const MIN_SEP_DB: f32 = 2.0;
const MIN_DPRIME: f32 = 2.0;
const RSSI_QUANT_DB: f32 = 1.0;

struct Informative {
    bssid: String,
    open: f32,
    closed: f32,
}

fn informative(o: &Opening) -> Vec<Informative> {
    let seg = |st: OpenState| o.training.iter().filter(|t| t.state == st).collect::<Vec<_>>();
    let (so, sc) = (seg(OpenState::Open), seg(OpenState::Closed));
    if so.len() < MIN_SEGMENTS_PER_STATE || sc.len() < MIN_SEGMENTS_PER_STATE {
        return vec![];
    }
    let mut out = vec![];
    for ap in &so[0].aps {
        let col = |segs: &Vec<&storage::TrainingSegment>| -> Option<Vec<f32>> {
            segs.iter().map(|s| s.aps.iter().find(|a| a.bssid == ap.bssid).map(|a| a.mean_dbm)).collect()
        };
        let (Some(vo), Some(vc)) = (col(&so), col(&sc)) else { continue };
        let (mo, sdo) = mean_std(&vo).unwrap();
        let (mc, sdc) = mean_std(&vc).unwrap();
        let noise = ((sdo + sdc) / 2.0).max(RSSI_QUANT_DB);
        let sep = (mo - mc).abs();
        if sep >= MIN_SEP_DB && sep / noise >= MIN_DPRIME {
            out.push(Informative { bssid: ap.bssid.clone(), open: mo, closed: mc });
        }
    }
    out
}

/// Why this opening cannot be classified (for the UI), or None if it can.
pub fn untrainable_reason(o: &Opening) -> Option<String> {
    let n = |st| o.training.iter().filter(|t| t.state == st).count();
    if n(OpenState::Open) < MIN_SEGMENTS_PER_STATE || n(OpenState::Closed) < MIN_SEGMENTS_PER_STATE {
        return Some(format!("needs {MIN_SEGMENTS_PER_STATE}+ open and {MIN_SEGMENTS_PER_STATE}+ closed recordings (has {} open, {} closed)", n(OpenState::Open), n(OpenState::Closed)));
    }
    if informative(o).is_empty() {
        return Some("open and closed look the same in the Wi-Fi signals from this spot; no access point separates them".into());
    }
    None
}

pub fn classify(o: &Opening, current: &[ApStat]) -> Class {
    let inf = informative(o);
    let mut d_open = 0.0;
    let mut d_closed = 0.0;
    let mut n = 0;
    for i in &inf {
        if let Some(c) = current.iter().find(|c| c.bssid == i.bssid) {
            d_open += (c.mean_dbm - i.open).powi(2);
            d_closed += (c.mean_dbm - i.closed).powi(2);
            n += 1;
        }
    }
    if n == 0 {
        return Class::Unknown;
    }
    let (d_open, d_closed) = ((d_open / n as f32).sqrt(), (d_closed / n as f32).sqrt());
    let conf = (d_open - d_closed).abs() / (d_open + d_closed).max(1e-6);
    if conf < MIN_CONFIDENCE {
        Class::Unknown
    } else if d_open < d_closed {
        Class::Open(conf)
    } else {
        Class::Closed(conf)
    }
}
