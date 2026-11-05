// SPDX-License-Identifier: AGPL-3.0-or-later
mod engine;
mod plan;
mod view3d;

use analysis::{
    aploc::{fit_aps, ApFit},
    personloc,
    who::{classify as classify_who, Features, Who},
    security::{AwayMonitor, Inputs, OpeningState, Phase},
    map::{detect_barriers, floor_loss_db, locate, Barrier, Fix},
    monitor::Movement,
    openings::{classify, untrainable_reason, Class},
    sound::SoundMotion,
};
use eframe::egui::{self, Align2, Color32, FontId, RichText, Sense};
use engine::{now_ms, AudioStatus, Engine};
use plan::{draw_floor, draw_grid, st, Blob, PlanData, GRID_H, GRID_W};
use storage::{new_id, now_unix, AlertLabel, AlertRecord, AppState, ApStat, Floor, House, MotionSample, ObjectKind, OpenState, Opening, OpeningKind, PlacedObject, WhoLabel, RoomScan, Sensitivity, Spot, Store, TrainingSegment, UserWall};

#[derive(PartialEq, Clone, Copy)]
enum Tool {
    None,
    Spot,
    Door,
    Window,
    SoundScan,
    Object(ObjectKind),
    Wall,
    MoveObject,
    Computer,
}

enum CaptureKind {
    Spot { label: String, with_sound: bool },
    Train { opening_id: String, state: OpenState },
}

struct Capture {
    kind: CaptureKind,
    x: f32,
    y: f32,
    start_ms: u64,
    started: std::time::Instant,
}

#[derive(Clone, Copy, PartialEq)]
enum Confirm {
    DeleteHouse,
    DeleteAll,
    Remap,
}

enum Screen {
    KeychainError(String),
    Consent,
    Main,
}

#[derive(Default)]
struct Derived {
    key: (u64, usize, usize, usize),
    fix: Option<Fix>,
    classes: Vec<Class>,
    barriers: Option<Result<Vec<Barrier>, String>>,
    link_weights: std::collections::HashMap<String, f32>,
    ap_fits: Vec<ApFit>,
}

/// Pet-safe listening schedule: sound on for LISTEN_ON_SECS out of every LISTEN_CYCLE_SECS.
const LISTEN_ON_SECS: u64 = 90;
const LISTEN_CYCLE_SECS: u64 = 240;

fn in_quiet_hours(h: &House) -> bool {
    use chrono::Timelike;
    if !h.quiet_hours || h.quiet_start_hour == h.quiet_end_hour {
        return false;
    }
    let now = chrono::Local::now().hour() as u8;
    let (s, e) = (h.quiet_start_hour, h.quiet_end_hour);
    if s < e { now >= s && now < e } else { now >= s || now < e }
}

