// SPDX-License-Identifier: AGPL-3.0-or-later
//! macOS backend: CoreWLAN active scan.
//! Since macOS 14, SSID/BSSID are withheld unless the app has Location permission;
//! in that case this backend reports PermissionDenied instead of returning anonymous rows.

use crate::{channel_from_freq, unix_ms, Observation, ScanError, WifiScanner};
use objc2_core_wlan::CWWiFiClient;
use std::time::{Duration, SystemTime};

pub struct MacScanner;

impl MacScanner {
    pub fn new() -> Self {
        Self
    }
}

impl WifiScanner for MacScanner {
    fn backend_name(&self) -> &'static str {
        "macOS (CoreWLAN)"
    }
    fn min_interval(&self) -> Duration {
        Duration::from_secs(10)
    }
    fn scan(&mut self) -> Result<Vec<Observation>, ScanError> {
        unsafe {
            let client = CWWiFiClient::sharedWiFiClient();
            let iface = client.interface().ok_or(ScanError::NoAdapter)?;
            let nets = iface
                .scanForNetworksWithName_error(None)
                .map_err(|e| ScanError::Backend(e.localizedDescription().to_string()))?;
            let read_at = SystemTime::now();
            let mut out = Vec::new();
            let mut redacted = 0usize;
            for n in nets.iter() {
                let Some(bssid) = n.bssid() else {
                    redacted += 1;
                    continue;
                };
                let channel = n.wlanChannel().map(|c| c.channelNumber() as u32);
                out.push(Observation {
                    bssid: bssid.to_string(),
                    ssid: n.ssid().map(|s| s.to_string()),
                    rssi_dbm: Some(n.rssiValue() as i32),
                    frequency_mhz: None,
                    channel: channel.filter(|c| *c > 0),
                    age_ms: None,
                    stamp_ms: Some(unix_ms(read_at)),
                    read_at,
                });
            }
            let _ = channel_from_freq;
            if out.is_empty() && redacted > 0 {
                return Err(ScanError::PermissionDenied(
                    "macOS withholds network identifiers until the app is granted Location permission (System Settings > Privacy & Security > Location Services)".into(),
                ));
            }
            Ok(out)
        }
    }
}
