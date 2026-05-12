// passes.rs – Satellite pass prediction using satkit SGP4 propagation.
//
// Algorithm:
//   1. Parse TLE lines with satkit::TLE.
//   2. Step through time at coarse resolution (e.g. 30 s) looking for
//      transitions above the minimum elevation threshold.
//   3. Once a rising edge is found, bracket AOS with a binary-search bisect.
//   4. Similarly bracket LOS on the falling edge.
//   5. Within the pass, sample at finer resolution (10 s) to find max elevation.
//
// Coordinate path:
//   SGP4 → TEME position → ITRF (via qteme2itrf quaternion) → ENU at ground station
//   EL  = atan2(Up, sqrt(East²+North²))
//   AZ  = atan2(East, North)  [measured clockwise from North]

use anyhow::{Result, anyhow};
use chrono::{DateTime, TimeZone, Utc};
use once_cell::unsync::OnceCell;
use satkit::{Instant as SatInstant, TLE, frametransform, itrfcoord::ITRFCoord, sgp4::sgp4};
use tracing::debug;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A ground station with geodetic coordinates.
#[derive(Debug, Clone)]
pub struct GroundStation {
    pub name: String,
    /// Geodetic latitude, decimal degrees (positive North)
    pub latitude_deg: f64,
    /// Geodetic longitude, decimal degrees (positive East)
    pub longitude_deg: f64,
    /// Elevation above the ellipsoid (above sea level) in meters.
    pub elevation_m: f64,
    /// Height above the ground in meters.
    pub altitude_m: f64,
}

/// One predicted pass over a ground station.
#[derive(Debug, Clone)]
pub struct Pass {
    pub station_name: String,
    /// Acquisition of Signal
    pub aos_utc: DateTime<Utc>,
    pub aos_elev_deg: f64,
    pub aos_az_deg: f64,
    /// Time of maximum elevation
    pub max_utc: DateTime<Utc>,
    pub max_elev_deg: f64,
    pub max_az_deg: f64,
    /// Loss of Signal
    pub los_utc: DateTime<Utc>,
    pub los_elev_deg: f64,
    pub los_az_deg: f64,
    /// Pass duration above min_elev in whole seconds
    pub duration_secs: u64,
}

