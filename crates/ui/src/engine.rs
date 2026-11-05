// SPDX-License-Identifier: AGPL-3.0-or-later
//! Live sensing: Wi-Fi scanner thread + ultrasonic audio session + analyzers.
//! Everything here comes from real hardware. When a sensor is off or unavailable the fields stay empty.

use analysis::{
    monitor::{ChangeDetector, ChangeEvent, Movement},
    sound::{MotionDetector, RoomScanAccumulator, SoundMotion, Vitals, VitalsEstimator},
    wifi::LiveStore,
};
use audio::{AudioSession, SELF_TEST_MIN_SNR_DB};
use scanner::{default_scanner, Observation};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};
use storage::Echo;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

enum ScanMsg {
    Scan(Vec<Observation>),
    Error(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum AudioStatus {
    Off,
    Disabled(String),
    Testing(usize),
    Ready(f32),
}

pub struct Engine {
    rx: Receiver<ScanMsg>,
    idle: Arc<AtomicBool>,
    pub backend: &'static str,
    pub live: LiveStore,
    pub last_scan: Vec<Observation>,
    pub wifi_error: Option<String>,
    pub movement: Movement,
    change: ChangeDetector,
    pub events: Vec<ChangeEvent>,

    session: Option<AudioSession>,
    pub audio_status: AudioStatus,
    test_snrs: Vec<f32>,
    motion: MotionDetector,
    vitals: VitalsEstimator,
    pub sound_motion: SoundMotion,
    /// While sound movement is detected: (distance m, echo size dB, speed m/s if known).
    pub mover: Option<(f32, f32, Option<f32>)>,
    pub vitals_out: Vitals,
    room_scan: Option<(RoomScanAccumulator, Instant)>,
    pub audio_summary: Option<String>,
    pub self_test_snr: Option<f32>,
    last_vitals: Instant,
    /// Set once when a self-test passes: (snr dB, sample rate). The app saves it to the house cache.
    pub new_selftest: Option<(f32, u32)>,
    /// (amplitude, snr) of a self-test that passed in this app run; lets later listening windows skip re-testing.
    tested: Option<(f32, f32)>,
    amplitude: f32,
    /// When the last Wi-Fi scan succeeded.
    pub last_scan_ok: Option<Instant>,
    /// Since when Wi-Fi scans have been failing (None while they succeed).
    pub err_since: Option<Instant>,
}

impl Engine {
    pub fn new(ctx: &eframe::egui::Context) -> Self {
        let (tx, rx) = channel();
        let idle = Arc::new(AtomicBool::new(true)); // sensing starts only when the app says so
        let mut scanner = default_scanner();
        let backend = scanner.backend_name();
        let (flag, ctx) = (idle.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut last: Option<Instant> = None;
            loop {
                let ready = last.map_or(true, |t| t.elapsed() >= scanner.min_interval());
                if flag.load(Ordering::Relaxed) || !ready {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                let msg = match scanner.scan() {
                    Ok(v) => ScanMsg::Scan(v),
                    Err(e) => ScanMsg::Error(e.to_string()),
                };
                last = Some(Instant::now());
                if tx.send(msg).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        Self {
            rx, idle, backend,
            live: LiveStore::default(), last_scan: vec![], wifi_error: None,
            movement: Movement::Unknown("no Wi-Fi measurements yet".into()),
            change: ChangeDetector::default(), events: vec![],
            session: None, audio_status: AudioStatus::Off, test_snrs: vec![],
            motion: MotionDetector::new(), vitals: VitalsEstimator::new(),
            sound_motion: SoundMotion::Unknown("sound sensing off".into()), mover: None,
            vitals_out: Vitals::default(), room_scan: None, audio_summary: None, self_test_snr: None, last_vitals: Instant::now(), new_selftest: None, tested: None, amplitude: 0.0, last_scan_ok: None, err_since: None,
        }
    }

    /// Turn Wi-Fi scanning on/off. Takes effect before the next scan.
    pub fn set_wifi_active(&mut self, on: bool) {
        self.idle.store(!on, Ordering::Relaxed);
    }

    pub fn probe_audio(&mut self) {
        self.audio_summary = Some(match audio::probe() {
            Ok(c) => format!("microphone: {} ({} channel(s)); speaker: {}; common sample rate {} Hz", c.input_name, c.input_channels, c.output_name, c.sample_rate),
            Err(e) => format!("not available: {e}"),
        });
    }

    pub fn audio_running(&self) -> bool {
        self.session.is_some()
    }

    pub fn amplitude_in_use(&self) -> f32 {
        self.amplitude
    }

    /// Forget a passed self-test so the next session runs it again.
    pub fn forget_selftest(&mut self) {
        self.tested = None;
    }

    pub fn start_audio(&mut self, amplitude: f32) {
        if self.session.is_some() {
            return;
        }
        match AudioSession::start(amplitude) {
            Ok(s) => {
                self.session = Some(s);
                self.amplitude = amplitude;
                self.audio_status = match self.tested {
                    Some((a, snr)) if (a - amplitude).abs() < 1e-3 => AudioStatus::Ready(snr),
                    _ => AudioStatus::Testing(0),
                };
                self.test_snrs.clear();
                self.motion = MotionDetector::new();
                self.vitals = VitalsEstimator::new();
                self.sound_motion = SoundMotion::Unknown(if matches!(self.audio_status, AudioStatus::Ready(_)) { "learning the room's echo pattern".into() } else { "self-test running".to_string() });
                self.vitals_out = Vitals::default();
            }
            Err(e) => self.audio_status = AudioStatus::Disabled(e.to_string()),
        }
    }

    /// Stops the speaker and microphone immediately.
    pub fn stop_audio(&mut self) {
        self.session = None;
        self.room_scan = None;
        if !matches!(self.audio_status, AudioStatus::Disabled(_)) {
            self.audio_status = AudioStatus::Off;
        }
        self.sound_motion = SoundMotion::Unknown("sound sensing off".into());
        self.mover = None;
        self.vitals_out = Vitals::default();
    }

    pub fn clear_live(&mut self) {
        self.live.clear();
        self.last_scan.clear();
        self.events.clear();
        self.change = ChangeDetector::default();
        self.movement = Movement::Unknown("no Wi-Fi measurements yet".into());
    }

    pub fn begin_room_scan(&mut self) {
        self.room_scan = Some((RoomScanAccumulator::new(), Instant::now()));
    }

    pub fn room_scan_active(&self) -> bool {
        self.room_scan.is_some()
    }

    /// Some(result) when a room scan finished (Ok(echoes) or Err(reason)); None while still running.
    pub fn take_room_scan(&mut self) -> Option<Result<Vec<Echo>, String>> {
        let (acc, started) = self.room_scan.as_ref()?;
        if acc.frames() >= 60 {
            let r = acc.finish(60);
            self.room_scan = None;
            return Some(r);
        }
        if started.elapsed() > Duration::from_secs(25) {
            let r = acc.finish(60);
            self.room_scan = None;
            return Some(r);
        }
        None
    }

    pub fn poll(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                ScanMsg::Scan(v) => {
                    self.last_scan_ok = Some(Instant::now());
                    self.err_since = None;
                    self.live.push(&v);
                    self.last_scan = v;
                    self.wifi_error = None;
                    let now = now_ms();
                    self.movement = analysis::monitor::movement(&self.live, now);
                    if let Some(ev) = self.change.update(&self.live, now) {
                        self.events.push(ev);
                    }
                }
                ScanMsg::Error(e) => {
                    self.err_since.get_or_insert(Instant::now());
                    self.wifi_error = Some(e);
                }
            }
        }
        let mut frames = vec![];
        if let Some(s) = &self.session {
            while let Ok(f) = s.rx.try_recv() {
                frames.push(f);
            }
        }
        for f in frames {
            if let AudioStatus::Testing(n) = self.audio_status {
                self.test_snrs.push(f.direct_snr_db);
                self.audio_status = AudioStatus::Testing(n + 1);
                if n + 1 >= 40 {
                    let mut v = self.test_snrs.clone();
                    v.sort_by(|a, b| a.total_cmp(b));
                    let med = v[v.len() / 2];
                    self.self_test_snr = Some(med);
                    if med >= SELF_TEST_MIN_SNR_DB {
                        self.audio_status = AudioStatus::Ready(med);
                        self.tested = Some((self.amplitude, med));
                        self.new_selftest = Some((med, self.session.as_ref().map_or(0, |s| s.caps.sample_rate)));
                    } else {
                        self.session = None;
                        self.audio_status = AudioStatus::Disabled(audio::AudioError::SelfTest(med, SELF_TEST_MIN_SNR_DB).to_string());
                        self.sound_motion = SoundMotion::Unknown("sound sensing disabled".into());
                        return;
                    }
                }
                continue;
            }
            if let Some((acc, _)) = &mut self.room_scan {
                acc.push(&f.profile, f.wall_m_per_bin, f.direct_snr_db, SELF_TEST_MIN_SNR_DB);
            }
            self.sound_motion = self.motion.update(&f.profile, f.wall_m_per_bin, f.period_s);
            self.mover = self.motion.mover();
            let moving = matches!(self.sound_motion, SoundMotion::Moving(..));
            self.vitals.push(&f.iq, f.wall_m_per_bin, f.direct_snr_db, f.period_s, moving, f.epoch);
        }
        if matches!(self.audio_status, AudioStatus::Ready(_)) && self.last_vitals.elapsed() >= Duration::from_secs(2) {
            self.last_vitals = Instant::now();
            self.vitals_out = self.vitals.estimate();
        }
    }
}
