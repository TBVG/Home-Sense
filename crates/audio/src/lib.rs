// SPDX-License-Identifier: AGPL-3.0-or-later
//! Ultrasonic echo ranging with the computer's own speaker and microphone (cpal).
//!
//! A linear chirp (18-20 kHz) is played repeatedly with a fixed period. Because the
//! excitation is periodic, circular cross-correlation of one period of microphone
//! signal with the chirp gives the room's impulse response, whatever the start offset.
//! The strongest peak is the speaker-to-microphone direct path; everything after it
//! is echo, measured as extra path length. A wall at distance D adds about 2D.
//!
//! Nothing here is simulated: if the hardware cannot play/record these frequencies,
//! the self-test fails and sound sensing is disabled with a reason.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SampleRate, SizedSample, Stream};
use rustfft::{num_complex::Complex32, FftPlanner};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

pub const SPEED_OF_SOUND: f32 = 343.0; // m/s at about 20 C; real speed varies with temperature
const F0: f32 = 18_000.0;
const F1: f32 = 20_000.0;
const CHIRP_SECS: f32 = 0.008;
const PERIOD_SECS: f32 = 0.060;
/// Chirp level (fraction of full scale) in normal mode.
pub const NORMAL_AMPLITUDE: f32 = 0.4;
/// Chirp level in pet-safe mode: about 10 dB quieter. Actual loudness also depends on the system volume.
pub const PET_SAFE_AMPLITUDE: f32 = 0.12;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no microphone (input device) found")]
    NoInput,
    #[error("no speaker (output device) found")]
    NoOutput,
    #[error("microphone and speaker share no sample rate of 44.1 kHz or higher, so ultrasound is not possible")]
    NoCommonRate,
    #[error("audio stream error: {0}")]
    Stream(String),
    #[error("ultrasound self-test failed: direct-path signal was only {0:.1} dB above noise (needs {1:.0} dB). The OS audio processing or the hardware likely blocks ultrasound, or volume is too low")]
    SelfTest(f32, f32),
}

#[derive(Debug, Clone)]
pub struct AudioCaps {
    pub sample_rate: u32,
    pub input_name: String,
    pub output_name: String,
    /// How many channels the microphone offers. Two or more (a mic array) could one day give a direction from sound alone.
    pub input_channels: u16,
}

/// One period of echo data.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Envelope of the matched-filter output. Index 0 is the direct path; index k is k bins of extra path.
    pub profile: Vec<f32>,
    /// Complex echo profile (I, Q) with the direct path as phase reference (index 0). The phase of an
    /// echo changes with its path length: 4*pi*distance/wavelength, which is how small movements are tracked.
    pub iq: Vec<(f32, f32)>,
    /// Changes when the reference lag had to be re-locked; phase continuity is broken at that point.
    pub epoch: u32,
    /// Wall distance represented by one bin (extra path / 2), in metres.
    pub wall_m_per_bin: f32,
    /// Direct-path peak vs median of the profile, in dB.
    pub direct_snr_db: f32,
    pub period_s: f32,
}

pub const SELF_TEST_MIN_SNR_DB: f32 = 15.0;

/// Look at the default devices and find a sample rate both support. Does not make sound.
pub fn probe() -> Result<AudioCaps, AudioError> {
    let host = cpal::default_host();
    let inp = host.default_input_device().ok_or(AudioError::NoInput)?;
    let out = host.default_output_device().ok_or(AudioError::NoOutput)?;
    let rate = pick_rate(&inp, &out).ok_or(AudioError::NoCommonRate)?;
    Ok(AudioCaps {
        sample_rate: rate,
        input_name: inp.name().unwrap_or_else(|_| "unknown input".into()),
        output_name: out.name().unwrap_or_else(|_| "unknown output".into()),
        input_channels: inp.supported_input_configs().ok().and_then(|c| c.map(|x| x.channels()).max()).unwrap_or(1),
    })
}

fn pick_rate(inp: &cpal::Device, out: &cpal::Device) -> Option<u32> {
    let ir: Vec<_> = inp.supported_input_configs().ok()?.collect();
    let or: Vec<_> = out.supported_output_configs().ok()?.collect();
    [48_000u32, 44_100, 96_000, 88_200, 192_000].into_iter().find(|r| {
        ir.iter().any(|c| c.min_sample_rate().0 <= *r && *r <= c.max_sample_rate().0)
            && or.iter().any(|c| c.min_sample_rate().0 <= *r && *r <= c.max_sample_rate().0)
    })
}