struct App {
    store: Option<Store>,
    screen: Screen,
    state: AppState,
    consent_check: bool,
    engine: Engine,
    houses: Vec<(String, String)>,
    house: Option<House>,
    floor_idx: usize,
    paused: bool,
    tool: Tool,
    capture: Option<Capture>,
    room_scan_target: Option<(f32, f32)>,
    status: String,
    show_settings: bool,
    show_signals: bool,
    show_houses: bool,
    stacked: bool,
    show_layout: bool,
    show_barriers: bool,
    show_aps: bool,
    pending_teach: Option<WhoLabel>,
    selected_object: Option<String>,
    wall_start: Option<(f32, f32)>,
    away: AwayMonitor,
    banner: Option<String>,
    health_msg: Option<String>,
    health_notified: bool,
    events_seen: usize,
    last_tick: std::time::SystemTime,
    last_heartbeat: std::time::Instant,
    restore_gap: Option<(u64, u64)>,
    away_classes: Vec<(String, String, analysis::openings::Class)>,
    away_classes_scan: u64,
    alerts_open: bool,
    view3d: bool,
    cam: view3d::Camera,
    wizard: bool,
    wizard_floor: String,
    wizard_sound: bool,
    wizard_level: i32,
    muted: bool,
    duty_epoch: std::time::Instant,
    sound_off_reason: String,
    force_audio_until: Option<std::time::Instant>,
    pending_scan: Option<(f32, f32)>,
    guided_since: Option<std::time::Instant>,
    new_floor_name: String,
    new_floor_level: i32,
    new_house_name: String,
    rename_buf: String,
    spot_label: String,
    selected_opening: Option<String>,
    confirm: Option<Confirm>,
    backup_pass: String,
    derived: Derived,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::light());
        let mut app = Self {
            store: None, screen: Screen::Consent, state: AppState::default(), consent_check: false,
            engine: Engine::new(&cc.egui_ctx), houses: vec![], house: None, floor_idx: 0, paused: false,
            tool: Tool::None, capture: None, room_scan_target: None, status: String::new(),
            show_settings: false, show_signals: false, show_houses: false, stacked: false, show_layout: true, show_barriers: true, show_aps: true, pending_teach: None, selected_object: None, wall_start: None, away: AwayMonitor::new(), banner: None, health_msg: None, health_notified: false, events_seen: 0, last_tick: std::time::SystemTime::now(), last_heartbeat: std::time::Instant::now(), restore_gap: None, away_classes: vec![], away_classes_scan: u64::MAX, alerts_open: false, view3d: true, cam: view3d::Camera::default(),
            wizard: false, wizard_floor: String::new(), wizard_sound: false, wizard_level: 0, muted: false, duty_epoch: std::time::Instant::now(), sound_off_reason: String::new(), force_audio_until: None, pending_scan: None, guided_since: None,
            new_floor_name: String::new(), new_floor_level: 0, new_house_name: String::new(), rename_buf: String::new(),
            spot_label: String::new(), selected_opening: None, confirm: None,
            backup_pass: String::new(), derived: Derived::default(),
        };
        app.open_store();
        app
    }

    fn open_store(&mut self) {
        match Store::open_default() {
            Ok(s) => {
                self.state = s.load_state();
                self.houses = s.list_houses();
                let want = self.state.active_house.clone();
                let id = want.filter(|w| self.houses.iter().any(|h| &h.0 == w)).or_else(|| self.houses.first().map(|h| h.0.clone()));
                if let Some(id) = id {
                    self.house = s.load_house(&id).ok();
                }
                if self.house.as_ref().map_or(false, |h| h.away_armed) {
                    self.away.arm(now_ms(), 0);
                    let hb = self.state.heartbeat_unix;
                    if hb > 0 && now_unix() > hb + 120 {
                        self.restore_gap = Some((hb, now_unix()));
                    }
                }
                self.screen = if self.state.consent_given { Screen::Main } else { Screen::Consent };
                self.store = Some(s);
            }
            Err(e) => self.screen = Screen::KeychainError(e.to_string()),
        }
    }

    fn save(&mut self) {
        if let (Some(s), Some(h)) = (&self.store, &self.house) {
            if let Err(e) = s.save_house(h) {
                self.status = format!("Could not save: {e}");
            }
        }
    }

    fn switch_house(&mut self, id: &str) {
        if let Some(s) = &self.store {
            self.house = s.load_house(id).ok();
            self.state.active_house = Some(id.to_string());
            let _ = s.save_state(&self.state);
        }
        if self.away.armed_at().is_some() {
            self.away.disarm();
            self.status = "Away mode was disarmed because you switched houses.".into();
        }
        self.selected_object = None;
        self.wall_start = None;
        self.banner = None;
        self.health_msg = None;
        self.floor_idx = 0;
        self.view3d = true;
        self.capture = None;
        self.selected_opening = None;
        self.tool = Tool::None;
        self.engine.clear_live();
        self.engine.audio_status = AudioStatus::Off;
        self.derived = Derived::default();
    }

    fn refresh_houses(&mut self) {
        if let Some(s) = &self.store {
            self.houses = s.list_houses();
        }
    }

    fn mapped(&self) -> bool {
        self.house.as_ref().map_or(false, |h| !h.floors.is_empty())
    }

    /// Start/stop real sensors according to pause, settings and whether a house is being mapped.
    fn sync_sensing(&mut self) {
        let active = matches!(self.screen, Screen::Main) && self.mapped() && !self.paused;
        let (wifi, sound, pet, quiet) = self.house.as_ref().map_or((false, false, true, false), |h| (h.wifi_enabled, h.sound_enabled, h.pet_safe, in_quiet_hours(h)));
        self.engine.set_wifi_active(active && wifi);
        let forced = self.force_audio_until.map_or(false, |t| std::time::Instant::now() < t) || self.engine.room_scan_active();
        let on_window = !pet || forced || self.duty_epoch.elapsed().as_secs() % LISTEN_CYCLE_SECS < LISTEN_ON_SECS;
        let disabled = matches!(self.engine.audio_status, AudioStatus::Disabled(_));
        let (want, reason) = if !active {
            (false, "paused")
        } else if !sound {
            (false, "turned off in Settings")
        } else if self.muted {
            (false, "muted")
        } else if quiet {
            (false, "quiet hours")
        } else if disabled {
            (false, "disabled, see below")
        } else if !on_window {
            (false, "pet-safe rest period")
        } else {
            (true, "")
        };
        self.sound_off_reason = reason.into();
        let amp = if pet { audio::PET_SAFE_AMPLITUDE } else { audio::NORMAL_AMPLITUDE };
        if self.engine.audio_running() && (self.engine.amplitude_in_use() - amp).abs() > 1e-3 {
            self.engine.stop_audio();
        }
        if want && !self.engine.audio_running() {
            self.engine.start_audio(amp);
        } else if !want && self.engine.audio_running() {
            self.engine.stop_audio();
        }
    }

    /// Ask for a sound scan at (x, y). It starts once the speaker/microphone are running and tested.
    fn request_room_scan(&mut self, x: f32, y: f32) {
        let Some(h) = &self.house else { return };
        if in_quiet_hours(h) {
            self.status = "Sound is not played during quiet hours.".into();
        } else if self.muted {
            self.status = "Sound is muted (button at the top).".into();
        } else if !h.sound_enabled {
            self.status = "Sound sensing is off in Settings.".into();
        } else if let AudioStatus::Disabled(r) = &self.engine.audio_status {
            self.status = format!("Sound scan not done: {r}");
        } else {
            self.pending_scan = Some((x, y));
            self.force_audio_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
            self.status = "Sound scan will start when the speaker is ready. Please stay quiet.".into();
        }
    }

    fn set_paused(&mut self, p: bool) {
        self.paused = p;
        self.sync_sensing();
        if p {
            self.capture = None;
            self.status = "Paused. All sensing is stopped.".into();
        } else {
            self.status.clear();
        }
    }

    fn refresh_derived(&mut self) {
        let Some(h) = &self.house else { return };
        let Some(fl) = h.floors.get(self.floor_idx) else { return };
        let spots: usize = h.floors.iter().map(|f| f.spots.len()).sum();
        let train: usize = fl.openings.iter().map(|o| o.training.len() + 1).sum();
        let key = (self.engine.live.scans, spots, train, self.floor_idx);
        if key == self.derived.key && self.derived.barriers.is_some() {
            return;
        }
        let now = now_ms();
        let stats = self.engine.live.stats_between(now.saturating_sub(120_000), now);
        self.derived = Derived {
            key,
            fix: if h.wifi_enabled { locate(&stats, &h.floors) } else { None },
            classes: fl.openings.iter().map(|o| classify(o, &stats)).collect(),
            barriers: Some(detect_barriers(fl)),
            link_weights: analysis::monitor::link_weights(&self.engine.live, now),
            ap_fits: fit_aps(&fl.spots),
        };
    }

    fn service_capture(&mut self) {
        // Wi-Fi spot / training capture
        if let Some(c) = &self.capture {
            let now = now_ms();
            let stats: Vec<ApStat> = self.engine.live.stats_between(c.start_ms, now);
            let multi = stats.iter().filter(|a| a.n >= 2).count();
            let done = multi >= 3;
            let timeout = c.started.elapsed().as_secs() >= 90 || (self.engine.wifi_error.is_some() && stats.is_empty() && c.started.elapsed().as_secs() >= 8);
            if done || timeout {
                let c = self.capture.take().unwrap();
                let needed = if matches!(c.kind, CaptureKind::Train { .. }) { 2 } else { 3 };
                if stats.len() < needed {
                    self.status = format!("Not saved: only {} access point(s) measured (need {needed}+). {}", stats.len(), self.engine.wifi_error.clone().map_or("Try again, or check that Wi-Fi sensing is on.".to_string(), |e| format!("Wi-Fi problem: {e}")));
                    return;
                }
                let fi = self.floor_idx;
                let mut want_scan: Option<(f32, f32)> = None;
                let Some(h) = &mut self.house else { return };
                match c.kind {
                    CaptureKind::Spot { label, with_sound } => {
                        h.floors[fi].spots.push(Spot { id: new_id(), x: c.x, y: c.y, label: if label.trim().is_empty() { None } else { Some(label.trim().into()) }, taken_unix: now_unix(), aps: stats.clone() });
                        self.status = format!("Spot saved from {} access points.", stats.len());
                        if with_sound {
                            want_scan = Some((c.x, c.y));
                        }
                    }
                    CaptureKind::Train { opening_id, state } => {
                        if let Some(o) = h.floors[fi].openings.iter_mut().find(|o| o.id == opening_id) {
                            o.training.push(TrainingSegment { state, taken_unix: now_unix(), aps: stats });
                            self.status = "Recording saved.".into();
                        }
                    }
                }
                self.save();
                self.derived = Derived::default();
                if let Some((x, y)) = want_scan {
                    let keep = std::mem::take(&mut self.status);
                    self.request_room_scan(x, y);
                    self.status = format!("{keep} {}", self.status);
                }
            }
        }
        // start a requested sound scan once sound is ready
        if let Some((x, y)) = self.pending_scan {
            match self.engine.audio_status.clone() {
                AudioStatus::Ready(_) if self.engine.audio_running() && !self.engine.room_scan_active() => {
                    self.engine.begin_room_scan();
                    self.room_scan_target = Some((x, y));
                    self.pending_scan = None;
                    self.status = "Sound scan running, please stay quiet.".into();
                }
                AudioStatus::Disabled(r) => {
                    self.pending_scan = None;
                    self.status = format!("Sound scan not done: {r}");
                }
                _ => {
                    if self.force_audio_until.map_or(true, |t| std::time::Instant::now() >= t) {
                        self.pending_scan = None;
                        self.status = "Sound scan did not start: the speaker/microphone were not ready in time.".into();
                    }
                }
            }
        }
        // sound scan result
        if let Some(r) = self.engine.take_room_scan() {
            if let Some((x, y)) = self.room_scan_target.take() {
                match r {
                    Ok(echoes) => {
                        let n = echoes.len();
                        if let Some(h) = &mut self.house {
                            h.floors[self.floor_idx].room_scans.push(RoomScan { id: new_id(), x, y, taken_unix: now_unix(), echoes });
                        }
                        self.status = format!("Sound scan saved: {n} echo(es) found above the noise.");
                        self.save();
                    }
                    Err(e) => self.status = format!("Sound scan not saved: {e}"),
                }
            }
        }
    }

    fn click_plan(&mut self, x: f32, y: f32) {
        let fi = self.floor_idx;
        match self.tool {
            Tool::Spot => {
                if self.capture.is_some() {
                    return;
                }
                if !self.house.as_ref().map_or(false, |h| h.wifi_enabled) || self.paused {
                    self.status = "Wi-Fi sensing is off or paused.".into();
                    return;
                }
                let with_sound = self.wizard_sound && self.house.as_ref().map_or(false, |h| h.floors[fi].spots.is_empty());
                self.capture = Some(Capture { kind: CaptureKind::Spot { label: self.spot_label.clone(), with_sound }, x, y, start_ms: now_ms(), started: std::time::Instant::now() });
                self.spot_label.clear();
            }
            Tool::Door | Tool::Window => {
                let kind = if self.tool == Tool::Door { OpeningKind::Door } else { OpeningKind::Window };
                let id = new_id();
                if let Some(h) = &mut self.house {
                    h.floors[fi].openings.push(Opening { id: id.clone(), kind, x, y, horizontal: true, label: None, training: vec![] });
                }
                self.selected_opening = Some(id);
                self.tool = Tool::None;
                self.status = "Placed, shown as unknown until trained. Use the panel on the right to train it.".into();
                self.save();
            }
            Tool::SoundScan => {
                self.request_room_scan(x, y);
                self.tool = Tool::None;
            }
            Tool::Object(kind) => {
                let (w, d, hh) = kind.default_size();
                let id = new_id();
                if let Some(h) = &mut self.house {
                    h.floors[fi].objects.push(PlacedObject { id: id.clone(), kind, x, y, w, d, h: hh, rot_deg: 0.0, label: None });
                }
                self.selected_object = Some(id);
                self.selected_opening = None;
                self.tool = Tool::None;
                self.status = format!("{} placed. Use the panel on the right to rotate, resize or move it.", kind.name());
                self.save();
            }
            Tool::Wall => {
                match self.wall_start {
                    None => self.wall_start = Some((x, y)),
                    Some(a) => {
                        if a != (x, y) {
                            if let Some(h) = &mut self.house {
                                h.floors[fi].walls.push(UserWall { id: new_id(), a, b: (x, y) });
                            }
                            self.save();
                        }
                        self.wall_start = Some((x, y));
                    }
                }
            }
            Tool::Computer => {
                if let Some(h) = &mut self.house {
                    h.floors[fi].computer = Some((x, y));
                }
                self.tool = Tool::None;
                self.status = "This computer is now marked on the map. Movement is located relative to it.".into();
                self.save();
            }
            Tool::MoveObject => {
                if let (Some(id), Some(h)) = (self.selected_object.clone(), &mut self.house) {
                    if let Some(o) = h.floors[fi].objects.iter_mut().find(|o| o.id == id) {
                        o.x = x;
                        o.y = y;
                    }
                }
                self.tool = Tool::None;
                self.save();
            }
            Tool::None => {
                let hit_opening = self.house.as_ref().and_then(|h| {
                    h.floors[fi].openings.iter().find(|o| (o.x - x).abs() < 0.8 && (o.y - y).abs() < 0.8).map(|o| o.id.clone())
                });
                if hit_opening.is_some() {
                    self.selected_opening = hit_opening;
                    self.selected_object = None;
                } else {
                    let hit_obj = self.house.as_ref().and_then(|h| h.floors[fi].objects.iter().rev().find(|o| o.contains(x, y, 0.1)).map(|o| o.id.clone()));
                    self.selected_opening = None;
                    self.selected_object = hit_obj;
                }
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.engine.poll();
        if let Some((snr, rate)) = self.engine.new_selftest.take() {
            if let Some(h) = &mut self.house {
                h.cache.audio_self_test_unix = Some(now_unix());
                h.cache.audio_self_test_snr_db = Some(snr);
                h.cache.audio_sample_rate = Some(rate);
            }
            self.save();
        }
        self.sync_sensing();
        self.service_capture();
        self.refresh_derived();
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.wall_start = None;
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(500));

        match &self.screen {
            Screen::KeychainError(e) => {
                let e = e.clone();
                egui::CentralPanel::default().frame(egui::Frame::new().fill(Color32::WHITE)).show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(120.0);
                        ui.heading("Secure storage unavailable");
                        ui.label(format!("{e}\n\nThis app stores data encrypted with a key in your operating system's keychain.\nIt will not store anything without it (there is no unencrypted fallback)."));
                        if ui.button("Try again").clicked() {
                            self.open_store();
                        }
                    });
                });
                return;
            }
            Screen::Consent => {
                egui::CentralPanel::default().frame(egui::Frame::new().fill(Color32::WHITE)).show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(60.0);
                        ui.heading("Before you start");
                        ui.add_space(10.0);
                        ui.set_max_width(560.0);
                        ui.label("This app senses movement and breathing in your home using this computer's Wi-Fi radio and, optionally, its speaker and microphone.\n\n\
                                  Everyone who lives in or regularly visits the home should know about this and agree.\n\n\
                                  Everything stays on this computer, encrypted. The app makes no network connections, has no accounts and sends no telemetry.\n\n\
                                  Sound sensing uses ultrasound (about 18-20 kHz). It is inaudible to most adults but may bother pets such as dogs, cats and small rodents.\n\n\
                                  The Pause button stops all sensing immediately.");
                        ui.add_space(12.0);
                        ui.checkbox(&mut self.consent_check, "Everyone in my home knows about this and agrees.");
                        ui.add_space(8.0);
                        if ui.add_enabled(self.consent_check, egui::Button::new("Continue")).clicked() {
                            self.state.consent_given = true;
                            if let Some(s) = &self.store {
                                let _ = s.save_state(&self.state);
                            }
                            self.screen = Screen::Main;
                        }
                    });
                });
                return;
            }
            Screen::Main => {}
        }

        self.tick_security(ctx);
        if let Some(w) = self.pending_teach.take() {
            if let Some(f) = self.movement_features() {
                if let Some(h) = &mut self.house {
                    h.motion_samples.push(MotionSample { at_unix: now_unix(), who: w, size_db: f.size_db, speed_mps: f.speed_mps, wifi_ratio: f.wifi_ratio });
                    while h.motion_samples.len() > 300 {
                        h.motion_samples.remove(0);
                    }
                }
                self.save();
                self.status = format!("Thanks, saved as {w:?}. The more you teach it, the better it tells them apart.");
            } else {
                self.status = "That movement stopped before it could be measured.".into();
            }
        }
        self.top_bar(ctx);
        self.banner_panel(ctx);
        if self.mapped() {
            self.floor_panel(ctx);
            self.side_panel(ctx);
            self.object_panel(ctx);
            self.status_panel(ctx);
        }
        self.central(ctx);
        self.windows(ctx);
    }
}

