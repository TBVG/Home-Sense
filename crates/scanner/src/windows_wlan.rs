// SPDX-License-Identifier: AGPL-3.0-or-later
//! Windows backend: WLAN API (wlanapi). Reports lRssi in dBm as given by the driver.

use crate::{channel_from_freq, unix_ms, Observation, ScanError, WifiScanner};
use std::time::{Duration, SystemTime};
use windows::core::GUID;
use windows::Win32::Foundation::{HANDLE, WIN32_ERROR};
use windows::Win32::NetworkManagement::WiFi::*;

pub struct WindowsScanner {
    handle: Option<HANDLE>,
}

impl WindowsScanner {
    pub fn new() -> Self {
        Self { handle: None }
    }

    fn open(&mut self) -> Result<HANDLE, ScanError> {
        if let Some(h) = self.handle {
            return Ok(h);
        }
        let mut negotiated = 0u32;
        let mut h = HANDLE::default();
        let r = unsafe { WlanOpenHandle(2, None, &mut negotiated, &mut h) };
        if r != 0 {
            return Err(ScanError::Backend(format!("WlanOpenHandle failed (error {r})")));
        }
        self.handle = Some(h);
        Ok(h)
    }
}

// The handle is only used from the one scanner thread.
unsafe impl Send for WindowsScanner {}

impl Drop for WindowsScanner {
    fn drop(&mut self) {
        if let Some(h) = self.handle {
            unsafe { WlanCloseHandle(h, None) };
        }
    }
}

fn check(r: u32, what: &str) -> Result<(), ScanError> {
    match WIN32_ERROR(r) {
        WIN32_ERROR(0) => Ok(()),
        WIN32_ERROR(5) => Err(ScanError::PermissionDenied(format!("{what}: access denied (Windows may require Location services to be on for Wi-Fi scanning)"))),
        e => Err(ScanError::Backend(format!("{what} failed (error {})", e.0))),
    }
}

impl WifiScanner for WindowsScanner {
    fn backend_name(&self) -> &'static str {
        "Windows (WLAN API)"
    }
    fn min_interval(&self) -> Duration {
        Duration::from_secs(10)
    }
    fn scan(&mut self) -> Result<Vec<Observation>, ScanError> {
        let h = self.open()?;
        unsafe {
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            check(WlanEnumInterfaces(h, None, &mut list), "WlanEnumInterfaces")?;
            let n = (*list).dwNumberOfItems as usize;
            if n == 0 {
                WlanFreeMemory(list as *const _);
                return Err(ScanError::NoAdapter);
            }
            let guid: GUID = (*list).InterfaceInfo[0].InterfaceGuid;
            WlanFreeMemory(list as *const _);

            // Request a fresh scan, give the driver time to finish, then read the table.
            let _ = WlanScan(h, &guid, None, None, None);
            std::thread::sleep(Duration::from_secs(4));

            let mut bss: *mut WLAN_BSS_LIST = std::ptr::null_mut();
            check(
                WlanGetNetworkBssList(h, &guid, None, dot11_BSS_type_any, false, None, &mut bss),
                "WlanGetNetworkBssList",
            )?;
            let read_at = SystemTime::now();
            let count = (*bss).dwNumberOfItems as usize;
            let entries = std::slice::from_raw_parts((*bss).wlanBssEntries.as_ptr(), count);
            let mut out = Vec::with_capacity(count);
            for e in entries {
                let b = e.dot11Bssid;
                let len = (e.dot11Ssid.uSSIDLength as usize).min(32);
                let ssid = String::from_utf8_lossy(&e.dot11Ssid.ucSSID[..len]).to_string();
                let freq = e.ulChCenterFrequency / 1000; // kHz -> MHz
                out.push(Observation {
                    bssid: format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", b[0], b[1], b[2], b[3], b[4], b[5]),
                    ssid: if ssid.is_empty() { None } else { Some(ssid) },
                    rssi_dbm: Some(e.lRssi),
                    frequency_mhz: if freq > 0 { Some(freq) } else { None },
                    channel: channel_from_freq(freq),
                    age_ms: None,
                    stamp_ms: Some(unix_ms(read_at)),
                    read_at,
                });
            }
            WlanFreeMemory(bss as *const _);
            Ok(out)
        }
    }
}
