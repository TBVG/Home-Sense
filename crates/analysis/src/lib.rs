// SPDX-License-Identifier: AGPL-3.0-or-later
//! Pure analysis logic. Every output derives from measurements passed in; nothing is invented.
//! Where there is not enough data, functions return `None`/`Err(reason)` and the UI shows "--".

pub mod map;
pub mod aploc;
pub mod monitor;
pub mod personloc;
pub mod openings;
pub mod security;
pub mod sound;
pub mod who;
pub mod wifi;

pub(crate) fn median(v: &mut [f32]) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

pub(crate) fn mean_std(v: &[f32]) -> Option<(f32, f32)> {
    if v.is_empty() {
        return None;
    }
    let m = v.iter().sum::<f32>() / v.len() as f32;
    let var = v.iter().map(|x| (x - m) * (x - m)).sum::<f32>() / v.len() as f32;
    Some((m, var.sqrt()))
}

#[cfg(test)]
mod tests {
    use super::*;
    /// MATH CHECK: median of [3,1,2] is 2; of [1,2,3,4] is 2.5.
    #[test]
    fn math_check_median() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&mut [1.0, 2.0, 3.0, 4.0]), Some(2.5));
    }
    /// MATH CHECK: population mean/std of [1,3] is (2,1).
    #[test]
    fn math_check_mean_std() {
        assert_eq!(mean_std(&[1.0, 3.0]), Some((2.0, 1.0)));
    }
}