impl App {
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").frame(egui::Frame::new().fill(Color32::WHITE).inner_margin(8.0)).show(ctx, |ui| {
            ui.horizontal(|ui| {
                if self.mapped() {
                    for (t, lbl, tip) in [(Tool::Spot, "I'm here", "Map a spot: stand somewhere and click that place"), (Tool::Door, "Door", "Place a door"), (Tool::Window, "Window", "Place a window"), (Tool::SoundScan, "Sound scan", "Scan the room you are in with sound")] {
                        if ui.selectable_label(self.tool == t, lbl).on_hover_text(tip).clicked() {
                            self.tool = if self.tool == t { Tool::None } else { t };
                        }
                    }
                    match self.away.phase(now_ms()) {
                        Phase::Disarmed => {
                            if ui.button(RichText::new("Arm: I'm leaving").color(Color32::from_rgb(30, 120, 70)).strong()).clicked() {
                                self.arm();
                            }
                        }
                        Phase::Arming(left) => {
                            if ui.button(format!("Arming... {} s (cancel)", left / 1000 + 1)).clicked() {
                                self.disarm();
                            }
                        }
                        Phase::Armed => {
                            if ui.button(RichText::new("ARMED: I'm home").color(Color32::from_rgb(190, 40, 40)).strong()).clicked() {
                                self.disarm();
                            }
                        }
                    }
                    if self.tool == Tool::Spot {
                        ui.add(egui::TextEdit::singleline(&mut self.spot_label).hint_text("name this spot (optional)").desired_width(150.0));
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(RichText::new("\u{2699}").size(18.0)).on_hover_text("Settings").clicked() {
                        self.show_settings = true;
                    }
                    let cur = self.house.as_ref().map_or("no house".to_string(), |h| h.name.clone());
                    let mut pick: Option<String> = None;
                    egui::ComboBox::from_id_salt("houses").selected_text(cur).show_ui(ui, |ui| {
                        for (id, name) in &self.houses {
                            if ui.selectable_label(self.house.as_ref().map_or(false, |h| &h.id == id), name).clicked() {
                                pick = Some(id.clone());
                            }
                        }
                        ui.separator();
                        if ui.button("Manage houses...").clicked() {
                            self.show_houses = true;
                        }
                    });
                    if let Some(id) = pick {
                        self.switch_house(&id);
                    }
                    if self.mapped() {
                        let label = if self.paused { RichText::new("Resume sensing").color(Color32::from_rgb(30, 120, 70)) } else { RichText::new("Pause sensing").color(Color32::from_rgb(170, 50, 40)) };
                        if ui.button(label).clicked() {
                            self.set_paused(!self.paused);
                        }
                        ui.toggle_value(&mut self.show_signals, "Signals");
                        let unseen = self.house.as_ref().map_or(0, |h| h.alerts.iter().filter(|a| !a.seen).count());
                        if ui.button(if unseen > 0 { format!("Alerts ({unseen})") } else { "Alerts".to_string() }).clicked() {
                            self.alerts_open = true;
                        }
                        if self.house.as_ref().map_or(false, |h| h.sound_enabled) {
                            let l = if self.muted { "Sound: muted" } else { "Sound: on" };
                            if ui.selectable_label(self.muted, l).on_hover_text("Mute sound without pausing Wi-Fi sensing (for example when a pet is near)").clicked() {
                                self.muted = !self.muted;
                            }
                        }
                    }
                });
            });
        });
    }

    fn floor_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("floors").exact_width(150.0).frame(egui::Frame::new().fill(Color32::WHITE).inner_margin(8.0)).show(ctx, |ui| {
            ui.label(RichText::new("Floors").strong());
            let mut order: Vec<usize> = (0..self.house.as_ref().unwrap().floors.len()).collect();
            order.sort_by_key(|i| std::cmp::Reverse(self.house.as_ref().unwrap().floors[*i].level));
            for i in order {
                let name = self.house.as_ref().unwrap().floors[i].name.clone();
                if ui.selectable_label(self.floor_idx == i, name).clicked() {
                    self.floor_idx = i;
                    self.selected_opening = None;
                }
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.new_floor_name).hint_text("new floor name").desired_width(110.0));
            ui.horizontal(|ui| {
                ui.label("level");
                ui.add(egui::DragValue::new(&mut self.new_floor_level));
            });
            if ui.add_enabled(!self.new_floor_name.trim().is_empty(), egui::Button::new("Add floor")).clicked() {
                let f = Floor { id: new_id(), name: self.new_floor_name.trim().into(), level: self.new_floor_level, spots: vec![], openings: vec![], room_scans: vec![], objects: vec![], walls: vec![], computer: None };
                let h = self.house.as_mut().unwrap();
                h.floors.push(f);
                self.floor_idx = h.floors.len() - 1;
                self.new_floor_name.clear();
                self.save();
            }
            ui.separator();
            ui.label(RichText::new("View").strong());
            ui.horizontal(|ui| {
                if ui.selectable_label(self.view3d, "3D").clicked() {
                    self.view3d = true;
                }
                if ui.selectable_label(!self.view3d, "Top").on_hover_text("Flat top-down plan, best for mapping").clicked() {
                    self.view3d = false;
                    self.stacked = false;
                }
            });
            if ui.checkbox(&mut self.stacked, "Stacked floors").changed() && self.stacked {
                self.view3d = true;
            }
            if self.view3d {
                ui.checkbox(&mut self.cam.cutaway, "Cut away front walls");
            }
            if self.view3d && ui.button("Reset view").clicked() {
                self.cam = view3d::Camera::default();
            }
            ui.separator();
            ui.label(RichText::new("Layout (placed by you)").strong());
            egui::ComboBox::from_id_salt("add_obj").selected_text("Add furniture...").width(112.0).show_ui(ui, |ui| {
                for k in ObjectKind::ALL {
                    if ui.selectable_label(false, k.name()).clicked() {
                        self.tool = Tool::Object(k);
                        self.wall_start = None;
                    }
                }
            });
            if ui.selectable_label(self.tool == Tool::Computer, "Mark this computer").on_hover_text("Click where this computer sits. Movement is located relative to it.").clicked() {
                self.tool = if self.tool == Tool::Computer { Tool::None } else { Tool::Computer };
                self.wall_start = None;
            }
            if ui.selectable_label(self.tool == Tool::Wall, "Draw wall").clicked() {
                self.tool = if self.tool == Tool::Wall { Tool::None } else { Tool::Wall };
                self.wall_start = None;
            }
            if self.tool == Tool::Wall {
                ui.label(RichText::new("Click corners one after another. Esc or Finish line ends the line.").small().color(Color32::from_gray(110)));
                if ui.button("Finish line").clicked() {
                    self.wall_start = None;
                }
                if ui.button("Undo last wall").clicked() {
                    let fi = self.floor_idx;
                    if let Some(h) = &mut self.house {
                        h.floors[fi].walls.pop();
                    }
                    self.save();
                }
            }
            ui.checkbox(&mut self.show_layout, "Show furniture and walls");
            ui.checkbox(&mut self.show_barriers, "Show measured barriers");
            ui.checkbox(&mut self.show_aps, "Show access points (estimated)");
        });
    }

    fn side_panel(&mut self, ctx: &egui::Context) {
        let Some(id) = self.selected_opening.clone() else { return };
        let fi = self.floor_idx;
        let Some(o) = self.house.as_ref().and_then(|h| h.floors[fi].openings.iter().find(|o| o.id == id).cloned()) else { return };
        egui::SidePanel::right("opening").exact_width(240.0).frame(egui::Frame::new().fill(Color32::WHITE).inner_margin(8.0)).show(ctx, |ui| {
            ui.label(RichText::new(if o.kind == OpeningKind::Door { "Door" } else { "Window" }).strong());
            let mut label = o.label.clone().unwrap_or_default();
            let mut horizontal = o.horizontal;
            let mut changed = false;
            changed |= ui.add(egui::TextEdit::singleline(&mut label).hint_text("name (optional)")).changed();
            changed |= ui.checkbox(&mut horizontal, "horizontal wall").changed();
            ui.separator();
            let n = |st| o.training.iter().filter(|t| t.state == st).count();
            ui.label(format!("recordings: {} closed, {} open", n(OpenState::Closed), n(OpenState::Open)));
            match untrainable_reason(&o) {
                Some(r) => ui.label(RichText::new(format!("Shown as unknown: {r}.")).color(Color32::from_gray(110))),
                None => ui.label("Trained."),
            };
            ui.label("Keep this computer where it normally sits. Hold the door/window in the state, press the button, and wait. Repeat a few times, alternating.");
            let busy = self.capture.is_some();
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy && !self.paused, egui::Button::new("Record CLOSED")).clicked() {
                    self.capture = Some(Capture { kind: CaptureKind::Train { opening_id: id.clone(), state: OpenState::Closed }, x: o.x, y: o.y, start_ms: now_ms(), started: std::time::Instant::now() });
                }
                if ui.add_enabled(!busy && !self.paused, egui::Button::new("Record OPEN")).clicked() {
                    self.capture = Some(Capture { kind: CaptureKind::Train { opening_id: id.clone(), state: OpenState::Open }, x: o.x, y: o.y, start_ms: now_ms(), started: std::time::Instant::now() });
                }
            });
            if ui.button("Clear training").clicked() {
                if let Some(h) = &mut self.house {
                    if let Some(op) = h.floors[fi].openings.iter_mut().find(|x| x.id == id) {
                        op.training.clear();
                    }
                }
                self.save();
            }
            if ui.button("Remove").clicked() {
                if let Some(h) = &mut self.house {
                    h.floors[fi].openings.retain(|x| x.id != id);
                }
                self.selected_opening = None;
                self.save();
            }
            if changed {
                if let Some(h) = &mut self.house {
                    if let Some(op) = h.floors[fi].openings.iter_mut().find(|x| x.id == id) {
                        op.label = if label.trim().is_empty() { None } else { Some(label) };
                        op.horizontal = horizontal;
                    }
                }
                self.save();
            }
        });
    }

    fn status_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status").frame(egui::Frame::new().fill(Color32::from_gray(250)).inner_margin(8.0)).show(ctx, |ui| {
            let g = Color32::from_gray(90);
            if let Some(c) = &self.capture {
                let now = now_ms();
                let stats = self.engine.live.stats_between(c.start_ms, now);
                let multi = stats.iter().filter(|a| a.n >= 2).count();
                ui.colored_label(Color32::from_rgb(160, 90, 20), format!("Hold still, measuring... {multi}/3 access points have 2+ fresh measurements ({}s). Wi-Fi scans are throttled by the OS, so this takes 20-40 s.", c.started.elapsed().as_secs()));
            }
            if !self.status.is_empty() {
                ui.label(RichText::new(&self.status).color(g));
            }
            if self.engine.mover.is_some() {
                let counts = |w: WhoLabel| self.house.as_ref().map_or(0, |h| h.motion_samples.iter().filter(|s| s.who == w).count());
                let (np, nq, no) = (counts(WhoLabel::Person), counts(WhoLabel::Pet), counts(WhoLabel::Other));
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Movement detected. Who is it? Teach the app:").color(Color32::from_rgb(170, 110, 20)));
                    for (lab, w) in [("Person", WhoLabel::Person), ("Pet", WhoLabel::Pet), ("Other", WhoLabel::Other)] {
                        if ui.small_button(lab).clicked() {
                            self.pending_teach = Some(w);
                        }
                    }
                    ui.label(RichText::new(format!("taught so far: {np} person, {nq} pet, {no} other (it answers after 4 examples of two kinds)")).small().color(g));
                });
            }
            if let Some(t) = self.guided_since {
                ui.label(RichText::new(format!("Guided walk-around ({} min so far, about 5 min total): stand in a spot, click it on the grid, hold still until saved, then move a step or two (about 1 m) and repeat. Use all areas of the floor. Spots 0.5-4 m apart are compared to find barriers.", t.elapsed().as_secs() / 60)).color(g));
                if ui.button("Finish mapping").clicked() {
                    self.guided_since = None;
                    self.tool = Tool::None;
                    self.view3d = true;
                }
            }
            if let Some(e) = &self.engine.wifi_error {
                ui.colored_label(Color32::from_rgb(160, 60, 40), format!("Wi-Fi: {e}"));
            }
            let mv = match &self.engine.movement {
                Movement::Unknown(r) => format!("house movement (Wi-Fi): -- ({r})"),
                Movement::NoUnusualVariation(c) => format!("house movement (Wi-Fi): none unusual ({:.0}%)", c * 100.0),
                Movement::Moving(c) => format!("house movement (Wi-Fi): movement somewhere in the house, location unknown ({:.0}%)", c * 100.0),
            };
            ui.label(RichText::new(mv).color(g));
            if let Some(e) = self.engine.events.last() {
                ui.label(RichText::new(format!("last lasting signal change: {} access points shifted, median {:+.1} dB", e.aps, e.median_shift_db)).color(g));
            }
            if let Some(fix) = &self.derived.fix {
                let fname = self.house.as_ref().and_then(|h| h.floors.get(fix.floor_index)).map_or("?".into(), |f| f.name.clone());
                let fc = fix.floor_confidence.map_or("only floor with matches".to_string(), |c| format!("{:.0}%", c * 100.0));
                ui.label(RichText::new(format!("this computer is probably on: {fname} (floor confidence {fc})")).color(g));
            } else {
                ui.label(RichText::new("this computer's location: -- (needs 3+ mapped spots and current Wi-Fi matches)").color(g));
            }
            if let Some(h) = &self.house {
                if h.floors.len() >= 2 {
                    let mut idx: Vec<usize> = (0..h.floors.len()).collect();
                    idx.sort_by_key(|i| h.floors[*i].level);
                    for w in idx.windows(2) {
                        let (a, b) = (&h.floors[w[0]], &h.floors[w[1]]);
                        let t = floor_loss_db(a, b).map_or("--".to_string(), |(d, n)| format!("{:+.1} dB median ({n} shared APs)", d));
                        ui.label(RichText::new(format!("signal {} minus {}: {t}", a.name, b.name)).color(g));
                    }
                }
            }
            match &self.derived.barriers {
                Some(Err(r)) => ui.label(RichText::new(format!("signal barriers: -- ({r})")).color(g)),
                Some(Ok(b)) => ui.label(RichText::new(format!("signal barriers on this floor: {}", b.len())).color(g)),
                None => ui.label(""),
            };
            let snd = match &self.engine.audio_status {
                AudioStatus::Off => format!("sound: off ({})", self.sound_off_reason),
                AudioStatus::Disabled(r) => format!("sound: disabled. {r}"),
                AudioStatus::Testing(n) => format!("sound: self-test {n}/40"),
                AudioStatus::Ready(s) => format!("sound: ready (self-test {s:.0} dB)"),
            };
            ui.label(RichText::new(snd).color(g));
            if self.house.as_ref().map_or(false, |h| h.sound_enabled && h.pet_safe) {
                ui.label(RichText::new(format!("pet-safe mode: quiet chirps, sound on {LISTEN_ON_SECS} s of every {LISTEN_CYCLE_SECS} s")).color(g));
            }
            if matches!(self.engine.audio_status, AudioStatus::Ready(_)) {
                let m = match &self.engine.sound_motion {
                    SoundMotion::Unknown(r) => format!("room motion (sound): -- ({r})"),
                    SoundMotion::Still => "room motion (sound): none".into(),
                    SoundMotion::Moving(c, r) => format!("room motion (sound): detected at about {r:.1} m ({:.0}%)", c * 100.0),
                };
                ui.label(RichText::new(m).color(g));
                if let Some((v, c)) = self.engine.vitals_out.breathing {
                    ui.label(RichText::new(format!("breathing: {v:.0}/min ({:.0}%), measured from sound", c * 100.0)).color(g));
                }
                if let Some((v, c)) = self.engine.vitals_out.heart {
                    ui.label(RichText::new(format!("heart: {v:.0} bpm, experimental ({:.0}%)", c * 100.0)).color(g));
                }
                if let Some(r) = &self.engine.vitals_out.reason {
                    ui.label(RichText::new(format!("breathing / heart: -- ({r})")).color(g));
                }
            }
        });
    }

    fn central(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().frame(egui::Frame::new().fill(Color32::WHITE)).show(ctx, |ui| {
            if !self.mapped() {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() / 2.0 - 30.0);
                    if self.wizard {
                        return;
                    }
                    if ui.add_sized([220.0, 56.0], egui::Button::new(RichText::new("Map this house").size(20.0))).clicked() {
                        if self.house.is_none() {
                            if let Some(s) = &self.store {
                                match s.create_house("Home") {
                                    Ok(h) => {
                                        self.state.active_house = Some(h.id.clone());
                                        let _ = s.save_state(&self.state);
                                        self.house = Some(h);
                                        self.refresh_houses();
                                    }
                                    Err(e) => self.status = format!("Could not create house: {e}"),
                                }
                            }
                        }
                        if self.house.is_some() {
                            self.wizard = true;
                        }
                    }
                    if !self.status.is_empty() {
                        ui.label(&self.status);
                    }
                });
                return;
            }
            let rect = ui.available_rect_before_wrap();
            let (resp, painter) = ui.allocate_painter(rect.size(), Sense::click_and_drag());
            let nfl = self.house.as_ref().unwrap().floors.len();
            let fi = self.floor_idx.min(nfl - 1);
            if self.paused {
                painter.text(rect.left_top() + egui::vec2(8.0, 8.0), Align2::LEFT_TOP, "Paused", FontId::proportional(14.0), Color32::from_rgb(170, 50, 40));
            }
            if self.stacked && !self.view3d {
                self.view3d = true;
            }

            if self.view3d {
                if resp.dragged() {
                    let dl = resp.drag_delta();
                    self.cam.yaw -= dl.x * 0.008;
                    self.cam.elev = (self.cam.elev + dl.y * 0.006).clamp(0.3, std::f32::consts::FRAC_PI_2);
                }
                if resp.hovered() {
                    let sc = ui.input(|i| i.smooth_scroll_delta.y);
                    if sc != 0.0 {
                        self.cam.zoom = (self.cam.zoom * (1.0 + sc * 0.002)).clamp(0.4, 4.0);
                    }
                }
                let region = self.view_region();
                let mut click: Option<(f32, f32)> = None;
                let mut pick_floor: Option<usize> = None;
                {
                    let h = self.house.as_ref().unwrap();
                    let mut order: Vec<usize> = (0..nfl).collect();
                    order.sort_by_key(|i| h.floors[*i].level);
                    let drawn: Vec<usize> = if self.stacked { order } else { vec![fi] };
                    let store: Vec<(usize, Vec<Barrier>, Vec<Class>)> = drawn
                        .iter()
                        .map(|&i| {
                            let bars = if !self.show_barriers { vec![] } else if i == fi { self.derived.barriers.as_ref().and_then(|b| b.as_ref().ok()).cloned().unwrap_or_default() } else { detect_barriers(&h.floors[i]).unwrap_or_default() };
                            let cls = if i == fi { self.derived.classes.clone() } else { vec![Class::Unknown; h.floors[i].openings.len()] };
                            (i, bars, cls)
                        })
                        .collect();
                    let items: Vec<view3d::FloorItem> = store
                        .iter()
                        .map(|(i, b, c)| view3d::FloorItem {
                            name: &h.floors[*i].name,
                            data: PlanData {
                                floor: &h.floors[*i],
                                barriers: b,
                                classes: c,
                                fix: if *i == fi && h.floors[*i].computer.is_none() { self.derived.fix.as_ref().filter(|f| f.floor_index == fi) } else { None },
                                blob: if *i == fi { self.blob_for(fi) } else { None },
                                selected_opening: if *i == fi { self.selected_opening.as_deref() } else { None },
                                objects: if self.show_layout { &h.floors[*i].objects } else { &[] },
                                walls: if self.show_layout { &h.floors[*i].walls } else { &[] },
                                selected_object: if *i == fi { self.selected_object.as_deref() } else { None },
                                computer: h.floors[*i].computer,
                                aps: if self.show_aps && *i == fi { &self.derived.ap_fits } else { &[] },
                                detail: true,
                                labels: !self.stacked,
                            },
                        })
                        .collect();
                    let sel_rank = drawn.iter().position(|i| *i == fi).unwrap_or(0);
                    let proj = view3d::render(&painter, rect, &self.cam, &items, self.stacked, sel_rank, region);
                    if let Some((wx, wy)) = self.wall_start {
                        painter.circle_stroke(proj.p(wx, wy, 0.0), 6.0, st(2.0, Color32::from_rgb(40, 110, 220)));
                    }
                    if let Some(c) = &self.capture {
                        let z0 = if self.stacked { sel_rank as f32 * view3d::FLOOR_GAP } else { 0.0 };
                        painter.circle_stroke(proj.p(c.x, c.y, z0), 10.0, st(2.0, Color32::from_rgb(200, 120, 30)));
                    }
                    if resp.clicked() {
                        if let Some(pos) = resp.interact_pointer_pos() {
                            if self.stacked {
                                if let Some(rank) = view3d::hit_floor(&proj, drawn.len(), pos, region) {
                                    pick_floor = Some(drawn[rank]);
                                }
                            } else {
                                let (x, y) = proj.unproject(pos, 0.0);
                                let (x, y) = ((x * 2.0).round() / 2.0, (y * 2.0).round() / 2.0);
                                if (region.0..=region.1).contains(&x) && (region.2..=region.3).contains(&y) {
                                    click = Some((x, y));
                                }
                            }
                        }
                    }
                }
                if let Some(i) = pick_floor {
                    self.floor_idx = i;
                    self.stacked = false;
                    self.selected_opening = None;
                }
                if let Some((x, y)) = click {
                    self.click_plan(x, y);
                }
                painter.text(rect.right_bottom() + egui::vec2(-8.0, -6.0), Align2::RIGHT_BOTTOM, "drag to rotate, scroll to zoom", FontId::proportional(10.0), Color32::from_gray(160));
                return;
            }

            // Top view: flat plan, best for mapping
            let h = self.house.as_ref().unwrap();
            // while mapping the whole grid stays visible; otherwise the view fits what has been measured or placed
            let reg = if self.guided_since.is_some() { (0.0, GRID_W, 0.0, GRID_H) } else { self.view_region() };
            let s = (rect.width() / (reg.1 - reg.0)).min(rect.height() / (reg.3 - reg.2)) * 0.96;
            let origin = rect.center() - egui::vec2((reg.0 + reg.1) / 2.0 * s, (reg.2 + reg.3) / 2.0 * s);
            let map = move |x: f32, y: f32| origin + egui::vec2(x * s, y * s);
            draw_grid(&painter, &map);
            let fix = if h.floors[fi].computer.is_none() { self.derived.fix.as_ref().filter(|f| f.floor_index == fi) } else { None };
            let blob = self.blob_for(fi);
            let empty = vec![];
            let bars = if self.show_barriers { self.derived.barriers.as_ref().and_then(|b| b.as_ref().ok()).unwrap_or(&empty) } else { &empty };
            draw_floor(&painter, &map, s, &PlanData { floor: &h.floors[fi], barriers: bars, classes: &self.derived.classes, fix, blob, selected_opening: self.selected_opening.as_deref(), objects: if self.show_layout { &h.floors[fi].objects } else { &[] }, walls: if self.show_layout { &h.floors[fi].walls } else { &[] }, selected_object: self.selected_object.as_deref(), computer: h.floors[fi].computer, aps: if self.show_aps { &self.derived.ap_fits } else { &[] }, detail: true, labels: true });
            if let Some((wx, wy)) = self.wall_start {
                painter.circle_stroke(map(wx, wy), 6.0, st(2.0, Color32::from_rgb(40, 110, 220)));
            }
            if let Some(c) = &self.capture {
                painter.circle_stroke(map(c.x, c.y), 9.0, st(2.0, Color32::from_rgb(200, 120, 30)));
            }
            if resp.clicked() {
                if let Some(p) = resp.interact_pointer_pos() {
                    let (x, y) = (((p.x - origin.x) / s * 2.0).round() / 2.0, ((p.y - origin.y) / s * 2.0).round() / 2.0);
                    if (0.0..=GRID_W).contains(&x) && (0.0..=GRID_H).contains(&y) {
                        self.click_plan(x, y);
                    }
                }
            }
        });
    }

    /// The part of the grid shown as the 3D floor plate: the measured area plus a margin, or the whole grid when nothing is measured yet.
    fn view_region(&self) -> view3d::Region {
        let full = (0.0, GRID_W, 0.0, GRID_H);
        let Some(h) = &self.house else { return full };
        let mut pts: Vec<(f32, f32)> = vec![];
        for f in &h.floors {
            pts.extend(f.spots.iter().map(|s| (s.x, s.y)));
            pts.extend(f.openings.iter().map(|o| (o.x, o.y)));
            pts.extend(f.room_scans.iter().map(|r| (r.x, r.y)));
            pts.extend(f.objects.iter().map(|o| (o.x, o.y)));
            for w in &f.walls {
                pts.push(w.a);
                pts.push(w.b);
            }
        }
        if pts.is_empty() {
            return full;
        }
        let (mut x0, mut x1, mut y0, mut y1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
        for (x, y) in pts {
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
        let fit = |lo: f32, hi: f32, min_len: f32, max: f32| -> (f32, f32) {
            let (mut a, mut b) = (lo - 3.0, hi + 3.0);
            if b - a < min_len {
                let c = (a + b) / 2.0;
                a = c - min_len / 2.0;
                b = c + min_len / 2.0;
            }
            if a < 0.0 {
                b -= a;
                a = 0.0;
            }
            if b > max {
                a -= b - max;
                b = max;
            }
            (a.max(0.0).floor(), b.min(max).ceil())
        };
        let (xa, xb) = fit(x0, x1, 14.0, GRID_W);
        let (ya, yb) = fit(y0, y1, 10.0, GRID_H);
        (xa, xb, ya, yb)
    }

    /// Measurements of the movement being sensed right now, for telling people from pets.
    fn movement_features(&self) -> Option<Features> {
        let (_, size, speed) = self.engine.mover?;
        let c = match &self.engine.sound_motion {
            SoundMotion::Moving(c, _) => *c,
            _ => return None,
        };
        let wsum: f32 = self.derived.link_weights.values().sum();
        Some(Features { size_db: Some(size), speed_mps: speed, wifi_ratio: if self.derived.link_weights.is_empty() { None } else { Some(wsum / c.max(0.05)) } })
    }

    fn current_who(&self) -> Option<Who> {
        let f = self.movement_features()?;
        Some(classify_who(&self.house.as_ref()?.motion_samples, &f))
    }

    /// The movement / breathing cloud for floor `fi`: a probability map built from the Wi-Fi links that are being
    /// disturbed and the sound distance. With little evidence it falls back to a ring at the sound distance.
    fn blob_for(&self, fi: usize) -> Option<Blob> {
        let h = self.house.as_ref()?;
        let fl = h.floors.get(fi)?;
        let (l, lsig) = match fl.computer {
            Some(p) => (p, 0.3),
            None => {
                let f = self.derived.fix.as_ref().filter(|f| f.floor_index == fi)?;
                ((f.x, f.y), f.radius_m)
            }
        };
        let v = &self.engine.vitals_out;
        let wifi_moving = matches!(self.engine.movement, Movement::Moving(_));
        let (sound_range, kind, _conf) = match &self.engine.sound_motion {
            SoundMotion::Moving(c, r) => (Some(*r), "movement", *c),
            _ if v.breathing.is_some() || v.heart.is_some() => (v.range_m, "periodic motion (possible breathing)", v.breathing.or(v.heart).map_or(0.1, |b| b.1)),
            _ if wifi_moving => (None, "movement (Wi-Fi only)", 0.3),
            _ => return None,
        };
        let est = personloc::estimate(l, lsig, &self.derived.ap_fits, &self.derived.link_weights, sound_range, self.view_region())?;
        let (who, height) = match self.current_who() {
            Some(Who::Person(c)) => (format!("likely person, {:.0}%", c * 100.0), 1.8),
            Some(Who::Pet(c)) => (format!("likely pet, {:.0}%", c * 100.0), 0.5),
            Some(Who::Other(c)) => (format!("likely something else, {:.0}%", c * 100.0), 1.0),
            _ => ("person, pet or air flow: unknown".to_string(), 1.2),
        };
        let sources = match (est.wifi_links, sound_range.is_some()) {
            (0, _) => "sound".to_string(),
            (n, true) => format!("{n} Wi-Fi links + sound"),
            (n, false) => format!("{n} Wi-Fi links"),
        };
        Some(Blob { x: est.cx, y: est.cy, radius_m: est.spread_m, confidence: est.quality, kind, who, height_m: height, ring: est.ring_m, cells: est.cells, sources, breathing: v.breathing, heart: v.heart })
    }

    fn windows(&mut self, ctx: &egui::Context) {
        self.alerts_window(ctx);
        // Guided mapping wizard
        if self.wizard {
            egui::Window::new("Map this house").anchor(Align2::CENTER_CENTER, [0.0, 0.0]).collapsible(false).resizable(false).show(ctx, |ui| {
                ui.label("You will carry this computer around, stand in spots, and click each spot on a blank grid (1 square = 1 m). Takes about 5 minutes. Nothing is drawn until real measurements exist.");
                ui.add(egui::TextEdit::singleline(&mut self.wizard_floor).hint_text("name of the floor you are on (e.g. Ground)"));
                ui.horizontal(|ui| {
                    ui.label("level (0 = ground, -1 = basement, 1 = first floor...)");
                    ui.add(egui::DragValue::new(&mut self.wizard_level));
                });
                ui.checkbox(&mut self.wizard_sound, "Also scan this room with sound (ultrasound; may bother pets)");
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.wizard_floor.trim().is_empty(), egui::Button::new("Start")).clicked() {
                        if let Some(h) = &mut self.house {
                            h.floors.push(Floor { id: new_id(), name: self.wizard_floor.trim().into(), level: self.wizard_level, spots: vec![], openings: vec![], room_scans: vec![], objects: vec![], walls: vec![], computer: None });
                            h.sound_enabled = self.wizard_sound;
                            self.floor_idx = h.floors.len() - 1;
                        }
                        self.engine.audio_status = AudioStatus::Off;
                        self.save();
                        self.wizard = false;
                        self.tool = Tool::Spot;
                        self.view3d = false;
                        self.stacked = false;
                        self.guided_since = Some(std::time::Instant::now());
                        self.status.clear();
                    }
                    if ui.button("Cancel").clicked() {
                        self.wizard = false;
                    }
                });
            });
        }

        // Signals table (live, real)
        if self.show_signals {
            let mut open = true;
            egui::Window::new("Live Wi-Fi signals").open(&mut open).default_width(640.0).show(ctx, |ui| {
                ui.label(format!("backend: {}", self.engine.backend));
                if self.engine.last_scan.is_empty() {
                    ui.label("--  no measurements yet");
                    return;
                }
                let mut rows = self.engine.last_scan.clone();
                rows.sort_by_key(|o| std::cmp::Reverse(o.rssi_dbm.unwrap_or(i32::MIN)));
                egui_extras::TableBuilder::new(ui)
                    .striped(true)
                    .max_scroll_height(320.0)
                    .column(egui_extras::Column::auto().at_least(150.0))
                    .column(egui_extras::Column::auto().at_least(130.0))
                    .column(egui_extras::Column::auto().at_least(70.0))
                    .column(egui_extras::Column::auto().at_least(60.0))
                    .column(egui_extras::Column::remainder())
                    .header(20.0, |mut h| {
                        for t in ["SSID", "BSSID", "RSSI dBm", "Channel", "Freq MHz"] {
                            h.col(|ui| { ui.strong(t); });
                        }
                    })
                    .body(|body| {
                        body.rows(18.0, rows.len(), |mut r| {
                            let o = &rows[r.index()];
                            r.col(|ui| { ui.label(o.ssid.as_deref().unwrap_or("--")); });
                            r.col(|ui| { ui.label(&o.bssid); });
                            r.col(|ui| { ui.label(o.rssi_dbm.map_or("--".into(), |v| v.to_string())); });
                            r.col(|ui| { ui.label(o.channel.map_or("--".into(), |v| v.to_string())); });
                            r.col(|ui| { ui.label(o.frequency_mhz.map_or("--".into(), |v| v.to_string())); });
                        });
                    });
            });
            self.show_signals = open;
        }

        // Houses
        if self.show_houses {
            let mut open = true;
            egui::Window::new("Houses").open(&mut open).show(ctx, |ui| {
                ui.label("Each house has its own encrypted data and its own key.");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.new_house_name).hint_text("new house name"));
                    if ui.add_enabled(!self.new_house_name.trim().is_empty(), egui::Button::new("Add house")).clicked() {
                        if let Some(s) = &self.store {
                            match s.create_house(&self.new_house_name) {
                                Ok(h) => {
                                    let id = h.id.clone();
                                    self.new_house_name.clear();
                                    self.refresh_houses();
                                    self.switch_house(&id);
                                }
                                Err(e) => self.status = format!("Could not create house: {e}"),
                            }
                        }
                    }
                });
                if self.house.is_some() {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.rename_buf).hint_text("rename current house to..."));
                        if ui.add_enabled(!self.rename_buf.trim().is_empty(), egui::Button::new("Rename")).clicked() {
                            if let Some(h) = &mut self.house {
                                h.name = self.rename_buf.trim().into();
                            }
                            self.rename_buf.clear();
                            self.save();
                            self.refresh_houses();
                        }
                    });
                    if ui.button("Delete current house").clicked() {
                        self.confirm = Some(Confirm::DeleteHouse);
                    }
                }
            });
            self.show_houses = open;
        }

        if self.show_settings {
            let mut open = true;
            egui::Window::new("Settings").open(&mut open).default_width(480.0).show(ctx, |ui| {
                self.settings_ui(ui);
            });
            self.show_settings = open;
        }

        if let Some(c) = self.confirm {
            let msg = match c {
                Confirm::DeleteHouse => "Delete this house and its encryption key? This cannot be undone.",
                Confirm::DeleteAll => "Delete ALL houses, keys and app data? This cannot be undone.",
                Confirm::Remap => "Remove all mapped spots and sound scans from this house so you can map it again? Floors, doors and windows stay.",
            };
            egui::Window::new("Are you sure?").anchor(Align2::CENTER_CENTER, [0.0, 0.0]).collapsible(false).resizable(false).show(ctx, |ui| {
                ui.label(msg);
                ui.horizontal(|ui| {
                    if ui.button("Yes").clicked() {
                        self.run_confirm(c);
                        self.confirm = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm = None;
                    }
                });
            });
        }
    }

    fn run_confirm(&mut self, c: Confirm) {
        match c {
            Confirm::DeleteHouse => {
                if let (Some(s), Some(h)) = (&self.store, &self.house) {
                    if let Err(e) = s.delete_house(&h.id) {
                        self.status = format!("Delete failed: {e}");
                    }
                }
                self.house = None;
                self.refresh_houses();
                if let Some((id, _)) = self.houses.first().cloned() {
                    self.switch_house(&id);
                } else {
                    self.engine.clear_live();
                    self.state.active_house = None;
                }
            }
            Confirm::DeleteAll => {
                if let Some(s) = &self.store {
                    if let Err(e) = s.delete_all() {
                        self.status = format!("Delete failed: {e}");
                    }
                }
                self.house = None;
                self.houses.clear();
                self.state = AppState::default();
                self.consent_check = false;
                self.engine.clear_live();
                self.screen = Screen::Consent;
            }
            Confirm::Remap => {
                if let Some(h) = &mut self.house {
                    for f in &mut h.floors {
                        f.spots.clear();
                        f.room_scans.clear();
                    }
                    self.tool = Tool::Spot;
                    self.view3d = false;
                    self.guided_since = Some(std::time::Instant::now());
                }
                self.derived = Derived::default();
                self.save();
            }
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        if let Some(h) = &mut self.house {
            let (mut w, mut s) = (h.wifi_enabled, h.sound_enabled);
            ui.checkbox(&mut w, "Wi-Fi sensing");
            ui.checkbox(&mut s, "Sound sensing (ultrasound; may bother pets)");
            let mut dirty = false;
            if w != h.wifi_enabled || s != h.sound_enabled {
                if s && !h.sound_enabled {
                    self.engine.audio_status = AudioStatus::Off; // allow a fresh self-test
                    self.engine.forget_selftest();
                }
                h.wifi_enabled = w;
                h.sound_enabled = s;
                dirty = true;
            }
            dirty |= ui.checkbox(&mut h.pet_safe, "Pet-safe mode (recommended)").changed();
            ui.label(RichText::new("Quieter chirps (about 10 dB lower), sound on only about a third of the time (90 s on, 150 s off), and sound scans only when you ask. This reduces exposure but cannot make ultrasound inaudible to dogs or cats, who hear well above 20 kHz. Quieter chirps can make the self-test fail on weak microphones.").small().color(Color32::from_gray(110)));
            dirty |= ui.checkbox(&mut h.quiet_hours, "Quiet hours: never play sound").changed();
            if h.quiet_hours {
                ui.horizontal(|ui| {
                    ui.label("from");
                    dirty |= ui.add(egui::DragValue::new(&mut h.quiet_start_hour).range(0..=23).suffix(":00")).changed();
                    ui.label("to");
                    dirty |= ui.add(egui::DragValue::new(&mut h.quiet_end_hour).range(0..=23).suffix(":00")).changed();
                    ui.label("(this computer's local time)");
                });
            }
            if dirty {
                self.save();
            }
            ui.horizontal(|ui| {
                if ui.button("Clear cache").on_hover_text("Forget the audio self-test result and live readings").clicked() {
                    if let Some(h) = &mut self.house {
                        h.cache = Default::default();
                    }
                    self.engine.clear_live();
                    self.engine.audio_status = AudioStatus::Off;
                    self.engine.forget_selftest();
                    self.derived = Derived::default();
                    self.save();
                }
                if ui.button("Re-map this house").clicked() {
                    self.confirm = Some(Confirm::Remap);
                }
                if ui.button("Delete this house").clicked() {
                    self.confirm = Some(Confirm::DeleteHouse);
                }
            });
        }
        if ui.button("Delete all data").clicked() {
            self.confirm = Some(Confirm::DeleteAll);
        }
        ui.separator();
        ui.label(RichText::new("Away mode").strong());
        let mut away_dirty = false;
        if let Some(h) = &mut self.house {
            egui::ComboBox::from_label("Sensitivity").selected_text(format!("{:?}", h.away.sensitivity)).show_ui(ui, |ui| {
                for s in [Sensitivity::Low, Sensitivity::Medium, Sensitivity::High] {
                    away_dirty |= ui.selectable_value(&mut h.away.sensitivity, s, format!("{s:?}")).changed();
                }
            });
            ui.horizontal(|ui| {
                ui.label("Time to leave after arming");
                away_dirty |= ui.add(egui::DragValue::new(&mut h.away.grace_secs).range(10..=900).suffix(" s")).changed();
            });
            ui.horizontal(|ui| {
                ui.label("Quiet period between alerts of the same kind");
                away_dirty |= ui.add(egui::DragValue::new(&mut h.away.cooldown_mins).range(1..=120).suffix(" min")).changed();
            });
            away_dirty |= ui.checkbox(&mut h.away.notify, "Desktop notifications (local only)").changed();
            away_dirty |= ui.checkbox(&mut h.away.ignore_pets, "Do not alert for movement the app is fairly sure is a pet (needs teaching first)").changed();
            ui.label(RichText::new("Higher sensitivity catches more but raises false alarms. Keep the computer on, plugged in and awake while armed.").small().color(Color32::from_gray(110)));
        }
        if away_dirty {
            self.save();
        }
        ui.separator();
        ui.label(RichText::new("Encrypted backup").strong());
        ui.add(egui::TextEdit::singleline(&mut self.backup_pass).password(true).hint_text("backup passphrase"));
        ui.horizontal(|ui| {
            let ok = !self.backup_pass.is_empty();
            if ui.add_enabled(ok && self.house.is_some(), egui::Button::new("Export current house...")).clicked() {
                if let Some(path) = rfd::FileDialog::new().set_file_name("house.hab").save_file() {
                    let id = self.house.as_ref().unwrap().id.clone();
                    self.status = match self.store.as_ref().unwrap().export_backup(&id, &self.backup_pass).and_then(|b| std::fs::write(&path, b).map_err(Into::into)) {
                        Ok(()) => "Backup written.".into(),
                        Err(e) => format!("Export failed: {e}"),
                    };
                }
            }
            if ui.add_enabled(ok, egui::Button::new("Import backup as new house...")).clicked() {
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    let r = std::fs::read(&path).map_err(storage::StoreError::from).and_then(|b| self.store.as_ref().unwrap().import_backup(&b, &self.backup_pass));
                    match r {
                        Ok(h) => {
                            let id = h.id.clone();
                            self.refresh_houses();
                            self.switch_house(&id);
                            self.status = "Backup imported as a new house.".into();
                        }
                        Err(e) => self.status = format!("Import failed: {e}"),
                    }
                }
            }
        });
        ui.label("The passphrase is not stored. You need it to read the backup again.");
        ui.separator();
        ui.label(RichText::new("Permissions status").strong());
        ui.label(format!("Keychain: {}", if self.store.is_some() { "working" } else { "unavailable" }));
        ui.label(format!("Wi-Fi backend: {}", self.engine.backend));
        match &self.engine.wifi_error {
            Some(e) => ui.colored_label(Color32::from_rgb(160, 60, 40), format!("Wi-Fi: {e}")),
            None => ui.label(if self.engine.last_scan.is_empty() { "Wi-Fi: no scan result yet" } else { "Wi-Fi: scans succeeding" }),
        };
        if ui.button("Check microphone and speaker").clicked() {
            self.engine.probe_audio();
        }
        if let Some(s) = &self.engine.audio_summary {
            ui.label(s);
        }
        ui.label(format!("Ultrasound self-test: {}", match &self.engine.audio_status {
            AudioStatus::Off => "not running".to_string(),
            AudioStatus::Disabled(r) => format!("failed. {r}"),
            AudioStatus::Testing(n) => format!("running ({n}/40)"),
            AudioStatus::Ready(s) => format!("passed ({s:.0} dB)"),
        }));
        if let Some(c) = self.house.as_ref().map(|h| &h.cache) {
            match (c.audio_self_test_snr_db, c.audio_sample_rate) {
                (Some(s), Some(r)) => ui.label(format!("Saved self-test result: {s:.0} dB at {r} Hz (cleared by Clear cache)")),
                _ => ui.label("Saved self-test result: none"),
            };
        }
        ui.label("macOS: Wi-Fi network names need Location permission; microphone needs Microphone permission.");
        ui.separator();
        ui.label(RichText::new("About and privacy").strong());
        ui.label("All data stays on this computer, encrypted (ChaCha20-Poly1305) with a per-house key in your OS keychain. The app makes no network connections, has no accounts and no telemetry. It contains no simulated or sample data: what is shown comes from real measurements or from things you placed. Unknown values show as -- or dashed.");
        if let Some(s) = &self.store {
            ui.label(format!("Data folder: {}", s.data_dir().display()));
        }
    }
}


