// SPDX-License-Identifier: AGPL-3.0-or-later
//! Everything stored is a real measurement or something the user typed/placed.
//! There are no defaults that look like data: new houses start with no floors and no spots.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApStat {
    pub bssid: String,
    pub ssid: Option<String>,
    pub mean_dbm: f32,
    pub std_dbm: f32,
    /// Number of distinct OS measurements averaged.
    pub n: u32,
}

/// A place the user stood and tapped "I'm here". Position is where the user tapped (metres on the grid).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spot {
    pub id: String,
    pub x: f32,
    pub y: f32,
    /// Optional name typed by the user (e.g. a room name). Never auto-generated.
    pub label: Option<String>,
    pub taken_unix: u64,
    pub aps: Vec<ApStat>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum OpeningKind {
    Door,
    Window,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum OpenState {
    Open,
    Closed,
}

/// One recorded segment while the user held the opening in a known state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingSegment {
    pub state: OpenState,
    pub taken_unix: u64,
    pub aps: Vec<ApStat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Opening {
    pub id: String,
    pub kind: OpeningKind,
    pub x: f32,
    pub y: f32,
    /// Orientation chosen by the user: true = horizontal wall segment.
    pub horizontal: bool,
    pub label: Option<String>,
    pub training: Vec<TrainingSegment>,
}

/// One measured echo from the sound scan (distance only; a single speaker/mic gives no direction).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Echo {
    pub distance_m: f32,
    /// How far above the noise floor the echo peak was, in dB.
    pub snr_db: f32,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomScan {
    pub id: String,
    pub x: f32,
    pub y: f32,
    pub taken_unix: u64,
    pub echoes: Vec<Echo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Floor {
    pub id: String,
    pub name: String,
    /// Sort order, lowest first (basement < ground < 1st ...), set by the user.
    pub level: i32,
    pub spots: Vec<Spot>,
    pub openings: Vec<Opening>,
    pub room_scans: Vec<RoomScan>,
    /// Furniture and objects the user placed. Never detected, never guessed.
    #[serde(default)]
    pub objects: Vec<PlacedObject>,
    /// Walls the user drew. Never detected, never guessed.
    #[serde(default)]
    pub walls: Vec<UserWall>,
    /// Where the user put this computer on the map (it normally stays in one place). Never guessed.
    #[serde(default)]
    pub computer: Option<(f32, f32)>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ObjectKind {
    Sofa,
    Bed,
    Table,
    Chair,
    Desk,
    Tv,
    Lamp,
    Plant,
    Shelf,
    Fridge,
    Wardrobe,
    Box,
}

impl ObjectKind {
    pub const ALL: [ObjectKind; 12] = [
        ObjectKind::Sofa, ObjectKind::Bed, ObjectKind::Table, ObjectKind::Chair, ObjectKind::Desk, ObjectKind::Tv,
        ObjectKind::Lamp, ObjectKind::Plant, ObjectKind::Shelf, ObjectKind::Fridge, ObjectKind::Wardrobe, ObjectKind::Box,
    ];
    pub fn name(&self) -> &'static str {
        match self {
            ObjectKind::Sofa => "Sofa",
            ObjectKind::Bed => "Bed",
            ObjectKind::Table => "Table",
            ObjectKind::Chair => "Chair",
            ObjectKind::Desk => "Desk",
            ObjectKind::Tv => "TV",
            ObjectKind::Lamp => "Lamp",
            ObjectKind::Plant => "Plant",
            ObjectKind::Shelf => "Shelf",
            ObjectKind::Fridge => "Fridge",
            ObjectKind::Wardrobe => "Wardrobe",
            ObjectKind::Box => "Box",
        }
    }
    /// Starting size (width, depth, height in metres) when the user places one. The user can change it.
    pub fn default_size(&self) -> (f32, f32, f32) {
        match self {
            ObjectKind::Sofa => (2.0, 0.9, 0.85),
            ObjectKind::Bed => (1.6, 2.0, 0.55),
            ObjectKind::Table => (1.4, 0.8, 0.75),
            ObjectKind::Chair => (0.5, 0.5, 0.9),
            ObjectKind::Desk => (1.4, 0.7, 0.75),
            ObjectKind::Tv => (1.2, 0.25, 0.8),
            ObjectKind::Lamp => (0.35, 0.35, 1.5),
            ObjectKind::Plant => (0.5, 0.5, 1.0),
            ObjectKind::Shelf => (0.9, 0.35, 1.8),
            ObjectKind::Fridge => (0.7, 0.7, 1.8),
            ObjectKind::Wardrobe => (1.2, 0.6, 2.0),
            ObjectKind::Box => (0.6, 0.6, 0.6),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlacedObject {
    pub id: String,
    pub kind: ObjectKind,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub d: f32,
    pub h: f32,
    pub rot_deg: f32,
    pub label: Option<String>,
}

impl PlacedObject {
    /// Local coordinates (centre origin, x across the width, y across the depth) to plan metres.
    pub fn to_world(&self, lx: f32, ly: f32) -> (f32, f32) {
        let r = self.rot_deg.to_radians();
        (self.x + lx * r.cos() - ly * r.sin(), self.y + lx * r.sin() + ly * r.cos())
    }
    pub fn corners(&self) -> [(f32, f32); 4] {
        let (hw, hd) = (self.w / 2.0, self.d / 2.0);
        [self.to_world(-hw, -hd), self.to_world(hw, -hd), self.to_world(hw, hd), self.to_world(-hw, hd)]
    }
    pub fn contains(&self, x: f32, y: f32, margin: f32) -> bool {
        let r = self.rot_deg.to_radians();
        let (dx, dy) = (x - self.x, y - self.y);
        let (lx, ly) = (dx * r.cos() + dy * r.sin(), -dx * r.sin() + dy * r.cos());
        lx.abs() <= self.w / 2.0 + margin && ly.abs() <= self.d / 2.0 + margin
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserWall {
    pub id: String,
    pub a: (f32, f32),
    pub b: (f32, f32),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum Sensitivity {
    Low,
    #[default]
    Medium,
    High,
}

/// Away mode settings, per house.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwaySettings {
    pub sensitivity: Sensitivity,
    /// Seconds after pressing Arm before alerts can fire, so you can leave.
    pub grace_secs: u32,
    /// Minutes before the same kind of alert can fire again.
    pub cooldown_mins: u32,
    /// Desktop notifications (local only) on top of the in-app banner.
    pub notify: bool,
    /// Do not raise alerts for movement the app is fairly sure is a pet (only works after you have taught it).
    #[serde(default)]
    pub ignore_pets: bool,
}
impl Default for AwaySettings {
    fn default() -> Self {
        Self { sensitivity: Sensitivity::Medium, grace_secs: 120, cooldown_mins: 5, notify: true, ignore_pets: false }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum WhoLabel {
    Person,
    Pet,
    Other,
}

/// What the sensors measured while the user said who was moving. Used to learn what people and pets look like in this home.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MotionSample {
    pub at_unix: u64,
    pub who: WhoLabel,
    pub size_db: Option<f32>,
    pub speed_mps: Option<f32>,
    pub wifi_ratio: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AlertLabel {
    Real,
    FalseAlarm(String),
}

/// One alert, with the evidence that triggered it, so it can be judged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRecord {
    pub id: String,
    pub at_unix: u64,
    pub kind: String,
    pub summary: String,
    pub evidence: Vec<String>,
    pub confidence: f32,
    pub label: Option<AlertLabel>,
    pub seen: bool,
}

/// Calibration measured on this computer's audio hardware. Cleared by "clear cache".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cache {
    pub audio_self_test_unix: Option<u64>,
    pub audio_self_test_snr_db: Option<f32>,
    pub audio_sample_rate: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct House {
    pub id: String,
    pub name: String,
    pub floors: Vec<Floor>,
    pub wifi_enabled: bool,
    pub sound_enabled: bool,
    /// Pet-safe mode: quieter chirps and listening only part of the time. On by default.
    #[serde(default = "yes")]
    pub pet_safe: bool,
    /// Sound is never played during quiet hours (local time) when enabled.
    #[serde(default)]
    pub quiet_hours: bool,
    #[serde(default = "default_quiet_start")]
    pub quiet_start_hour: u8,
    #[serde(default = "default_quiet_end")]
    pub quiet_end_hour: u8,
    pub cache: Cache,
    #[serde(default)]
    pub away: AwaySettings,
    /// True while Away mode is armed, so it can resume after a restart.
    #[serde(default)]
    pub away_armed: bool,
    #[serde(default)]
    pub alerts: Vec<AlertRecord>,
    /// Movement the user taught the app: who it was, with the measurements taken at that moment.
    #[serde(default)]
    pub motion_samples: Vec<MotionSample>,
}

fn yes() -> bool {
    true
}
fn default_quiet_start() -> u8 {
    22
}
fn default_quiet_end() -> u8 {
    7
}

impl House {
    pub fn new(id: String, name: String) -> Self {
        Self { id, name, floors: vec![], wifi_enabled: true, sound_enabled: true, pet_safe: true, quiet_hours: false, quiet_start_hour: 22, quiet_end_hour: 7, cache: Cache::default(), away: AwaySettings::default(), away_armed: false, alerts: vec![], motion_samples: vec![] }
    }
}

/// Non-sensitive app state kept in plain text: which house is active and whether consent was given.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppState {
    pub consent_given: bool,
    pub active_house: Option<String>,
    /// Last time the app was alive while Away mode was armed (unix seconds). Used to report gaps.
    #[serde(default)]
    pub heartbeat_unix: u64,
}


#[cfg(test)]
mod tests {
    use super::*;
    /// MATH CHECK: a 2 x 1 object rotated 90 degrees about (0,0) covers x in [-0.5,0.5], y in [-1,1].
    #[test]
    fn math_check_rotated_object_contains() {
        let o = PlacedObject { id: String::new(), kind: ObjectKind::Box, x: 0.0, y: 0.0, w: 2.0, d: 1.0, h: 1.0, rot_deg: 90.0, label: None };
        assert!(o.contains(0.0, 0.9, 0.0));
        assert!(!o.contains(0.9, 0.0, 0.0));
    }
}
