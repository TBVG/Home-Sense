// SPDX-License-Identifier: AGPL-3.0-or-later
//! Wi-Fi scanning behind a trait, one implementation per OS.
//!
//! Every value returned here comes from the operating system's Wi-Fi stack.
//! If a value cannot be read it is `None`; nothing is estimated or filled in.

use std::time::{Duration, SystemTime};

mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows_wlan;
mod unsupported;

/// One access point as reported by the OS at one moment.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    /// Hardware address of the access point.
    pub bssid: String,
    /// Network name. `None` for hidden networks or if the OS did not report it.
    pub ssid: Option<String>,
    /// Received signal strength in dBm, exactly as the OS reported it.
    pub rssi_dbm: Option<i32>,
    /// Centre frequency in MHz, as reported.
    pub frequency_mhz: Option<u32>,
    /// Channel number computed from the reported frequency (fixed mapping), if known.
    pub channel: Option<u32>,
    /// How old the OS says this measurement is, in milliseconds, if reported.
    /// Operating systems often return cached scan results, so this matters.
    pub age_ms: Option<u64>,
    /// Unix time in ms at which the OS measured this value, if known.
    /// Linux: derived from the kernel's "last seen" age. Windows/macOS: the time the
    /// scan returned (those APIs do not expose a per-AP measurement time).
    /// `None` means the backend cannot say, and time-based analysis ignores the reading.
    pub stamp_ms: Option<u64>,
    /// When this app read the value from the OS.
    pub read_at: SystemTime,
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("Wi-Fi scanning is not implemented on this operating system yet")]
    Unsupported,
    #[error("No Wi-Fi adapter was found")]
    NoAdapter,
    #[error("Required system tool is missing: {0}")]
    MissingTool(String),
    #[error("Permission denied: {0}")]
    PermissionDenied(String),
    #[error("Scan failed: {0}")]
    Backend(String),
}

pub trait WifiScanner: Send {
    /// Short name of the backend, shown in the UI.
    fn backend_name(&self) -> &'static str;
    /// Minimum time between scans. Callers must respect it (OS scan throttling).
    fn min_interval(&self) -> Duration;
    /// Read current access points from the OS.
    fn scan(&mut self) -> Result<Vec<Observation>, ScanError>;
}

pub(crate) fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

pub fn channel_from_freq(f: u32) -> Option<u32> {
    match f {
        2484 => Some(14),
        2412..=2472 => Some((f - 2407) / 5),
        5160..=5885 => Some((f - 5000) / 5),
        5955..=7115 => Some((f - 5950) / 5),
        _ => None,
    }
}

/// Create the scanner for the OS this binary was built for.
pub fn default_scanner() -> Box<dyn WifiScanner> {
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LinuxScanner::new())
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacScanner::new())
    }
    #[cfg(windows)]
    {
        Box::new(windows_wlan::WindowsScanner::new())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Box::new(unsupported::UnsupportedScanner)
    }
}
