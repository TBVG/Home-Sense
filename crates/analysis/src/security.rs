// SPDX-License-Identifier: AGPL-3.0-or-later
//! Away mode: watches the live signals while nobody should be home and raises alerts with their evidence.
//!
//! It works with a single device. Several design rules keep false alarms down:
//! * nothing can fire during the grace period after arming (you are leaving),
//! * Wi-Fi movement must show up in several of the last few scans, not just one,
//! * the same kind of alert is silenced for a cooldown after it fires,
//! * a door/window must be seen open several times in a row after being seen closed.
//! The thresholds are unvalidated design choices; the alert log lets you mark each alert real or false
//! so they can be measured on your own home.

use crate::{monitor::{ChangeEvent, Movement}, openings::Class, sound::SoundMotion};
use std::collections::{HashMap, VecDeque};
use storage::{AwaySettings, Sensitivity};

#[derive(Debug, Clone)]
pub struct Alert {
    pub kind: &'static str,
    pub summary: String,
    pub evidence: Vec<String>,
    pub confidence: f32,
    pub at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    Disarmed,
    /// milliseconds left before alerts are live
    Arming(u64),
    Armed,
}

pub struct OpeningState<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub class: Class,
}

pub struct Inputs<'a> {
    pub now_ms: u64,
    /// Counts up by one for every fresh Wi-Fi scan.
    pub scan_seq: u64,
    pub movement: &'a Movement,
    /// Change events that appeared since the last call.
    pub new_changes: &'a [ChangeEvent],
    pub sound_ready: bool,
    pub sound: &'a SoundMotion,
    pub openings: &'a [OpeningState<'a>],
}

#[derive(Default)]
pub struct AwayMonitor {
    armed_at: Option<u64>,
    grace_until: u64,
    votes: VecDeque<bool>,
    last_seq: u64,
    last_alert: HashMap<&'static str, u64>,
    seen_closed: HashMap<String, bool>,
    open_streak: HashMap<String, u32>,
    sound_since: Option<u64>,
}

fn move_threshold(s: Sensitivity) -> (f32, usize) {
    match s {
        Sensitivity::Low => (0.6, 4),
        Sensitivity::Medium => (0.5, 3),
        Sensitivity::High => (0.4, 2),
    }
}
fn sound_hold_ms(s: Sensitivity) -> u64 {
    match s {
        Sensitivity::Low => 8000,
        Sensitivity::Medium => 5000,
        Sensitivity::High => 3000,
    }
}