fn chirp(fs: f32) -> Vec<f32> {
    let n = (CHIRP_SECS * fs) as usize;
    let k = (F1 - F0) / CHIRP_SECS;
    (0..n)
        .map(|i| {
            let t = i as f32 / fs;
            let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (n as f32 - 1.0)).cos(); // Hann
            w * (2.0 * std::f32::consts::PI * (F0 * t + 0.5 * k * t * t)).sin()
        })
        .collect()
}

pub struct AudioSession {
    _out: Stream,
    _inp: Stream,
    pub rx: Receiver<Frame>,
    stop: Arc<AtomicBool>,
    pub caps: AudioCaps,
}

impl Drop for AudioSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl AudioSession {
    /// Start playing chirps and processing the microphone. Dropping the session stops everything at once.
    pub fn start(amplitude: f32) -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let inp = host.default_input_device().ok_or(AudioError::NoInput)?;
        let out = host.default_output_device().ok_or(AudioError::NoOutput)?;
        let rate = pick_rate(&inp, &out).ok_or(AudioError::NoCommonRate)?;
        let fs = rate as f32;

        let icfg = inp
            .supported_input_configs().map_err(|e| AudioError::Stream(e.to_string()))?
            .find(|c| c.min_sample_rate().0 <= rate && rate <= c.max_sample_rate().0)
            .ok_or(AudioError::NoCommonRate)?
            .with_sample_rate(SampleRate(rate));
        let ocfg = out
            .supported_output_configs().map_err(|e| AudioError::Stream(e.to_string()))?
            .find(|c| c.min_sample_rate().0 <= rate && rate <= c.max_sample_rate().0)
            .ok_or(AudioError::NoCommonRate)?
            .with_sample_rate(SampleRate(rate));

        let period = (PERIOD_SECS * fs).round() as usize;
        let mut one = vec![0.0f32; period];
        let c = chirp(fs);
        for (i, v) in c.iter().enumerate() {
            one[i] = *v * amplitude;
        }

        let (stx, srx) = channel::<Vec<f32>>();
        let (ftx, frx) = channel::<Frame>();
        let stop = Arc::new(AtomicBool::new(false));

        let out_stream = build_out(&out, &ocfg, one)?;
        let in_stream = build_in(&inp, &icfg, stx)?;
        out_stream.play().map_err(|e| AudioError::Stream(e.to_string()))?;
        in_stream.play().map_err(|e| AudioError::Stream(e.to_string()))?;

        let st = stop.clone();
        std::thread::spawn(move || process(srx, ftx, c, period, fs, st));

        Ok(Self {
            _out: out_stream,
            _inp: in_stream,
            rx: frx,
            stop,
            caps: AudioCaps {
                sample_rate: rate,
                input_name: inp.name().unwrap_or_default(),
                output_name: out.name().unwrap_or_default(),
                input_channels: icfg.channels(),
            },
        })
    }
}

fn build_out(dev: &cpal::Device, cfg: &cpal::SupportedStreamConfig, one: Vec<f32>) -> Result<Stream, AudioError> {
    fn mk<T: SizedSample + FromSample<f32>>(dev: &cpal::Device, c: &cpal::StreamConfig, one: Vec<f32>) -> Result<Stream, AudioError> {
        let ch = c.channels as usize;
        let mut pos = 0usize;
        dev.build_output_stream(
            c,
            move |data: &mut [T], _| {
                for frame in data.chunks_mut(ch) {
                    let v = T::from_sample(one[pos]);
                    pos = (pos + 1) % one.len();
                    for s in frame {
                        *s = v;
                    }
                }
            },
            |e| eprintln!("audio output error: {e}"),
            None,
        )
        .map_err(|e| AudioError::Stream(e.to_string()))
    }
    let c = cfg.config();
    match cfg.sample_format() {
        SampleFormat::F32 => mk::<f32>(dev, &c, one),
        SampleFormat::I16 => mk::<i16>(dev, &c, one),
        SampleFormat::U16 => mk::<u16>(dev, &c, one),
        f => Err(AudioError::Stream(format!("unsupported output sample format {f:?}"))),
    }
}