fn fmt_time(unix: u64) -> String {
    chrono::DateTime::from_timestamp(unix as i64, 0).map_or("?".to_string(), |d| d.with_timezone(&chrono::Local).format("%b %d %H:%M").to_string())
}

impl App {
    fn arm(&mut self) {
        let Some(h) = &self.house else { return };
        if self.paused {
            self.status = "Resume sensing before arming.".into();
            return;
        }
        if !h.wifi_enabled {
            self.status = "Turn Wi-Fi sensing on in Settings before arming.".into();
            return;
        }
        let grace = h.away.grace_secs as u64 * 1000;
        self.away.arm(now_ms(), grace);
        self.events_seen = self.engine.events.len();
        self.banner = None;
        self.health_notified = false;
        if let Some(h) = &mut self.house {
            h.away_armed = true;
        }
        self.state.heartbeat_unix = now_unix();
        if let Some(s) = &self.store {
            let _ = s.save_state(&self.state);
        }
        self.save();
        self.status = "Arming: leave now. Alerts start when the countdown ends.".into();
    }

    fn disarm(&mut self) {
        self.away.disarm();
        self.health_msg = None;
        self.health_notified = false;
        if let Some(h) = &mut self.house {
            h.away_armed = false;
        }
        self.save();
        self.status = "Away mode is off.".into();
    }

