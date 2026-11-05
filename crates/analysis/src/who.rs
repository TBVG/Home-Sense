// SPDX-License-Identifier: AGPL-3.0-or-later
//! Person or pet? Learned from the user's own labels, never guessed.
//!
//! Nothing in the sensors says "this is a cat". What differs is measurable: how big the echo is (corrected for
//! distance), how fast the mover travels, and how much the Wi-Fi links react compared with the sound echo
//! (Wi-Fi paths run at about table height, so a small low animal disturbs them less than a person does).
//! These numbers overlap between homes, so the app starts as "unknown" and only gives an answer after the user
//! has taught it at least a few examples of two different kinds, and then only when the answer is clear.

use storage::{MotionSample, WhoLabel};

#[derive(Debug, Clone, Copy, Default)]
pub struct Features {
    /// Echo size in dB, corrected for distance (bigger body, bigger number).
    pub size_db: Option<f32>,
    /// How fast the mover's distance changed, metres per second.
    pub speed_mps: Option<f32>,
    /// Wi-Fi link disturbance relative to the sound echo.
    pub wifi_ratio: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Who {
    Unknown(String),
    Person(f32),
    Pet(f32),
    Other(f32),
}

pub const MIN_SAMPLES_PER_CLASS: usize = 4;
const MIN_CONFIDENCE: f32 = 0.3;

fn vals(samples: &[&MotionSample], f: impl Fn(&MotionSample) -> Option<f32>) -> Vec<f32> {
    samples.iter().filter_map(|s| f(s)).collect()
}

fn stats(v: &[f32]) -> Option<(f32, f32)> {
    if v.len() < 2 {
        return None;
    }
    let m = v.iter().sum::<f32>() / v.len() as f32;
    let sd = (v.iter().map(|x| (x - m).powi(2)).sum::<f32>() / v.len() as f32).sqrt();
    Some((m, sd.max(0.15 * m.abs()).max(1e-3)))
}

pub fn classify(samples: &[MotionSample], f: &Features) -> Who {
    let kinds = [WhoLabel::Person, WhoLabel::Pet, WhoLabel::Other];
    let mut classes: Vec<(WhoLabel, [Option<(f32, f32)>; 3])> = vec![];
    for k in kinds {
        let s: Vec<&MotionSample> = samples.iter().filter(|s| s.who == k).collect();
        if s.len() >= MIN_SAMPLES_PER_CLASS {
            classes.push((k, [stats(&vals(&s, |m| m.size_db)), stats(&vals(&s, |m| m.speed_mps)), stats(&vals(&s, |m| m.wifi_ratio))]));
        }
    }
    if classes.len() < 2 {
        return Who::Unknown(format!("teach it at least {MIN_SAMPLES_PER_CLASS} examples of two kinds (person, pet, other)"));
    }
    let x = [f.size_db, f.speed_mps, f.wifi_ratio];
    let mut dists: Vec<(WhoLabel, f32)> = vec![];
    for (k, st) in &classes {
        let (mut d, mut n) = (0.0f32, 0);
        for i in 0..3 {
            if let (Some(xv), Some((m, sd))) = (x[i], st[i]) {
                d += ((xv - m) / sd).powi(2);
                n += 1;
            }
        }
        if n > 0 {
            dists.push((*k, d / n as f32));
        }
    }
    if dists.len() < 2 {
        return Who::Unknown("not enough measurements of this movement".into());
    }
    dists.sort_by(|a, b| a.1.total_cmp(&b.1));
    let conf = (dists[1].1 - dists[0].1) / (dists[1].1 + dists[0].1).max(1e-6);
    if conf < MIN_CONFIDENCE {
        return Who::Unknown("looks like more than one of the kinds you taught".into());
    }
    match dists[0].0 {
        WhoLabel::Person => Who::Person(conf),
        WhoLabel::Pet => Who::Pet(conf),
        WhoLabel::Other => Who::Other(conf),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(who: WhoLabel, size: f32) -> MotionSample {
        MotionSample { at_unix: 0, who, size_db: Some(size), speed_mps: None, wifi_ratio: None }
    }

    /// LOGIC CHECK (hand-made numbers, never reaches the UI): taught four people at about 20 dB and four pets at about 10 dB,
    /// a 19 dB movement is a person and an 11 dB one is a pet. With these spreads (3 dB and 1.5 dB) the two kinds are equally
    /// likely at 13.33 dB, which must come back unknown, and with nothing taught the answer is unknown.
    #[test]
    fn logic_check_nearest_taught_kind() {
        let mut v = vec![];
        for d in [-1.0, -0.5, 0.5, 1.0] {
            v.push(s(WhoLabel::Person, 20.0 + d));
            v.push(s(WhoLabel::Pet, 10.0 + d));
        }
        let f = |size| Features { size_db: Some(size), ..Default::default() };
        assert!(matches!(classify(&v, &f(19.0)), Who::Person(_)));
        assert!(matches!(classify(&v, &f(11.0)), Who::Pet(_)));
        assert!(matches!(classify(&[], &f(11.0)), Who::Unknown(_)));
        assert!(matches!(classify(&v, &f(13.3)), Who::Unknown(_)));
    }
}
