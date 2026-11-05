// SPDX-License-Identifier: AGPL-3.0-or-later
//! Stationary monitoring from live Wi-Fi: house-wide movement and lasting "something changed" shifts.
//! Needs history before it says anything; until then it reports why it is unknown.

use crate::{mean_std, median, wifi::LiveStore};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Movement {
    Unknown(String),
    NoUnusualVariation(f32),
    Moving(f32),
}

#[derive(Debug, Clone)]
pub struct ChangeEvent {
    pub at_ms: u64,
    pub aps: usize,
    pub median_shift_db: f32,
}

const RECENT_MS: u64 = 60_000;
const BASE_MS: u64 = 30 * 60_000;
const QUANT_DB: f32 = 1.0;

fn abs_diffs(s: &[(u64, f32)]) -> Vec<f32> {
    s.windows(2).map(|w| (w[1].1 - w[0].1).abs()).collect()
}

/// A network is only a useful link if it is heard well enough most of the time.
const MIN_USEFUL_RSSI_DBM: f32 = -88.0;
/// Fewest usable networks needed to judge movement. Two is enough for a flat with a single router (2.4 and 5 GHz).
const MIN_VOTERS: usize = 2;

/// Link quality 0.1 (barely audible) to 1.0 (strong): strong links count for more than weak flickery ones.
fn quality(mean_dbm: f32) -> f32 {
    ((mean_dbm + 90.0) / 40.0).clamp(0.1, 1.0)
}

/// House-wide movement from the Wi-Fi signals.
///
/// Votes are weighted by link quality, so a strong router outweighs a dozen weak neighbours, which matters in a flat
/// where there is only one router of your own. At least two networks must be disturbed, never just one, to avoid
/// reacting to a single network glitch.
pub fn movement(store: &LiveStore, now_ms: u64) -> Movement {
    let (mut flagged, mut voters, mut q_flagged, mut q_total) = (0, 0, 0.0f32, 0.0f32);
    for s in store.aps.values() {
        let recent: Vec<(u64, f32)> = s.samples.iter().copied().filter(|(t, _)| *t + RECENT_MS >= now_ms).collect();
        let base: Vec<(u64, f32)> = s.samples.iter().copied().filter(|(t, _)| *t + RECENT_MS < now_ms && *t + BASE_MS >= now_ms).collect();
        let (dr, mut db) = (abs_diffs(&recent), abs_diffs(&base));
        if dr.len() < 2 || db.len() < 8 {
            continue;
        }
        let mean = base.iter().map(|(_, r)| *r).sum::<f32>() / base.len() as f32;
        if mean < MIN_USEFUL_RSSI_DBM {
            continue;
        }
        let a_recent = dr.iter().sum::<f32>() / dr.len() as f32;
        let a_base = median(&mut db).unwrap().max(QUANT_DB);
        let q = quality(mean);
        voters += 1;
        q_total += q;
        if a_recent >= 2.0 * a_base && a_recent >= 2.0 {
            flagged += 1;
            q_flagged += q;
        }
    }
    if voters < MIN_VOTERS || q_total < 1.0 {
        return Movement::Unknown(format!("learning this home's normal signal behaviour ({voters} usable networks have enough history, need {MIN_VOTERS})"));
    }
    let frac = q_flagged / q_total;
    if flagged >= 2 && frac >= 0.4 { Movement::Moving(frac) } else { Movement::NoUnusualVariation(1.0 - frac) }
}

#[derive(Default)]
pub struct ChangeDetector {
    since_ms: Option<u64>,
    fired: bool,
    clear_since: Option<u64>,
}