    fn notify(&self, ctx: &egui::Context, title: &str, body: &str) {
        if self.house.as_ref().map_or(true, |h| h.away.notify) {
            let (t, b) = (title.to_string(), body.to_string());
            std::thread::spawn(move || {
                let _ = notify_rust::Notification::new().summary(&t).body(&b).show();
            });
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Critical));
    }

    fn add_alert(&mut self, ctx: &egui::Context, kind: &str, summary: &str, evidence: Vec<String>, confidence: f32) {
        self.add_alert_ex(ctx, kind, summary, evidence, confidence, false);
    }

    /// `quiet` records the alert in the log without a banner or notification (used for ignored pets).
    fn add_alert_ex(&mut self, ctx: &egui::Context, kind: &str, summary: &str, evidence: Vec<String>, confidence: f32, quiet: bool) {
        let Some(h) = &mut self.house else { return };
        h.alerts.push(AlertRecord { id: new_id(), at_unix: now_unix(), kind: kind.into(), summary: summary.into(), evidence, confidence, label: None, seen: quiet });
        while h.alerts.len() > 500 {
            h.alerts.remove(0);
        }
        self.save();
        if !quiet {
            self.banner = Some(format!("{summary} ({})", fmt_time(now_unix())));
            self.notify(ctx, "Home awareness", summary);
        }
    }

    fn health_problem(&self) -> Option<String> {
        if self.paused {
            return Some("Sensing is paused: your home is NOT being monitored.".into());
        }
        let h = self.house.as_ref()?;
        if !h.wifi_enabled {
            return Some("Wi-Fi sensing is off in Settings: Away mode cannot watch the house.".into());
        }
        if let Some(t) = self.engine.err_since {
            if t.elapsed().as_secs() >= 20 {
                return Some(format!("Wi-Fi scanning is failing ({}): your home is NOT being monitored.", self.engine.wifi_error.clone().unwrap_or_default()));
            }
        }
        match self.engine.last_scan_ok {
            Some(t) if t.elapsed().as_secs() > 90 => Some(format!("No fresh Wi-Fi readings for {} s: your home may not be monitored.", t.elapsed().as_secs())),
            None => {
                let armed = self.away.armed_at().map_or(0, |a| now_ms().saturating_sub(a));
                if armed > 60_000 { Some("No Wi-Fi readings have arrived yet: your home is NOT being monitored.".into()) } else { None }
            }
            _ => None,
        }
    }

    /// Runs every frame: records gaps, checks health, and turns live readings into alerts while armed.
    fn tick_security(&mut self, ctx: &egui::Context) {
        let wall = std::time::SystemTime::now();
        let gap = wall.duration_since(self.last_tick).map_or(0, |d| d.as_secs());
        self.last_tick = wall;
        let now = now_ms();
        if self.away.phase(now) == Phase::Disarmed {
            return;
        }
        if let Some((a, b)) = self.restore_gap.take() {
            self.add_alert(ctx, "monitoring-gap", "Monitoring was interrupted", vec![format!("Away mode was armed, but the app was not running from {} to {}", fmt_time(a), fmt_time(b))], 1.0);
        }
        if gap > 60 {
            self.add_alert(ctx, "monitoring-gap", "Monitoring was interrupted", vec![format!("The app was not running for about {} min (computer asleep or app frozen)", gap / 60)], 1.0);
        }
        if self.last_heartbeat.elapsed().as_secs() >= 30 {
            self.last_heartbeat = std::time::Instant::now();
            self.state.heartbeat_unix = now_unix();
            if let Some(s) = &self.store {
                let _ = s.save_state(&self.state);
            }
        }
        // health: never silently stop protecting
        self.health_msg = self.health_problem();
        if let Some(msg) = self.health_msg.clone() {
            if !self.health_notified {
                self.health_notified = true;
                self.add_alert(ctx, "monitoring-problem", "Away mode is not protecting your home", vec![msg], 1.0);
            }
            return;
        }
        self.health_notified = false;

        // classes of every trained door/window on every floor, refreshed when a new scan arrives
        if self.away_classes_scan != self.engine.live.scans {
            self.away_classes_scan = self.engine.live.scans;
            let stats = self.engine.live.stats_between(now.saturating_sub(120_000), now);
            self.away_classes = self.house.as_ref().map_or(vec![], |h| {
                h.floors.iter().flat_map(|f| f.openings.iter().map(|o| (o.id.clone(), o.label.clone().unwrap_or_else(|| if o.kind == OpeningKind::Door { "Door".into() } else { "Window".into() }), classify(o, &stats)))).collect()
            });
        }
        let start = self.events_seen.min(self.engine.events.len());
        let new_events = self.engine.events[start..].to_vec();
        self.events_seen = self.engine.events.len();
        let sound_on = self.house.as_ref().map_or(false, |h| h.sound_enabled) && !self.muted;
        let ops: Vec<OpeningState> = self.away_classes.iter().map(|(id, name, c)| OpeningState { id, name, class: *c }).collect();
        let settings = self.house.as_ref().map(|h| h.away.clone()).unwrap_or_default();
        let alerts = self.away.update(
            &Inputs { now_ms: now, scan_seq: self.engine.live.scans, movement: &self.engine.movement, new_changes: &new_events, sound_ready: sound_on && matches!(self.engine.audio_status, AudioStatus::Ready(_)), sound: &self.engine.sound_motion, openings: &ops },
            &settings,
        );
        let who = self.current_who();
        let ignore_pets = settings.ignore_pets;
        for a in alerts {
            let mut summary = a.summary.clone();
            let mut quiet = false;
            if a.kind == "movement" || a.kind == "sound-motion" {
                match &who {
                    Some(Who::Pet(c)) if ignore_pets && *c >= 0.7 => {
                        quiet = true;
                        summary = format!("{summary} (likely pet, ignored)");
                    }
                    Some(Who::Pet(c)) => summary = format!("{summary} (likely pet, {:.0}%)", c * 100.0),
                    Some(Who::Person(c)) => summary = format!("{summary} (likely person, {:.0}%)", c * 100.0),
                    _ => {}
                }
            }
            self.add_alert_ex(ctx, a.kind, &summary, a.evidence, a.confidence, quiet);
        }
    }

    fn protection_summary(&self) -> String {
        let mv = match &self.engine.movement {
            analysis::monitor::Movement::Unknown(_) => "Wi-Fi movement: still learning, NOT protecting yet",
            _ => "Wi-Fi movement: watching",
        };
        let snd = match (self.house.as_ref().map_or(false, |h| h.sound_enabled), &self.engine.audio_status) {
            (false, _) => "sound: off".to_string(),
            (true, AudioStatus::Ready(_)) => "sound motion: watching".to_string(),
            (true, _) => "sound motion: not ready".to_string(),
        };
        let trained = self.away_classes.iter().filter(|(_, _, c)| !matches!(c, analysis::openings::Class::Unknown)).count();
        let total: usize = self.house.as_ref().map_or(0, |h| h.floors.iter().map(|f| f.openings.len()).sum());
        format!("{mv} | lasting changes: watching | {snd} | doors/windows: {trained} of {total} readable")
    }

    fn banner_panel(&mut self, ctx: &egui::Context) {
        let phase = self.away.phase(now_ms());
        if phase == Phase::Disarmed && self.banner.is_none() {
            return;
        }
        let (alert, health) = (self.banner.clone(), self.health_msg.clone());
        egui::TopBottomPanel::top("banner").frame(egui::Frame::new().fill(Color32::from_rgb(250, 246, 240)).inner_margin(8.0)).show(ctx, |ui| {
            if let Some(a) = alert {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("ALERT: {a}")).color(Color32::from_rgb(190, 40, 40)).strong());
                    if ui.button("View alerts").clicked() {
                        self.alerts_open = true;
                    }
                    if ui.button("Dismiss").clicked() {
                        self.banner = None;
                    }
                });
            }
            if let Some(h) = health {
                ui.label(RichText::new(h).color(Color32::from_rgb(180, 90, 20)).strong());
            }
            match phase {
                Phase::Arming(left) => {
                    ui.label(RichText::new(format!("Arming: leave now. Alerts start in {} s. Doors and windows that are open now will not trigger alerts.", left / 1000 + 1)).color(Color32::from_gray(90)));
                }
                Phase::Armed => {
                    let since = self.away.armed_at().map_or(0, |a| a / 1000);
                    ui.label(RichText::new(format!("Armed since {}. {}", fmt_time(since), self.protection_summary())).color(Color32::from_rgb(30, 120, 70)));
                }
                Phase::Disarmed => {}
            }
        });
    }

    fn alerts_window(&mut self, ctx: &egui::Context) {
        if !self.alerts_open {
            return;
        }
        let mut open = true;
        let mut label_change: Option<(String, Option<AlertLabel>)> = None;
        let mut clear = false;
        egui::Window::new("Alerts").open(&mut open).default_width(520.0).default_height(460.0).show(ctx, |ui| {
            let Some(h) = &self.house else { return };
            let (total, real, fals) = (h.alerts.len(), h.alerts.iter().filter(|a| a.label == Some(AlertLabel::Real)).count(), h.alerts.iter().filter(|a| matches!(a.label, Some(AlertLabel::FalseAlarm(_)))).count());
            ui.label(RichText::new(format!("{total} alerts: {real} marked real, {fals} marked false, {} not yet judged. Marking them lets you see how reliable alerts are in your home.", total - real - fals)).small().color(Color32::from_gray(100)));
            if ui.button("Clear alert history").clicked() {
                clear = true;
            }
            ui.separator();
            if h.alerts.is_empty() {
                ui.label("No alerts yet.");
            }
            egui::ScrollArea::vertical().show(ui, |ui| {
                for a in h.alerts.iter().rev() {
                    ui.group(|ui| {
                        ui.label(RichText::new(format!("{}  {}", fmt_time(a.at_unix), a.summary)).strong());
                        ui.label(RichText::new(format!("type: {}   confidence: {:.0}%", a.kind, a.confidence * 100.0)).small().color(Color32::from_gray(110)));
                        for e in &a.evidence {
                            ui.label(RichText::new(format!("- {e}")).small());
                        }
                        match &a.label {
                            Some(AlertLabel::Real) => {
                                ui.label("You marked this: real event");
                            }
                            Some(AlertLabel::FalseAlarm(why)) => {
                                ui.label(format!("You marked this: false alarm ({why})"));
                            }
                            None => {
                                ui.horizontal(|ui| {
                                    if ui.small_button("Real event").clicked() {
                                        label_change = Some((a.id.clone(), Some(AlertLabel::Real)));
                                    }
                                    ui.label("False alarm:");
                                    for why in ["pet", "fan or AC", "family", "other"] {
                                        if ui.small_button(why).clicked() {
                                            label_change = Some((a.id.clone(), Some(AlertLabel::FalseAlarm(why.into()))));
                                        }
                                    }
                                });
                            }
                        }
                    });
                }
            });
        });
        let mut changed = clear || label_change.is_some();
        if let Some(h) = &self.house {
            changed |= h.alerts.iter().any(|a| !a.seen);
        }
        if let Some(h) = &mut self.house {
            for a in &mut h.alerts {
                a.seen = true;
            }
            if let Some((id, lab)) = label_change {
                if let Some(a) = h.alerts.iter_mut().find(|a| a.id == id) {
                    a.label = lab;
                }
            }
            if clear {
                h.alerts.clear();
            }
        }
        if changed {
            self.save();
        }
        self.alerts_open = open;
    }

    fn object_panel(&mut self, ctx: &egui::Context) {
        let Some(id) = self.selected_object.clone() else { return };
        let fi = self.floor_idx;
        let Some(o) = self.house.as_ref().and_then(|h| h.floors.get(fi)).and_then(|f| f.objects.iter().find(|o| o.id == id).cloned()) else { return };
        let mut e = o.clone();
        let mut label = e.label.clone().unwrap_or_default();
        let mut delete = false;
        let mut dup = false;
        egui::SidePanel::right("object").exact_width(240.0).frame(egui::Frame::new().fill(Color32::WHITE).inner_margin(8.0)).show(ctx, |ui| {
            ui.label(RichText::new(format!("{} (placed by you)", o.kind.name())).strong());
            ui.add(egui::TextEdit::singleline(&mut label).hint_text("name (optional)"));
            ui.horizontal(|ui| {
                ui.label("width");
                ui.add(egui::DragValue::new(&mut e.w).speed(0.05).range(0.1..=8.0).suffix(" m"));
            });
            ui.horizontal(|ui| {
                ui.label("depth");
                ui.add(egui::DragValue::new(&mut e.d).speed(0.05).range(0.1..=8.0).suffix(" m"));
            });
            ui.horizontal(|ui| {
                ui.label("height");
                ui.add(egui::DragValue::new(&mut e.h).speed(0.05).range(0.05..=3.0).suffix(" m"));
            });
            ui.horizontal(|ui| {
                ui.label("rotation");
                ui.add(egui::DragValue::new(&mut e.rot_deg).speed(1.0).range(-360.0..=360.0).suffix(" deg"));
                if ui.small_button("-15").clicked() {
                    e.rot_deg -= 15.0;
                }
                if ui.small_button("+15").clicked() {
                    e.rot_deg += 15.0;
                }
            });
            ui.label("Move (0.5 m steps):");
            ui.horizontal(|ui| {
                if ui.small_button("left").clicked() {
                    e.x -= 0.5;
                }
                if ui.small_button("right").clicked() {
                    e.x += 0.5;
                }
                if ui.small_button("up").clicked() {
                    e.y -= 0.5;
                }
                if ui.small_button("down").clicked() {
                    e.y += 0.5;
                }
            });
            if ui.button("Move to next click").clicked() {
                self.tool = Tool::MoveObject;
            }
            ui.horizontal(|ui| {
                if ui.button("Duplicate").clicked() {
                    dup = true;
                }
                if ui.button("Delete").clicked() {
                    delete = true;
                }
            });
            ui.label(RichText::new("Furniture and walls are drawn only where you put them. The app cannot see them.").small().color(Color32::from_gray(110)));
        });
        e.label = if label.trim().is_empty() { None } else { Some(label) };
        if let Some(h) = &mut self.house {
            let objs = &mut h.floors[fi].objects;
            if delete {
                objs.retain(|x| x.id != id);
                self.selected_object = None;
            } else if dup {
                let mut c = e.clone();
                c.id = new_id();
                c.x += 0.5;
                c.y += 0.5;
                self.selected_object = Some(c.id.clone());
                objs.push(c);
            } else if let Some(slot) = objs.iter_mut().find(|x| x.id == id) {
                let changed = slot.w != e.w || slot.d != e.d || slot.h != e.h || slot.rot_deg != e.rot_deg || slot.x != e.x || slot.y != e.y || slot.label != e.label;
                if changed {
                    *slot = e;
                } else {
                    return;
                }
            }
        }
        self.save();
    }
}

fn main() -> eframe::Result {
    eframe::run_native(
        "Home awareness",
        eframe::NativeOptions {
            viewport: {
                let mut v = egui::ViewportBuilder::default().with_inner_size([1280.0, 780.0]).with_app_id("home-awareness");
                if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
                    v = v.with_icon(icon);
                }
                v
            },
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
