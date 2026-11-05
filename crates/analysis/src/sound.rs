// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sound analysis from ultrasonic echo profiles: wall echoes, motion, breathing, heart rate.
//!
//! The detection thresholds are conservative design choices that have NOT been validated
//! against recordings from real people and rooms. When quality gates are not met the
//! outputs are "unknown"/None rather than a guess.

use crate::median;
use rustfft::{num_complex::Complex32, FftPlanner};
use std::collections::VecDeque;
use storage::Echo;

const MIN_WALL_M: f32 = 0.3; // closer than this is the laptop itself / table surface
const MAX_WALL_M: f32 = 8.0;
const POOL: usize = 4;

fn pooled(profile: &[f32], m_per_bin: f32, lo_m: f32, hi_m: f32) -> Vec<f32> {
    let lo = (lo_m / m_per_bin).ceil() as usize;
    let hi = ((hi_m / m_per_bin) as usize).min(profile.len());
    if hi <= lo + POOL {
        return vec![];
    }
    profile[lo..hi].chunks_exact(POOL).map(|c| c.iter().sum::<f32>() / POOL as f32).collect()
}

/// Echo peaks (distance of reflecting surface from the laptop) from an averaged profile.
/// Direction is unknown with one speaker and one microphone.
pub fn echoes_from_profile(avg: &[f32], m_per_bin: f32) -> Vec<Echo> {
    let lo = (MIN_WALL_M / m_per_bin).ceil() as usize;
    let hi = ((MAX_WALL_M / m_per_bin) as usize).min(avg.len());
    if hi <= lo + 10 {
        return vec![];
    }
    let seg = &avg[lo..hi];
    let mut s = seg.to_vec();
    let med = median(&mut s).unwrap().max(1e-12);
    let mut dev: Vec<f32> = seg.iter().map(|v| (v - med).abs()).collect();
    let mad = median(&mut dev).unwrap().max(1e-12);
    let thr = med + 6.0 * 1.4826 * mad;
    let mut peaks: Vec<(usize, f32)> = vec![];
    for i in 1..seg.len() - 1 {
        if seg[i] > thr && seg[i] >= seg[i - 1] && seg[i] > seg[i + 1] {
            if let Some(last) = peaks.last_mut() {
                if i - last.0 < 6 {
                    if seg[i] > last.1 {
                        *last = (i, seg[i]);
                    }
                    continue;
                }
            }
            peaks.push((i, seg[i]));
        }
    }
    peaks.sort_by(|a, b| b.1.total_cmp(&a.1));
    peaks.truncate(6);
    peaks.sort_by_key(|p| p.0);
    peaks
        .into_iter()
        .map(|(i, v)| {
            let snr = 20.0 * (v / med).log10();
            Echo { distance_m: (lo + i) as f32 * m_per_bin, snr_db: snr, confidence: ((snr - 6.0) / 20.0).clamp(0.0, 1.0) }
        })
        .collect()
}

/// Averages many aligned profiles into one, keeping only frames whose direct path was clean.
pub struct RoomScanAccumulator {
    sum: Vec<f32>,
    n: u32,
    m_per_bin: f32,
    pub rejected: u32,
}