impl AwayMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn arm(&mut self, now_ms: u64, grace_ms: u64) {
        *self = Self::default();
        self.armed_at = Some(now_ms);
        self.grace_until = now_ms + grace_ms;
    }

    pub fn disarm(&mut self) {
        *self = Self::default();
    }

    pub fn armed_at(&self) -> Option<u64> {
        self.armed_at
    }

    pub fn phase(&self, now_ms: u64) -> Phase {
        match self.armed_at {
            None => Phase::Disarmed,
            Some(_) if now_ms < self.grace_until => Phase::Arming(self.grace_until - now_ms),
            Some(_) => Phase::Armed,
        }
    }

    fn cooled(&mut self, kind: &'static str, now: u64, cooldown_ms: u64) -> bool {
        if self.last_alert.get(kind).map_or(false, |t| now < t + cooldown_ms) {
            return false;
        }
        self.last_alert.insert(kind, now);
        true
    }

    pub fn update(&mut self, inp: &Inputs, set: &AwaySettings) -> Vec<Alert> {
        let mut out = vec![];
        if self.armed_at.is_none() {
            return out;
        }
        let live = inp.now_ms >= self.grace_until;
        let cd = set.cooldown_mins as u64 * 60_000;
        let (frac_thr, need) = move_threshold(set.sensitivity);

        let new_scan = inp.scan_seq != self.last_seq;
        // Wi-Fi movement: several of the last 5 scans must show it
        if new_scan {
            self.last_seq = inp.scan_seq;
            if !live {
                self.votes.clear();
            } else {
                let vote = match inp.movement {
                    Movement::Moving(f) => Some((*f >= frac_thr, *f)),
                    Movement::NoUnusualVariation(_) => Some((false, 0.0)),
                    Movement::Unknown(_) => None,
                };
                if let Some((v, _)) = vote {
                    self.votes.push_back(v);
                    while self.votes.len() > 5 {
                        self.votes.pop_front();
                    }
                }
                let hits = self.votes.iter().filter(|v| **v).count();
                if self.votes.len() >= 5 && hits >= need {
                    if let Movement::Moving(f) = inp.movement {
                        if self.cooled("movement", inp.now_ms, cd) {
                            out.push(Alert {
                                kind: "movement",
                                summary: "Movement detected in the house".into(),
                                evidence: vec![
                                    format!("Wi-Fi signals varied unusually in {hits} of the last 5 scans"),
                                    format!("{:.0}% of access points showed unusual variation just now", f * 100.0),
                                ],
                                confidence: *f,
                                at_ms: inp.now_ms,
                            });
                        }
                    }
                }
            }
        }

        // lasting signal changes
        for e in inp.new_changes {
            if live && e.at_ms >= self.grace_until && self.cooled("signal-change", inp.now_ms, cd) {
                out.push(Alert {
                    kind: "signal-change",
                    summary: "Something changed in the house".into(),
                    evidence: vec![format!("{} access points shifted, median {:+.1} dB, and stayed shifted for 90+ s", e.aps, e.median_shift_db)],
                    confidence: (e.aps as f32 / 6.0).clamp(0.2, 1.0),
                    at_ms: inp.now_ms,
                });
            }
        }

        // sound motion, continuous for a few seconds
        match (inp.sound_ready, inp.sound) {
            (true, SoundMotion::Moving(c, r)) if *c >= 0.3 => {
                let since = *self.sound_since.get_or_insert(inp.now_ms);
                if live && inp.now_ms >= since + sound_hold_ms(set.sensitivity) && self.cooled("sound-motion", inp.now_ms, cd) {
                    out.push(Alert {
                        kind: "sound-motion",
                        summary: "Motion detected near the computer".into(),
                        evidence: vec![format!("Ultrasound saw continuous motion about {r:.1} m away ({:.0}% sure)", c * 100.0)],
                        confidence: *c,
                        at_ms: inp.now_ms,
                    });
                }
            }
            _ => self.sound_since = None,
        }

        // doors and windows: closed, then open several times in a row
        for o in inp.openings {
            match o.class {
                Class::Closed(c) if c >= 0.5 => {
                    self.seen_closed.insert(o.id.to_string(), true);
                    self.open_streak.insert(o.id.to_string(), 0);
                }
                Class::Open(c) if c >= 0.5 => {
                    if !live {
                        self.seen_closed.insert(o.id.to_string(), false); // already open when you left: not an alert
                    } else if self.seen_closed.get(o.id).copied().unwrap_or(false) {
                        let n = self.open_streak.entry(o.id.to_string()).or_insert(0);
                        if new_scan {
                            *n += 1; // counts distinct scans, not screen refreshes
                        }
                        if *n >= 3 {
                            *n = 0;
                            self.seen_closed.insert(o.id.to_string(), false);
                            if self.cooled("opening", inp.now_ms, cd) {
                                out.push(Alert {
                                    kind: "opening",
                                    summary: format!("{} was opened", o.name),
                                    evidence: vec![format!("{} changed from closed to open ({:.0}% sure)", o.name, c * 100.0)],
                                    confidence: c,
                                    at_ms: inp.now_ms,
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(m: &mut AwayMonitor, now: u64, seq: u64, mv: &Movement) -> Vec<Alert> {
        let set = AwaySettings::default();
        m.update(&Inputs { now_ms: now, scan_seq: seq, movement: mv, new_changes: &[], sound_ready: false, sound: &SoundMotion::Still, openings: &[] }, &set)
    }

    /// LOGIC CHECK (hand-made event sequence, never reaches the UI): 5 scans in a row showing movement fire exactly one alert,
    /// and the cooldown silences the next one.
    #[test]
    fn logic_check_movement_fires_once_then_cools_down() {
        let mut m = AwayMonitor::new();
        m.arm(0, 0);
        let mv = Movement::Moving(0.9);
        let mut fired = 0;
        for i in 1..=8u64 {
            fired += run(&mut m, i * 10_000, i, &mv).len();
        }
        assert_eq!(fired, 1);
    }

    /// LOGIC CHECK: 2 hits in the last 5 scans is below the Medium requirement of 3, so nothing fires.
    #[test]
    fn logic_check_sparse_movement_does_not_fire() {
        let mut m = AwayMonitor::new();
        m.arm(0, 0);
        let pattern = [true, false, true, false, false, false, false];
        let mut fired = 0;
        for (i, hit) in pattern.iter().enumerate() {
            let mv = if *hit { Movement::Moving(0.9) } else { Movement::NoUnusualVariation(0.9) };
            fired += run(&mut m, (i as u64 + 1) * 10_000, i as u64 + 1, &mv).len();
        }
        assert_eq!(fired, 0);
    }

    /// LOGIC CHECK: nothing can fire during the grace period.
    #[test]
    fn logic_check_grace_period_is_silent() {
        let mut m = AwayMonitor::new();
        m.arm(0, 120_000);
        let mv = Movement::Moving(1.0);
        let mut fired = 0;
        for i in 1..=8u64 {
            fired += run(&mut m, i * 10_000, i, &mv).len();
        }
        assert_eq!(fired, 0);
        assert!(matches!(m.phase(50_000), Phase::Arming(_)));
    }

    /// LOGIC CHECK: a door seen closed then open three times in a row fires once; one already open when arming never does.
    #[test]
    fn logic_check_opening_rules() {
        let set = AwaySettings::default();
        let mut m = AwayMonitor::new();
        m.arm(0, 0);
        let mut fired = 0;
        let seq = [Class::Closed(0.9), Class::Open(0.9), Class::Open(0.9), Class::Open(0.9)];
        for (i, c) in seq.iter().enumerate() {
            let ops = [OpeningState { id: "d", name: "Door", class: *c }];
            fired += m.update(&Inputs { now_ms: (i as u64 + 1) * 1000, scan_seq: i as u64 + 1, movement: &Movement::Unknown(String::new()), new_changes: &[], sound_ready: false, sound: &SoundMotion::Still, openings: &ops }, &set).len();
        }
        assert_eq!(fired, 1);
        let mut m2 = AwayMonitor::new();
        m2.arm(0, 5000);
        let ops = [OpeningState { id: "d", name: "Door", class: Class::Open(0.9) }];
        let mut fired2 = 0;
        for t in [1000u64, 6000, 7000, 8000, 9000] {
            fired2 += m2.update(&Inputs { now_ms: t, scan_seq: t, movement: &Movement::Unknown(String::new()), new_changes: &[], sound_ready: false, sound: &SoundMotion::Still, openings: &ops }, &set).len();
        }
        assert_eq!(fired2, 0);
    }
}
