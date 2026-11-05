// SPDX-License-Identifier: AGPL-3.0-or-later
//! Fallback for OSes other than Linux, macOS and Windows.
//! It returns an error, never data.
#![allow(dead_code)]

use crate::{Observation, ScanError, WifiScanner};
use std::time::Duration;

pub struct UnsupportedScanner;

impl WifiScanner for UnsupportedScanner {
    fn backend_name(&self) -> &'static str {
        "none (not implemented on this OS yet)"
    }
    fn min_interval(&self) -> Duration {
        Duration::from_secs(5)
    }
    fn scan(&mut self) -> Result<Vec<Observation>, ScanError> {
        Err(ScanError::Unsupported)
    }
}
