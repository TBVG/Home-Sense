// SPDX-License-Identifier: AGPL-3.0-or-later
//! Rolling store of real OS measurements, de-duplicated so cached results are not counted twice.

use crate::mean_std;
use scanner::Observation;
use std::collections::{HashMap, VecDeque};
use storage::ApStat;

const KEEP_MS: u64 = 40 * 60 * 1000;
/// A reading whose OS timestamp is within this of the previous one for the same AP is the same cached measurement.
const SAME_MEASUREMENT_MS: u64 = 1500;

#[derive(Default)]
pub struct ApSeries {
    pub ssid: Option<String>,
    /// (measurement time in unix ms, RSSI dBm), oldest first.
    pub samples: VecDeque<(u64, f32)>,
}

#[derive(Default)]
pub struct LiveStore {
    pub aps: HashMap<String, ApSeries>,
    /// Number of scans received (used to know when a fresh scan arrived).
    pub scans: u64,
}

impl LiveStore {
    pub fn push(&mut self, obs: &[Observation]) {
        self.scans += 1;
        for o in obs {
            let (Some(stamp), Some(rssi)) = (o.stamp_ms, o.rssi_dbm) else { continue };
            let s = self.aps.entry(o.bssid.clone()).or_default();
            if o.ssid.is_some() {
                s.ssid = o.ssid.clone();
            }
            let fresh = s.samples.back().map_or(true, |(t, _)| stamp > t + SAME_MEASUREMENT_MS);
            if fresh {
                s.samples.push_back((stamp, rssi as f32));
            }
            while s.samples.front().map_or(false, |(t, _)| *t + KEEP_MS < stamp) {
                s.samples.pop_front();
            }
        }
    }

    pub fn clear(&mut self) {
        self.aps.clear();
        self.scans = 0;
    }

    /// Per-AP statistics over measurements with `from_ms <= t <= to_ms`.
    pub fn stats_between(&self, from_ms: u64, to_ms: u64) -> Vec<ApStat> {
        let mut out = vec![];
        for (bssid, s) in &self.aps {
            let v: Vec<f32> = s.samples.iter().filter(|(t, _)| *t >= from_ms && *t <= to_ms).map(|(_, r)| *r).collect();
            if let Some((m, sd)) = mean_std(&v) {
                out.push(ApStat { bssid: bssid.clone(), ssid: s.ssid.clone(), mean_dbm: m, std_dbm: if v.len() >= 2 { sd } else { 0.0 }, n: v.len() as u32 });
            }
        }
        out.sort_by(|a, b| b.mean_dbm.total_cmp(&a.mean_dbm));
        out
    }
}

/// Shared-AP RMS difference in dB between two sets. `None` unless at least `min_shared` APs are in both.
pub fn rms_distance(a: &[ApStat], b: &[ApStat], min_shared: usize) -> Option<f32> {
    let mut sum = 0.0;
    let mut n = 0;
    for x in a {
        if let Some(y) = b.iter().find(|y| y.bssid == x.bssid) {
            sum += (x.mean_dbm - y.mean_dbm).powi(2);
            n += 1;
        }
    }
    if n >= min_shared { Some((sum / n as f32).sqrt()) } else { None }
}