impl ChangeDetector {
    pub fn update(&mut self, store: &LiveStore, now_ms: u64) -> Option<ChangeEvent> {
        let mut shifts = vec![];
        for s in store.aps.values() {
            let recent: Vec<f32> = s.samples.iter().filter(|(t, _)| *t + RECENT_MS >= now_ms).map(|(_, r)| *r).collect();
            let prev: Vec<f32> = s.samples.iter().filter(|(t, _)| *t + RECENT_MS < now_ms && *t + 11 * 60_000 >= now_ms).map(|(_, r)| *r).collect();
            if recent.len() < 2 || prev.len() < 5 {
                continue;
            }
            let (mr, _) = mean_std(&recent).unwrap();
            let (mp, sp) = mean_std(&prev).unwrap();
            let shift = mr - mp;
            if shift.abs() >= 3.0f32.max(3.0 * sp / (recent.len() as f32).sqrt()) {
                shifts.push(shift);
            }
        }
        let active = shifts.len() >= 2;
        if active {
            self.clear_since = None;
            let since = *self.since_ms.get_or_insert(now_ms);
            if !self.fired && now_ms >= since + 90_000 {
                self.fired = true;
                let n = shifts.len();
                return Some(ChangeEvent { at_ms: since, aps: n, median_shift_db: median(&mut shifts).unwrap() });
            }
        } else {
            let c = *self.clear_since.get_or_insert(now_ms);
            if now_ms >= c + 120_000 {
                self.since_ms = None;
                self.fired = false;
            }
        }
        None
    }
}

/// For every access point: how strongly its signal is being disturbed right now (0 = normal, 1 = clearly disturbed).
/// Only access points with enough history appear. A link must be wobbling at least 1.5 dB per scan to count at all.
pub fn link_weights(store: &LiveStore, now_ms: u64) -> HashMap<String, f32> {
    let mut out = HashMap::new();
    for (bssid, s) in &store.aps {
        let recent: Vec<(u64, f32)> = s.samples.iter().copied().filter(|(t, _)| *t + RECENT_MS >= now_ms).collect();
        let base: Vec<(u64, f32)> = s.samples.iter().copied().filter(|(t, _)| *t + RECENT_MS < now_ms && *t + BASE_MS >= now_ms).collect();
        let (dr, mut db) = (abs_diffs(&recent), abs_diffs(&base));
        if dr.len() < 2 || db.len() < 8 {
            continue;
        }
        let mean = base.iter().map(|(_, r)| *r).sum::<f32>() / base.len() as f32;
        if mean < MIN_USEFUL_RSSI_DBM {
            continue;
        }
        let a_recent = dr.iter().sum::<f32>() / dr.len() as f32;
        let a_base = median(&mut db).unwrap().max(QUANT_DB);
        let w = if a_recent < 1.5 { 0.0 } else { ((a_recent / a_base - 1.0) / 2.0).clamp(0.0, 1.0) };
        out.insert(bssid.clone(), w);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use scanner::Observation;

    fn obs(bssid: &str, rssi: i32, stamp: u64) -> Observation {
        Observation { bssid: bssid.into(), ssid: None, rssi_dbm: Some(rssi), frequency_mhz: None, channel: None, age_ms: None, stamp_ms: Some(stamp), read_at: std::time::SystemTime::now() }
    }

    /// Hand-made series: 20 older scans where the signal alternates by 1 dB, then 4 recent scans alternating by 6 dB.
    fn store_with(bssids: &[(&str, i32)], disturbed: &[&str]) -> (LiveStore, u64) {
        let mut st = LiveStore::default();
        let base = 1_000_000_000u64;
        for k in 0..24u64 {
            let stamp = base + k * 10_000;
            let recent = k >= 20;
            for (b, level) in bssids {
                let swing = if recent && disturbed.contains(b) { 6 } else { 1 };
                st.push(&[obs(b, level + if k % 2 == 0 { 0 } else { swing }, stamp)]);
            }
        }
        (st, base + 23 * 10_000 + 1000)
    }

    /// LOGIC CHECK (hand-made series, never reaches the UI): a flat with one router (two bands) and weak far networks.
    /// Both router bands disturbed counts as movement; only one disturbed does not; with a single network it stays unknown.
    #[test]
    fn logic_check_single_router_flat() {
        let nets = [("r24", -45), ("r5", -50), ("n1", -85), ("n2", -86)];
        let (st, now) = store_with(&nets, &["r24", "r5"]);
        assert!(matches!(movement(&st, now), Movement::Moving(_)));
        let (st, now) = store_with(&nets, &["r24"]);
        assert!(matches!(movement(&st, now), Movement::NoUnusualVariation(_)));
        let (st, now) = store_with(&[("r24", -45)], &["r24"]);
        assert!(matches!(movement(&st, now), Movement::Unknown(_)));
    }
}
