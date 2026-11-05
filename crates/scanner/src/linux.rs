// SPDX-License-Identifier: AGPL-3.0-or-later
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]
//! Linux backend: asks NetworkManager for a rescan (best effort, no root needed),
//! then reads the kernel's nl80211 scan table through `iw ... scan dump`,
//! which reports true dBm values.

use crate::{channel_from_freq, unix_ms, Observation, ScanError, WifiScanner};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

pub struct LinuxScanner {
    iface: Option<String>,
    last_rescan: Option<Instant>,
}

impl LinuxScanner {
    pub fn new() -> Self {
        Self { iface: None, last_rescan: None }
    }

    fn run(cmd: &str, args: &[&str]) -> Result<String, ScanError> {
        let out = Command::new(cmd).args(args).output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ScanError::MissingTool(cmd.to_string())
            } else {
                ScanError::Backend(format!("{cmd}: {e}"))
            }
        })?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if err.to_lowercase().contains("permission") || err.contains("Operation not permitted") {
                return Err(ScanError::PermissionDenied(err));
            }
            return Err(ScanError::Backend(format!("{cmd} {}: {err}", args.join(" "))));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn find_iface() -> Result<String, ScanError> {
        let text = Self::run("iw", &["dev"])?;
        text.lines()
            .filter_map(|l| l.trim().strip_prefix("Interface "))
            .map(|s| s.trim().to_string())
            .next()
            .ok_or(ScanError::NoAdapter)
    }
}

impl WifiScanner for LinuxScanner {
    fn backend_name(&self) -> &'static str {
        "Linux (iw / nl80211)"
    }

    fn min_interval(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn scan(&mut self) -> Result<Vec<Observation>, ScanError> {
        if self.iface.is_none() {
            self.iface = Some(Self::find_iface()?);
        }
        let iface = self.iface.clone().unwrap();

        // Ask for a fresh scan at most every 15 s; failure is fine, the dump still works.
        let due = self.last_rescan.map_or(true, |t| t.elapsed() >= Duration::from_secs(15));
        if due {
            let _ = Command::new("nmcli").args(["device", "wifi", "rescan", "ifname", &iface]).output();
            self.last_rescan = Some(Instant::now());
        }

        let dump = Self::run("iw", &["dev", &iface, "scan", "dump"])?;
        Ok(parse_scan_dump(&dump, SystemTime::now()))
    }
}

/// Parse `iw dev <if> scan dump` output. Fields that are absent stay `None`.
pub(crate) fn parse_scan_dump(text: &str, read_at: SystemTime) -> Vec<Observation> {
    let mut out: Vec<Observation> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("BSS ") {
            let bssid: String = rest.chars().take_while(|c| *c != '(' && !c.is_whitespace()).collect();
            out.push(Observation {
                bssid,
                ssid: None,
                rssi_dbm: None,
                frequency_mhz: None,
                channel: None,
                age_ms: None,
                stamp_ms: None,
                read_at,
            });
            continue;
        }
        let Some(cur) = out.last_mut() else { continue };
        let t = line.trim();
        if let Some(v) = t.strip_prefix("signal:") {
            // e.g. "-52.00 dBm"
            cur.rssi_dbm = v.split_whitespace().next().and_then(|n| n.parse::<f32>().ok()).map(|f| f.round() as i32);
        } else if let Some(v) = t.strip_prefix("freq:") {
            cur.frequency_mhz = v.trim().parse::<f32>().ok().map(|f| f as u32);
            cur.channel = cur.frequency_mhz.and_then(channel_from_freq);
        } else if let Some(v) = t.strip_prefix("SSID:") {
            let s = v.trim();
            cur.ssid = if s.is_empty() { None } else { Some(s.to_string()) };
        } else if let Some(v) = t.strip_prefix("last seen:") {
            cur.age_ms = v.split_whitespace().next().and_then(|n| n.parse().ok());
            cur.stamp_ms = cur.age_ms.map(|a| unix_ms(read_at).saturating_sub(a));
        }
    }
    out
}