impl GroundStation {
    pub fn new(
        name: String,
        latitude_deg: f64,
        longitude_deg: f64,
        elevation_m: f64,
        altitude_m: f64,
    ) -> GroundStation {
        GroundStation {
            name,
            latitude_deg,
            longitude_deg,
            elevation_m,
            altitude_m,
            // ecef_cache: OnceCell::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Find all passes for multiple ground stations, searching from now.
///
/// * `line1`, `line2`  – two TLE lines (no name line)
/// * `stations`        – slice of ground stations to check
/// * `hours`           – how many hours ahead to search
/// * `min_elev_deg`    – minimum peak elevation to include a pass
pub fn find_passes(
    line1: &str,
    line2: &str,
    stations: &[GroundStation],
    hours: u64,
    min_elev_deg: f64,
) -> Result<Vec<Pass>> {
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    find_passes_from(line1, line2, stations, hours, min_elev_deg, now_unix)
}

/// Like [`find_passes`] but with an explicit Unix-seconds start time.
/// Used by tests and any caller that needs a deterministic window.
pub fn find_passes_from(
    line1: &str,
    line2: &str,
    stations: &[GroundStation],
    hours: u64,
    min_elev_deg: f64,
    start_unix: f64,
) -> Result<Vec<Pass>> {
    let mut tle = TLE::load_2line(line1, line2).map_err(|e| anyhow!("Failed to parse TLE: {e}"))?;

    let t_start = SatInstant::from_unixtime(start_unix);
    let t_end = SatInstant::from_unixtime(start_unix + hours as f64 * 3600.0);

    let mut all_passes: Vec<Pass> = Vec::new();

    for station in stations {
        let gs_itrf = ITRFCoord::from_geodetic_deg(
            station.latitude_deg,
            station.longitude_deg,
            station.altitude_m + station.elevation_m,
        );

        let mut passes = predict_passes_for_station(
            &mut tle,
            &gs_itrf,
            &station.name,
            &t_start,
            &t_end,
            min_elev_deg,
        )?;

        all_passes.append(&mut passes);
    }

    all_passes.sort_by(|a, b| a.aos_utc.cmp(&b.aos_utc));
    Ok(all_passes)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Elevation and azimuth of a satellite TEME position as seen from a ground station.
///
/// Returns `(elevation_deg, azimuth_deg)`.
fn look_angles(teme_pos_m: &[f64; 3], t: &SatInstant, gs_itrf: &ITRFCoord) -> (f64, f64) {
    // Rotate TEME → ITRF
    let teme_vec = satkit::Vector3::from([teme_pos_m[0], teme_pos_m[1], teme_pos_m[2]]);
    let q = frametransform::qteme2itrf(t);
    let sat_itrf_vec = q * teme_vec;

    // Convert to ENU at the ground station (to_enu takes the satellite's absolute ITRF position)
    let sat_itrf = ITRFCoord { itrf: sat_itrf_vec };
    let enu = gs_itrf.to_enu(&sat_itrf);
    let east = enu[0];
    let north = enu[1];
    let up = enu[2];

    let horiz = (east * east + north * north).sqrt();
    let elev_rad = up.atan2(horiz);
    // Azimuth: 0° = North, 90° = East, clockwise
    let az_rad = east.atan2(north);

    let elev_deg = elev_rad.to_degrees();
    let az_deg = az_rad.to_degrees().rem_euclid(360.0);
    (elev_deg, az_deg)
}

/// Propagate TLE at time t, return TEME position in metres.
fn propagate_teme(tle: &mut TLE, t: &SatInstant) -> Result<[f64; 3]> {
    let states = sgp4(tle, &[t.clone()]).map_err(|e| anyhow!("SGP4 error: {e}"))?;
    // SGP4 returns position in km; convert to metres
    Ok([
        states.pos[(0, 0)] * 1000.0,
        states.pos[(1, 0)] * 1000.0,
        states.pos[(2, 0)] * 1000.0,
    ])
}

/// Satkit Instant ↔ unix seconds helpers
fn instant_to_unixtime(t: &SatInstant) -> f64 {
    t.as_unixtime()
}

fn unixtime_to_instant(u: f64) -> SatInstant {
    SatInstant::from_unixtime(u)
}

fn instant_to_datetime(t: &SatInstant) -> DateTime<Utc> {
    let unix = instant_to_unixtime(t);
    Utc.timestamp_opt(unix as i64, ((unix.fract()) * 1e9) as u32)
        .single()
        .unwrap_or_else(|| Utc::now())
}

/// Find passes for one ground station over the search window.
fn predict_passes_for_station(
    tle: &mut TLE,
    gs_itrf: &ITRFCoord,
    station_name: &str,
    t_start: &SatInstant,
    t_end: &SatInstant,
    min_elev_deg: f64,
) -> Result<Vec<Pass>> {
    let coarse_step = 30.0_f64; // seconds – coarse scan
    let fine_step = 10.0_f64; // seconds – used inside a detected pass
    let bisect_iters = 16; // iterations for AOS/LOS bisection

    let unix_start = instant_to_unixtime(t_start);
    let unix_end = instant_to_unixtime(t_end);

    let mut passes = Vec::new();
    let mut t = unix_start;
    let mut prev_elev: Option<f64> = None;

    while t <= unix_end {
        let inst = unixtime_to_instant(t);
        let pos = match propagate_teme(tle, &inst) {
            Ok(p) => p,
            Err(_) => {
                t += coarse_step;
                prev_elev = None;
                continue;
            }
        };
        let (elev, _az) = look_angles(&pos, &inst, gs_itrf);
        debug!("t: {t}, elev: {elev}, az: {_az}");

        if let Some(prev) = prev_elev {
            // Detect rising edge (below → above min_elev)
            if prev < min_elev_deg && elev >= min_elev_deg {
                // Binary search for AOS (between t - coarse_step and t)
                let aos_unix = bisect_crossing(
                    tle,
                    gs_itrf,
                    t - coarse_step,
                    t,
                    min_elev_deg,
                    bisect_iters,
                    true,
                )?;

                // Scan forward in fine steps to find LOS and max
                let (los_unix, max_unix, max_elev, max_az, los_elev, los_az, aos_elev, aos_az) =
                    scan_pass(
                        tle,
                        gs_itrf,
                        aos_unix,
                        unix_end,
                        fine_step,
                        min_elev_deg,
                        bisect_iters,
                    )?;

                // Only record if max elevation meets threshold
                if max_elev >= min_elev_deg {
                    let duration = (los_unix - aos_unix).max(0.0) as u64;

                    passes.push(Pass {
                        station_name: station_name.to_string(),
                        aos_utc: instant_to_datetime(&unixtime_to_instant(aos_unix)),
                        aos_elev_deg: aos_elev,
                        aos_az_deg: aos_az,
                        max_utc: instant_to_datetime(&unixtime_to_instant(max_unix)),
                        max_elev_deg: max_elev,
                        max_az_deg: max_az,
                        los_utc: instant_to_datetime(&unixtime_to_instant(los_unix)),
                        los_elev_deg: los_elev,
                        los_az_deg: los_az,
                        duration_secs: duration,
                    });

                    // Advance t past the LOS so we don't re-detect the same pass
                    t = los_unix + coarse_step;
                    prev_elev = None;
                    continue;
                }
            }
        }

        prev_elev = Some(elev);
        t += coarse_step;
    }

    Ok(passes)
}

/// Binary search for the exact crossing time (above/below `threshold`).
///
/// * `rising = true`  → find the moment elev crosses *above* threshold
/// * `rising = false` → find the moment elev crosses *below* threshold
fn bisect_crossing(
    tle: &mut TLE,
    gs_itrf: &ITRFCoord,
    mut t_lo: f64,
    mut t_hi: f64,
    threshold: f64,
    iters: usize,
    rising: bool,
) -> Result<f64> {
    for _ in 0..iters {
        let t_mid = (t_lo + t_hi) / 2.0;
        let inst = unixtime_to_instant(t_mid);
        let pos = propagate_teme(tle, &inst)?;
        let (elev, _) = look_angles(&pos, &inst, gs_itrf);

        let above = elev >= threshold;
        if above == rising {
            t_hi = t_mid; // rising: the crossing is before mid
        } else {
            t_lo = t_mid;
        }
    }
    Ok((t_lo + t_hi) / 2.0)
}

/// Scan forward from AOS to LOS, finding max elevation and exact LOS.
#[allow(clippy::too_many_arguments)]
fn scan_pass(
    tle: &mut TLE,
    gs_itrf: &ITRFCoord,
    aos_unix: f64,
    unix_end: f64,
    fine_step: f64,
    min_elev_deg: f64,
    bisect_iters: usize,
) -> Result<(f64, f64, f64, f64, f64, f64, f64, f64)> {
    let mut t = aos_unix;
    let mut max_elev = f64::NEG_INFINITY;
    let mut max_az = 0.0_f64;
    let mut max_t = aos_unix;
    let mut prev_elev: Option<f64> = None;
    let mut prev_t = aos_unix;

    // AOS elevation and azimuth
    let aos_inst = unixtime_to_instant(aos_unix);
    let aos_pos = propagate_teme(tle, &aos_inst)?;
    let (aos_elev, aos_az) = look_angles(&aos_pos, &aos_inst, gs_itrf);

    while t <= unix_end + fine_step {
        let inst = unixtime_to_instant(t);
        let pos = match propagate_teme(tle, &inst) {
            Ok(p) => p,
            Err(_) => break,
        };
        let (elev, az) = look_angles(&pos, &inst, gs_itrf);

        if elev > max_elev {
            max_elev = elev;
            max_az = az;
            max_t = t;
        }

        // Detect falling edge (above → below threshold)
        if let Some(prev) = prev_elev {
            if prev >= min_elev_deg && elev < min_elev_deg {
                // Bisect for exact LOS
                let los_unix =
                    bisect_crossing(tle, gs_itrf, prev_t, t, min_elev_deg, bisect_iters, false)?;

                let los_inst = unixtime_to_instant(los_unix);
                let los_pos = propagate_teme(tle, &los_inst)?;
                let (los_elev, los_az) = look_angles(&los_pos, &los_inst, gs_itrf);

                return Ok((
                    los_unix, max_t, max_elev, max_az, los_elev, los_az, aos_elev, aos_az,
                ));
            }
        }

        prev_elev = Some(elev);
        prev_t = t;
        t += fine_step;
    }

    // If we hit unix_end without finding LOS, set LOS at the window boundary
    let los_unix = unix_end.min(t - fine_step);

    let los_inst = unixtime_to_instant(los_unix);
    let los_pos = propagate_teme(tle, &los_inst).unwrap_or([0.0; 3]);
    let (los_elev, los_az) = look_angles(&los_pos, &los_inst, gs_itrf);

    Ok((
        los_unix, max_t, max_elev, max_az, los_elev, los_az, aos_elev, aos_az,
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // NORAD 69015 TLE valid around 2026-05-12 05:08 UTC.
    const TLE1: &str = "1 69015U 26100AM  26132.21434330  .00004984  00000-0  22703-3 0  9996";
    const TLE2: &str = "2 69015  97.4040  30.6820 0008982 156.4865 203.6783 15.21097552  1353";

    // RAO ground station, Calgary AB
    fn rao() -> GroundStation {
        GroundStation::new("RAO".to_string(), 50.8680, -114.2911, 1200.0, 0.0)
    }

    // 2026-05-12 00:00:00 UTC — just before the TLE epoch, so SGP4 extrapolation
    // is minimal and results are most accurate.
    //
    // Derivation: 2000-01-01 = 946684800; +26 years (7 leap) = +9497 days =
    // 946684800 + 820540800 = 1767225600 (2026-01-01); +131 days to May 12 =
    // 1767225600 + 11318400 = 1778544000.
    const START_UNIX: f64 = 1778544000.0;

    /// The corrected look_angles must find ≥2 passes per day over RAO.
    /// A 97.4° LEO at 15.2 rev/day reaches latitudes up to 82.6°, so Calgary
    /// (50.87°N) is well within view — expect ~3–5 visible passes per day.
    #[test]
    fn detects_passes_over_rao_72h() {
        let passes = find_passes_from(TLE1, TLE2, &[rao()], 72, 5.0, START_UNIX)
            .expect("pass prediction must not error");

        assert_eq!(passes.len(), 18);
    }

    /// Each detected pass must meet basic physical constraints.
    #[test]
    fn pass_fields_are_physically_valid() {
        let passes = find_passes_from(TLE1, TLE2, &[rao()], 24, 5.0, START_UNIX)
            .expect("pass prediction must not error");

        assert!(!passes.is_empty(), "expected at least one pass in 24h");

        for p in &passes {
            assert!(
                p.max_elev_deg >= 5.0,
                "max_elev_deg {:.2}° is below the 5° filter",
                p.max_elev_deg
            );
            assert!(p.duration_secs > 0, "pass duration must be > 0");
            assert!(p.los_utc > p.aos_utc, "LOS must be after AOS");
            assert!(
                p.max_utc >= p.aos_utc && p.max_utc <= p.los_utc,
                "MAX {} must lie between AOS {} and LOS {}",
                p.max_utc,
                p.aos_utc,
                p.los_utc
            );
            // Sanity-check elevation at AOS/LOS — should be near (not far below) threshold.
            assert!(
                p.aos_elev_deg >= 4.0,
                "AOS elevation {:.2}° suspiciously far below threshold",
                p.aos_elev_deg
            );
        }
    }

    /// Passes must be returned in ascending AOS order.
    #[test]
    fn passes_are_chronological() {
        let passes = find_passes_from(TLE1, TLE2, &[rao()], 72, 5.0, START_UNIX)
            .expect("pass prediction must not error");

        for w in passes.windows(2) {
            assert!(
                w[0].aos_utc <= w[1].aos_utc,
                "passes out of order: {} > {}",
                w[0].aos_utc,
                w[1].aos_utc
            );
        }
    }

    /// All passes must start within the requested search window.
    #[test]
    fn passes_within_search_window() {
        let hours = 24_u64;
        let end_unix = START_UNIX + hours as f64 * 3600.0;

        let passes = find_passes_from(TLE1, TLE2, &[rao()], hours, 5.0, START_UNIX)
            .expect("pass prediction must not error");

        for p in &passes {
            let aos_unix = p.aos_utc.timestamp() as f64;
            assert!(
                aos_unix >= START_UNIX,
                "AOS {} is before the search window start",
                p.aos_utc
            );
            assert!(
                aos_unix <= end_unix,
                "AOS {} is after the search window end",
                p.aos_utc
            );
        }
    }
}
