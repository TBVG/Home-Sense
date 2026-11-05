// SPDX-License-Identifier: AGPL-3.0-or-later
//! Where movement probably is, from Wi-Fi link disturbance and the sound distance.
//!
//! * Wi-Fi: a person near the straight path between an access point and this computer disturbs that link.
//!   Each disturbed link adds a likelihood ellipse around its path (extra path length through the point small).
//!   Where several disturbed links cross, the likelihood is highest.
//! * Sound: the echo distance says the mover is at about that distance from the computer (a ring).
//! * Both together keep only the part of the ring that the Wi-Fi links also point to.
//! The result is a probability over map cells, so the drawn cloud is as large as the uncertainty. With too
//! little evidence there is no result, or only the ring.

use crate::aploc::ApFit;
use std::collections::HashMap;

pub struct Estimate {
    /// (x, y, share of probability) for the cells worth drawing, shares sum to 1.
    pub cells: Vec<(f32, f32, f32)>,
    pub cx: f32,
    pub cy: f32,
    /// RMS spread of the probability around its centre, metres.
    pub spread_m: f32,
    /// 0 = spread over everything, 1 = a single cell.
    pub quality: f32,
    /// Set when only the sound distance was available: the cells form a ring of this radius.
    pub ring_m: Option<f32>,
    pub wifi_links: usize,
}

const CELL: f32 = 0.5;
const SOUND_SIGMA_M: f32 = 0.5;
const BASE_SIGMA_M: f32 = 0.7;
const MIN_LINK_WEIGHT: f32 = 0.2;
// BASE_SIGMA_M is the width of a link's likelihood corridor around the straight path, before extra uncertainty.