fn build_in(dev: &cpal::Device, cfg: &cpal::SupportedStreamConfig, tx: Sender<Vec<f32>>) -> Result<Stream, AudioError> {
    fn mk<T: SizedSample>(dev: &cpal::Device, c: &cpal::StreamConfig, tx: Sender<Vec<f32>>) -> Result<Stream, AudioError>
    where
        f32: FromSample<T>,
    {
        let ch = c.channels as usize;
        dev.build_input_stream(
            c,
            move |data: &[T], _| {
                let mono: Vec<f32> = data.chunks(ch).map(|f| f.iter().map(|s| f32::from_sample(*s)).sum::<f32>() / ch as f32).collect();
                let _ = tx.send(mono);
            },
            |e| eprintln!("audio input error: {e}"),
            None,
        )
        .map_err(|e| AudioError::Stream(e.to_string()))
    }
    let c = cfg.config();
    match cfg.sample_format() {
        SampleFormat::F32 => mk::<f32>(dev, &c, tx),
        SampleFormat::I16 => mk::<i16>(dev, &c, tx),
        SampleFormat::U16 => mk::<u16>(dev, &c, tx),
        f => Err(AudioError::Stream(format!("unsupported input sample format {f:?}"))),
    }
}

fn process(rx: Receiver<Vec<f32>>, tx: Sender<Frame>, chirp: Vec<f32>, n: usize, fs: f32, stop: Arc<AtomicBool>) {
    let mut planner = FftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(n);
    let inv = planner.plan_fft_inverse(n);
    // Matched filter spectrum, analytic (positive frequencies only, doubled) so the result gives an envelope.
    let mut h: Vec<Complex32> = (0..n).map(|i| Complex32::new(*chirp.get(i).unwrap_or(&0.0), 0.0)).collect();
    fwd.process(&mut h);
    for v in h.iter_mut() {
        *v = v.conj();
    }
    let mut buf: Vec<f32> = Vec::new();
    let mut to_skip = (0.6 * fs) as usize; // discard start-up transients (gain control settling)
    let k_max = (n as f32 * 0.8) as usize;
    let mut ref_lag: Option<usize> = None;
    let mut epoch: u32 = 0;
    while !stop.load(Ordering::Relaxed) {
        let Ok(chunk) = rx.recv_timeout(std::time::Duration::from_millis(100)) else { continue };
        let skip = to_skip.min(chunk.len());
        to_skip -= skip;
        buf.extend_from_slice(&chunk[skip..]);
        while buf.len() >= n {
            let block: Vec<f32> = buf.drain(..n).collect();
            let mut x: Vec<Complex32> = block.iter().map(|v| Complex32::new(*v, 0.0)).collect();
            fwd.process(&mut x);
            for (i, v) in x.iter_mut().enumerate() {
                *v *= h[i];
                // analytic signal: keep DC/Nyquist once, double positive, zero negative
                if i > 0 && i < n / 2 {
                    *v *= 2.0;
                } else if i > n / 2 {
                    *v = Complex32::new(0.0, 0.0);
                }
            }
            inv.process(&mut x);
            let env: Vec<f32> = x.iter().map(|c| c.norm() / n as f32).collect();
            let (peak_i, peak) = env.iter().copied().enumerate().fold((0, 0.0f32), |a, (i, v)| if v > a.1 { (i, v) } else { a });
            let mut sorted = env.clone();
            sorted.sort_by(|a, b| a.total_cmp(b));
            let med = sorted[n / 2].max(1e-12);
            // Keep the reference lag fixed so carrier phase stays continuous from frame to frame.
            let ref_i = match ref_lag {
                None => {
                    ref_lag = Some(peak_i);
                    peak_i
                }
                Some(r) => {
                    let d = (peak_i + n - r) % n;
                    if d.min(n - d) >= 2 {
                        ref_lag = Some(peak_i);
                        epoch += 1;
                        peak_i
                    } else {
                        r
                    }
                }
            };
            let profile: Vec<f32> = (0..k_max).map(|k| env[(ref_i + k) % n]).collect();
            let z0 = x[ref_i];
            let rot = z0.conj() * (1.0 / z0.norm().max(1e-20));
            let iq: Vec<(f32, f32)> = (0..k_max).map(|k| { let z = x[(ref_i + k) % n] * rot / n as f32; (z.re, z.im) }).collect();
            let frame = Frame {
                profile,
                iq,
                epoch,
                wall_m_per_bin: SPEED_OF_SOUND / fs / 2.0,
                direct_snr_db: 20.0 * (peak.max(1e-12) / med).log10(),
                period_s: n as f32 / fs,
            };
            if tx.send(frame).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MATH CHECK: chirp length is duration x rate (8 ms at 48 kHz = 384 samples).
    #[test]
    fn math_check_chirp_len() {
        assert_eq!(chirp(48_000.0).len(), 384);
    }
}