impl RoomScanAccumulator {
    pub fn new() -> Self {
        Self { sum: vec![], n: 0, m_per_bin: 0.0, rejected: 0 }
    }
    pub fn frames(&self) -> u32 {
        self.n
    }
    pub fn push(&mut self, profile: &[f32], m_per_bin: f32, direct_snr_db: f32, min_snr_db: f32) {
        if direct_snr_db < min_snr_db {
            self.rejected += 1;
            return;
        }
        if self.sum.is_empty() {
            self.sum = vec![0.0; profile.len()];
            self.m_per_bin = m_per_bin;
        }
        for (a, b) in self.sum.iter_mut().zip(profile) {
            *a += b;
        }
        self.n += 1;
    }
    pub fn finish(&self, min_frames: u32) -> Result<Vec<Echo>, String> {
        if self.n < min_frames {
            return Err(format!("only {} clean frames captured (need {min_frames}); {} rejected as too noisy", self.n, self.rejected));
        }
        let avg: Vec<f32> = self.sum.iter().map(|v| v / self.n as f32).collect();
        Ok(echoes_from_profile(&avg, self.m_per_bin))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SoundMotion {
    Unknown(String),
    Still,
    /// confidence, range of the strongest change in metres from the laptop
    Moving(f32, f32),
}

pub struct MotionDetector {
    mu: Vec<f32>,
    dev: Vec<f32>,
    acc: Vec<f32>,
    frames: usize,
    scores: VecDeque<f32>,
    lo_m: f32,
    /// Highest score seen while learning the empty room; the motion threshold is set above it.
    warm_max: f32,
    rng_hist: VecDeque<f32>,
    mover: Option<(f32, f32, Option<f32>)>,
}

const WARMUP_FRAMES: usize = 100;
const SCORE_WIN: usize = 16;
const MOVE_SCORE: f32 = 2.0;

impl MotionDetector {
    pub fn new() -> Self {
        Self { mu: vec![], dev: vec![], acc: vec![], frames: 0, scores: VecDeque::new(), lo_m: MIN_WALL_M, warm_max: 0.0, rng_hist: VecDeque::new(), mover: None }
    }

    /// While movement is detected: (distance in m, echo size in dB corrected for distance, speed in m/s if known).
    pub fn mover(&self) -> Option<(f32, f32, Option<f32>)> {
        self.mover
    }

    pub fn update(&mut self, profile: &[f32], m_per_bin: f32, period_s: f32) -> SoundMotion {
        self.mover = None;
        let x = pooled(profile, m_per_bin, MIN_WALL_M, 6.0);
        if x.is_empty() {
            return SoundMotion::Unknown("echo profile too short".into());
        }
        if self.mu.len() != x.len() {
            self.mu = x.clone();
            self.dev = vec![0.0; x.len()];
            self.acc = vec![0.0; x.len()];
            self.frames = 0;
            self.scores.clear();
        }
        self.frames += 1;
        let mut dsorted = self.dev.clone();
        let floor = 0.5 * median(&mut dsorted).unwrap_or(0.0);
        let m: Vec<f32> = x.iter().enumerate().map(|(k, v)| (v - self.mu[k]).abs() / self.dev[k].max(floor).max(1e-9)).collect();
        // Evidence per distance bin, averaged over about 10 frames. Noise averages down; a person who stays put does not.
        for k in 0..x.len() {
            self.acc[k] = 0.9 * self.acc[k] + 0.1 * m[k].min(20.0);
        }
        let score = self.acc.iter().copied().fold(0.0f32, f32::max);
        if self.frames > 50 && self.frames <= WARMUP_FRAMES {
            self.warm_max = self.warm_max.max(score);
        }
        let thr = MOVE_SCORE.max(1.3 * self.warm_max);
        let moving_now = self.frames > WARMUP_FRAMES && score >= thr;
        // track where the strongest change is, frame by frame, to get the mover's speed
        if moving_now {
            let kinst = m.iter().enumerate().fold((0, 0.0f32), |a, (k, v)| if *v > a.1 { (k, *v) } else { a }).0;
            self.rng_hist.push_back(self.lo_m + (kinst * POOL) as f32 * m_per_bin);
            while self.rng_hist.len() > 16 {
                self.rng_hist.pop_front();
            }
        } else {
            self.rng_hist.clear();
        }
        if !moving_now {
            for k in 0..x.len() {
                self.dev[k] = 0.98 * self.dev[k] + 0.02 * (x[k] - self.mu[k]).abs();
                self.mu[k] = 0.98 * self.mu[k] + 0.02 * x[k];
            }
        }
        if self.frames <= WARMUP_FRAMES {
            return SoundMotion::Unknown(format!("learning the room's echo pattern ({}/{WARMUP_FRAMES})", self.frames));
        }
        self.scores.push_back(score);
        if self.scores.len() > SCORE_WIN {
            self.scores.pop_front();
        }
        if self.scores.len() < SCORE_WIN {
            return SoundMotion::Unknown("collecting".into());
        }
        let mut sc: Vec<f32> = self.scores.iter().copied().collect();
        let med = median(&mut sc).unwrap();
        if med >= thr {
            let (kmax, amax) = self.acc.iter().enumerate().fold((0, 0.0f32), |a, (k, v)| if *v > a.1 { (k, *v) } else { a });
            let range = self.lo_m + (kmax * POOL) as f32 * m_per_bin;
            let speed = if self.rng_hist.len() >= 8 { Some((self.rng_hist.back().unwrap() - self.rng_hist.front().unwrap()).abs() / ((self.rng_hist.len() - 1) as f32 * period_s)) } else { None };
            let size_db = 20.0 * (amax.max(0.1) * range.max(0.3) * range.max(0.3)).log10();
            self.mover = Some((range, size_db, speed));
            SoundMotion::Moving(((med - thr) / 4.0).clamp(0.05, 1.0), range)
        } else {
            SoundMotion::Still
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Vitals {
    /// (breaths per minute, confidence)
    pub breathing: Option<(f32, f32)>,
    /// (beats per minute, confidence). Experimental; strict gate.
    pub heart: Option<(f32, f32)>,
    pub reason: Option<String>,
    /// Distance from the laptop of the bin the signal came from, in metres.
    pub range_m: Option<f32>,
}

/// Breathing and heart rate from the *phase* of the echo, not its strength.
///
/// An echo from a target at distance d has phase 4*pi*d/wavelength (wavelength is about 18 mm at 19 kHz).
/// Chest motion of several millimetres swings the phase by several radians. Looking at strength only
/// would double the apparent rate (a full phase swing passes the strength peak twice), so the estimator
/// recovers the actual phase swing instead: a circle fit to the (I, Q) points followed by an unwrapped
/// angle, falling back to a straight-line projection when the motion is tiny.
pub struct VitalsEstimator {
    m_per_bin: f32,
    lo_bin: usize,
    series: VecDeque<Vec<(f32, f32)>>,
    snr: VecDeque<f32>,
    moving: VecDeque<bool>,
    period_s: f32,
    epoch: u32,
}

const WINDOW_S: f32 = 40.0;
const MIN_DIRECT_SNR_DB: f32 = 20.0;
const BREATH_RATIO_DB: f32 = 15.0;
const HEART_RATIO_DB: f32 = 20.0;
const MAX_CANDIDATES: usize = 6;

impl VitalsEstimator {
    pub fn new() -> Self {
        Self { m_per_bin: 0.0036, lo_bin: 0, series: VecDeque::new(), snr: VecDeque::new(), moving: VecDeque::new(), period_s: 0.06, epoch: 0 }
    }

    fn reset(&mut self) {
        self.series.clear();
        self.snr.clear();
        self.moving.clear();
    }

    /// `iq` is the complex echo profile referenced to the direct path (index 0), from the audio crate.
    pub fn push(&mut self, iq: &[(f32, f32)], m_per_bin: f32, direct_snr_db: f32, period_s: f32, moving: bool, epoch: u32) {
        if epoch != self.epoch {
            self.reset();
            self.epoch = epoch;
        }
        self.period_s = period_s;
        self.m_per_bin = m_per_bin;
        let lo = (MIN_WALL_M / m_per_bin).ceil() as usize;
        let hi = ((2.5 / m_per_bin) as usize).min(iq.len());
        if hi <= lo + POOL {
            return;
        }
        let v: Vec<(f32, f32)> = iq[lo..hi].to_vec();
        if self.series.back().map_or(false, |b| b.len() != v.len()) {
            self.reset();
        }
        self.lo_bin = lo;
        let cap = (WINDOW_S / period_s) as usize;
        self.series.push_back(v);
        self.snr.push_back(direct_snr_db);
        self.moving.push_back(moving);
        while self.series.len() > cap {
            self.series.pop_front();
            self.snr.pop_front();
            self.moving.pop_front();
        }
    }

    pub fn estimate(&self) -> Vitals {
        let cap = (WINDOW_S / self.period_s) as usize;
        let none = |r: &str| Vitals { breathing: None, heart: None, reason: Some(r.into()), range_m: None };
        if self.series.len() < cap {
            return none("collecting 40 s of echo data");
        }
        if self.moving.iter().any(|m| *m) {
            return none("person not still");
        }
        let mut s: Vec<f32> = self.snr.iter().copied().collect();
        if median(&mut s).unwrap() < MIN_DIRECT_SNR_DB {
            return none("signal not clean enough");
        }
        let fs = 1.0 / self.period_s;
        let n = self.series.len();
        let nbin = self.series[0].len();
        // Bins where the echo changes most over the window
        let mut energy: Vec<(usize, f32)> = (0..nbin)
            .map(|k| {
                let (mut mr, mut mi) = (0.0f32, 0.0f32);
                for f in &self.series {
                    mr += f[k].0;
                    mi += f[k].1;
                }
                let (mr, mi) = (mr / n as f32, mi / n as f32);
                let e: f32 = self.series.iter().map(|f| (f[k].0 - mr).powi(2) + (f[k].1 - mi).powi(2)).sum::<f32>() / n as f32;
                (k, e)
            })
            .collect();
        energy.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut cands: Vec<usize> = vec![];
        for (k, _) in &energy {
            if cands.iter().all(|c| (*c as i32 - *k as i32).abs() >= 12) {
                cands.push(*k);
                if cands.len() == MAX_CANDIDATES {
                    break;
                }
            }
        }
        let mut best: Option<(usize, Vec<f32>, f32, f32)> = None;
        for &k in &cands {
            let col: Vec<(f32, f32)> = self.series.iter().map(|f| f[k]).collect();
            if let Some(w) = demod(&col) {
                if let Some((f, r)) = band_peak(&w, fs, 0.1, 0.5, &[]) {
                    if best.as_ref().map_or(true, |b| r > b.3) {
                        best = Some((k, w, f, r));
                    }
                }
            }
        }
        let Some((k, w, f, r)) = best else { return none("no clean periodic motion found") };
        let thirty = (30.0 / self.period_s) as usize;
        let off = n - thirty;
        let stable = |lo: f32, hi: f32, excl: &[(f32, f32)], f0: f32| -> bool {
            let a = band_peak(&w[..thirty], fs, lo, hi, excl);
            let b = band_peak(&w[off..], fs, lo, hi, excl);
            matches!((a, b), (Some((fa, _)), Some((fb, _))) if (fa - fb).abs() <= 0.05 && (fa - f0).abs() <= 0.05)
        };
        let mut v = Vitals::default();
        let mut excl: Vec<(f32, f32)> = vec![];
        if r >= BREATH_RATIO_DB && stable(0.1, 0.5, &[], f) {
            v.breathing = Some((f * 60.0, ((r - BREATH_RATIO_DB) / 10.0).clamp(0.05, 1.0)));
            v.range_m = Some((self.lo_bin + k) as f32 * self.m_per_bin);
            for h in 2..=5 {
                excl.push((f * h as f32 - 0.08, f * h as f32 + 0.08));
            }
            // Heart rate only when breathing was established first (a person is there, still, and the signal is clean).
            if let Some((fh, rh)) = band_peak(&w, fs, 0.8, 2.5, &excl) {
                if rh >= HEART_RATIO_DB && stable(0.8, 2.5, &excl, fh) {
                    v.heart = Some((fh * 60.0, ((rh - HEART_RATIO_DB) / 10.0).clamp(0.05, 1.0)));
                }
            }
        }
        if v.breathing.is_none() {
            v.reason = Some("no periodic motion strong and steady enough".into());
        }
        v
    }
}

/// Turn a complex echo series at one distance bin into a motion waveform (arbitrary units, proportional to
/// displacement). Returns None if the points do not look like real phase motion.
fn demod(s: &[(f32, f32)]) -> Option<Vec<f32>> {
    let n = s.len() as f64;
    if n < 16.0 {
        return None;
    }
    let (mut mx, mut my) = (0.0f64, 0.0f64);
    for p in s {
        mx += p.0 as f64;
        my += p.1 as f64;
    }
    mx /= n;
    my /= n;
    let u: Vec<f64> = s.iter().map(|p| p.0 as f64 - mx).collect();
    let v: Vec<f64> = s.iter().map(|p| p.1 as f64 - my).collect();
    let (mut suu, mut svv, mut suv, mut sut, mut svt, mut st) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for i in 0..s.len() {
        let t = u[i] * u[i] + v[i] * v[i];
        suu += u[i] * u[i];
        svv += v[i] * v[i];
        suv += u[i] * v[i];
        sut += u[i] * t;
        svt += v[i] * t;
        st += t;
    }
    if suu + svv < 1e-18 {
        return None;
    }
    // Circle fit (Kasa): u^2 + v^2 = 2 a u + 2 b v + c, solved in closed form because the data are centred.
    let det = 16.0 * (suu * svv - suv * suv);
    if det.abs() > 1e-18 * (suu + svv).powi(2) {
        let a = (2.0 * sut * 4.0 * svv - 4.0 * suv * 2.0 * svt) / det;
        let b = (4.0 * suu * 2.0 * svt - 4.0 * suv * 2.0 * sut) / det;
        let c = st / n;
        let r2 = c + a * a + b * b;
        if r2 > 0.0 {
            let r = r2.sqrt();
            let res = (0..s.len()).map(|i| (((u[i] - a).powi(2) + (v[i] - b).powi(2)).sqrt() - r).powi(2)).sum::<f64>() / n;
            let res = res.sqrt();
            let mut ang: Vec<f64> = (0..s.len()).map(|i| (v[i] - b).atan2(u[i] - a)).collect();
            for i in 1..ang.len() {
                let mut d = ang[i] - ang[i - 1];
                while d > std::f64::consts::PI {
                    d -= std::f64::consts::TAU;
                    ang[i] -= std::f64::consts::TAU;
                }
                while d < -std::f64::consts::PI {
                    d += std::f64::consts::TAU;
                    ang[i] += std::f64::consts::TAU;
                }
            }
            let swing = ang.iter().cloned().fold(f64::MIN, f64::max) - ang.iter().cloned().fold(f64::MAX, f64::min);
            if r.is_finite() && swing >= 1.0 && r / res.max(1e-12) >= 4.0 {
                return Some(ang.iter().map(|a| *a as f32).collect());
            }
        }
    }
    // Small motion: the points lie on a short, nearly straight arc. Use the projection on the main axis.
    let tr = (suu + svv) / n;
    let dt = (suu * svv - suv * suv) / (n * n);
    let disc = (tr * tr / 4.0 - dt).max(0.0).sqrt();
    let (l1, l2) = (tr / 2.0 + disc, (tr / 2.0 - disc).max(0.0));
    if l1 > 0.0 && (l1 / l2.max(1e-30)).sqrt() >= 4.0 {
        let th = 0.5 * (2.0 * suv).atan2(suu - svv);
        return Some((0..s.len()).map(|i| (u[i] * th.cos() + v[i] * th.sin()) as f32).collect());
    }
    None
}

/// Strongest spectral peak in [lo,hi] Hz (skipping excluded ranges) and its ratio to the median
/// power of 0.05-2.5 Hz, in dB.
fn band_peak(x: &[f32], fs: f32, lo: f32, hi: f32, excl: &[(f32, f32)]) -> Option<(f32, f32)> {
    let n = x.len();
    if n < 16 {
        return None;
    }
    let t: Vec<f32> = (0..n).map(|i| i as f32).collect();
    let tm = (n - 1) as f32 / 2.0;
    let xm = x.iter().sum::<f32>() / n as f32;
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for i in 0..n {
        sxy += (t[i] - tm) * (x[i] - xm);
        sxx += (t[i] - tm) * (t[i] - tm);
    }
    let slope = sxy / sxx;
    let nfft = 8192usize.max(n.next_power_of_two());
    let mut buf: Vec<Complex32> = (0..nfft)
        .map(|i| {
            if i < n {
                let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (n as f32 - 1.0)).cos();
                Complex32::new((x[i] - xm - slope * (t[i] - tm)) * w, 0.0)
            } else {
                Complex32::new(0.0, 0.0)
            }
        })
        .collect();
    FftPlanner::<f32>::new().plan_fft_forward(nfft).process(&mut buf);
    let df = fs / nfft as f32;
    let power = |i: usize| buf[i].norm_sqr();
    let (ilo, ihi) = ((0.05 / df) as usize, (2.5 / df) as usize);
    let mut ref_p: Vec<f32> = (ilo..ihi.min(nfft / 2)).map(power).collect();
    let refm = median(&mut ref_p)?.max(1e-20);
    let mut best: Option<(f32, f32)> = None;
    for i in ((lo / df) as usize)..=((hi / df) as usize).min(nfft / 2 - 1) {
        let f = i as f32 * df;
        if excl.iter().any(|(a, b)| f >= *a && f <= *b) {
            continue;
        }
        let p = power(i);
        if best.map_or(true, |b| p > b.1) {
            best = Some((f, p));
        }
    }
    best.map(|(f, p)| (f, 10.0 * (p / refm).log10()))
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    /// Exact formula, no noise: static offset + a target whose echo phase follows chest motion.
    fn formula_series(n: usize, dt: f32, breath_rad: f32, heart_rad: f32) -> Vec<(f32, f32)> {
        (0..n)
            .map(|i| {
                let t = i as f32 * dt;
                let ph = breath_rad * (TAU * 0.25 * t).sin() + heart_rad * (TAU * 1.2 * t).sin();
                (1.0 + 2.0 * ph.cos(), 0.5 + 2.0 * ph.sin())
            })
            .collect()
    }

    /// MATH CHECK (noiseless formula, never reaches the UI): a phase swing of 3.5 rad at 0.25 Hz is recovered
    /// as 0.25 Hz, not the doubled 0.5 Hz that a strength-only method gives.
    #[test]
    fn math_check_phase_demod_no_doubling() {
        let w = demod(&formula_series(667, 0.06, 3.5, 0.0)).unwrap();
        let (f, _) = band_peak(&w, 1.0 / 0.06, 0.1, 0.5, &[]).unwrap();
        assert!((f - 0.25).abs() < 0.02, "got {f}");
    }

    /// MATH CHECK: a constant series (nothing moving) gives no waveform.
    #[test]
    fn math_check_constant_series_is_none() {
        assert!(demod(&vec![(1.0, 0.5); 100]).is_none());
    }

    /// MATH CHECK (noiseless formula): full estimator recovers 15 breaths/min and 72 beats/min.
    #[test]
    fn math_check_estimator_recovers_rates() {
        let series = formula_series(700, 0.06, 3.5, 0.2);
        let mut e = VitalsEstimator::new();
        for s in &series {
            let mut iq = vec![(0.0f32, 0.0f32); 700];
            iq[100] = *s;
            e.push(&iq, 0.0036, 35.0, 0.06, false, 0);
        }
        let v = e.estimate();
        let (b, _) = v.breathing.expect("breathing");
        assert!((b - 15.0).abs() < 1.5, "breathing {b}");
        let (h, _) = v.heart.expect("heart");
        assert!((h - 72.0).abs() < 3.0, "heart {h}");
    }
}