pub fn estimate(laptop: (f32, f32), laptop_sigma: f32, fits: &[ApFit], weights: &HashMap<String, f32>, sound_range: Option<f32>, region: (f32, f32, f32, f32)) -> Option<Estimate> {
    // One router often shows up as several networks (2.4 GHz, 5 GHz, guest). They are the same place, so they are
    // one source, not several: group fits whose positions are within 2 m and keep the strongest weight.
    let mut used: Vec<(&ApFit, f32)> = fits.iter().filter_map(|f| weights.get(&f.bssid).map(|w| (f, *w))).filter(|(_, w)| *w >= MIN_LINK_WEIGHT).collect();
    used.sort_by(|a, b| b.1.total_cmp(&a.1));
    struct Group<'a> {
        w: f32,
        pos: (f32, f32),
        cands: &'a [(f32, f32)],
    }
    let mut groups: Vec<Group> = vec![];
    for (f, w) in used {
        if let Some(g) = groups.iter_mut().find(|g| ((g.pos.0 - f.x).powi(2) + (g.pos.1 - f.y).powi(2)).sqrt() < 2.0) {
            g.w = g.w.max(w);
        } else {
            groups.push(Group { w, pos: (f.x, f.y), cands: &f.cands });
        }
    }
    let wifi_ok = groups.len() >= 2;
    if !wifi_ok && sound_range.is_none() {
        return None;
    }
    let wsum: f32 = groups.iter().map(|g| g.w).sum();
    let (nx, ny) = (((region.1 - region.0) / CELL).ceil() as usize, ((region.3 - region.2) / CELL).ceil() as usize);
    if nx == 0 || ny == 0 {
        return None;
    }
    let sig2 = BASE_SIGMA_M * BASE_SIGMA_M + laptop_sigma * laptop_sigma;
    let mut raw: Vec<(f32, f32, f32)> = Vec::with_capacity(nx * ny);
    for i in 0..nx {
        for j in 0..ny {
            let (x, y) = (region.0 + (i as f32 + 0.5) * CELL, region.2 + (j as f32 + 0.5) * CELL);
            let dl = ((x - laptop.0).powi(2) + (y - laptop.1).powi(2)).sqrt();
            let w_part = if wifi_ok {
                let mut s = 0.0;
                for g in &groups {
                    // average over every position the router could be at: handles a well-placed router and a
                    // distant neighbour (whose position is only known as a direction) in the same way
                    let mut link = 0.0;
                    for (ax, ay) in g.cands {
                        let da = ((x - ax).powi(2) + (y - ay).powi(2)).sqrt();
                        let dal = ((ax - laptop.0).powi(2) + (ay - laptop.1).powi(2)).sqrt();
                        let e = da + dl - dal;
                        link += (-e * e / (2.0 * sig2)).exp();
                    }
                    s += g.w * link / g.cands.len().max(1) as f32;
                }
                s / wsum + 0.02
            } else {
                1.0
            };
            let s_part = sound_range.map_or(1.0, |r| (-(dl - r).powi(2) / (2.0 * SOUND_SIGMA_M * SOUND_SIGMA_M)).exp());
            raw.push((x, y, w_part * s_part));
        }
    }
    let total: f32 = raw.iter().map(|c| c.2).sum();
    if total <= 0.0 {
        return None;
    }
    let n_cells = raw.len() as f32;
    let h: f32 = raw.iter().map(|c| c.2 / total).filter(|p| *p > 0.0).map(|p| -p * p.ln()).sum();
    let quality = (1.0 - h / n_cells.ln()).clamp(0.0, 1.0);
    let max = raw.iter().map(|c| c.2).fold(0.0f32, f32::max);
    let mut cells: Vec<(f32, f32, f32)> = raw.into_iter().filter(|c| c.2 >= 0.05 * max).collect();
    let kept: f32 = cells.iter().map(|c| c.2).sum();
    for c in cells.iter_mut() {
        c.2 /= kept;
    }
    let cx: f32 = cells.iter().map(|c| c.0 * c.2).sum();
    let cy: f32 = cells.iter().map(|c| c.1 * c.2).sum();
    let spread = cells.iter().map(|c| ((c.0 - cx).powi(2) + (c.1 - cy).powi(2)) * c.2).sum::<f32>().sqrt();
    Some(Estimate { cells, cx, cy, spread_m: spread, quality, ring_m: if wifi_ok { None } else { sound_range }, wifi_links: groups.len() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fit(b: &str, x: f32, y: f32) -> ApFit {
        ApFit { bssid: b.into(), ssid: None, x, y, exponent: 2.0, rmse_db: 1.0, radius_m: 0.5, spots: 10, cands: vec![(x, y)] }
    }

    /// MATH CHECK (hand-made geometry, never reaches the UI): this computer at (0,0), access points at (10,0) and (0,10),
    /// both links disturbed. The two link paths (the x axis and the y axis) only overlap near the computer,
    /// so the most likely cell must be within 2 m of the origin.
    #[test]
    fn math_check_two_links_cross_near_their_common_end() {
        let fits = vec![fit("a", 10.0, 0.0), fit("b", 0.0, 10.0)];
        let w: HashMap<String, f32> = [("a".to_string(), 1.0), ("b".to_string(), 1.0)].into_iter().collect();
        let e = estimate((0.0, 0.0), 0.0, &fits, &w, None, (-2.0, 12.0, -2.0, 12.0)).unwrap();
        let best = e.cells.iter().fold((0.0, 0.0, 0.0f32), |a, c| if c.2 > a.2 { *c } else { a });
        assert!(best.0.hypot(best.1) < 2.0, "best at {},{}", best.0, best.1);
    }

    /// MATH CHECK: with only a sound distance of 3 m, every drawn cell lies about 3 m from the computer (a ring).
    #[test]
    fn math_check_sound_only_is_a_ring() {
        let e = estimate((5.0, 5.0), 0.0, &[], &HashMap::new(), Some(3.0), (0.0, 10.0, 0.0, 10.0)).unwrap();
        assert_eq!(e.ring_m, Some(3.0));
        assert!(e.cells.iter().all(|c| (((c.0 - 5.0).powi(2) + (c.1 - 5.0).powi(2)).sqrt() - 3.0).abs() < 1.3));
    }

    /// MATH CHECK: no evidence gives no estimate.
    #[test]
    fn math_check_no_evidence_is_none() {
        assert!(estimate((0.0, 0.0), 0.0, &[], &HashMap::new(), None, (0.0, 4.0, 0.0, 4.0)).is_none());
    }

    /// MATH CHECK: two networks at the same place (2.4 GHz and 5 GHz of one router) are ONE source, so with no sound
    /// there is nothing to triangulate and no estimate.
    #[test]
    fn math_check_one_router_two_bands_is_one_source() {
        let fits = vec![fit("a", 10.0, 0.0), fit("a5", 10.0, 0.5)];
        let w: HashMap<String, f32> = [("a".to_string(), 1.0), ("a5".to_string(), 1.0)].into_iter().collect();
        assert!(estimate((0.0, 0.0), 0.0, &fits, &w, None, (-6.0, 12.0, -6.0, 6.0)).is_none());
    }

    /// MATH CHECK: two far-away networks known only by direction (one along +x, one along +y), the +x one disturbed much
    /// more, and a sound distance of 3 m: the best cell is on the +x side, near (3, 0).
    #[test]
    fn math_check_far_networks_give_a_direction() {
        let mut ax = fit("x", 30.0, 0.0);
        ax.cands = vec![(20.0, 0.0), (30.0, 0.0), (40.0, 0.0)];
        let mut ay = fit("y", 0.0, 30.0);
        ay.cands = vec![(0.0, 20.0), (0.0, 30.0), (0.0, 40.0)];
        let w: HashMap<String, f32> = [("x".to_string(), 1.0), ("y".to_string(), 0.25)].into_iter().collect();
        let e = estimate((0.0, 0.0), 0.0, &[ax, ay], &w, Some(3.0), (-6.0, 6.0, -6.0, 6.0)).unwrap();
        let best = e.cells.iter().fold((0.0, 0.0, 0.0f32), |a, c| if c.2 > a.2 { *c } else { a });
        assert!(best.0 > 2.0 && best.1.abs() < 1.5, "best at {},{}", best.0, best.1);
    }
}
