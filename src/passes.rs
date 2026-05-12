// passes.rs – Satellite pass prediction using satkit SGP4 propagation.
//
// Algorithm:
//   1. Parse TLE lines with satkit::TLE.
//   2. Step through time at 30 s resolution.
//   3. Accumulate samples while above min_elev_deg.
//   4. On falling edge, record the completed pass.
//
// Coordinate path:
//   SGP4 → TEME position → ITRF (via qteme2itrf quaternion) → ENU at ground station
//   EL  = atan2(Up, sqrt(East²+North²))
//   AZ  = atan2(East, North)  [measured clockwise from North]

use anyhow::{Result, anyhow};
use chrono::{DateTime, TimeZone, Utc};
use satkit::{Instant as SatInstant, TLE, frametransform, itrfcoord::ITRFCoord, sgp4::sgp4};
use tracing::trace;

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
#[allow(dead_code)]
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

    all_passes.sort_by_key(|a| a.aos_utc);
    Ok(all_passes)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Elevation and azimuth of a satellite TEME position as seen from a ground station.
///
/// Returns `(elevation_deg, azimuth_deg)`.
fn look_angles(teme_pos_m: &[f64; 3], t: &SatInstant, gs_itrf: &ITRFCoord) -> (f64, f64) {
    // Rotate TEME → ITRF.
    let teme_vec = satkit::Vector3::from(*teme_pos_m);

    let sat_itrf_matix = frametransform::qteme2itrf(t) * teme_vec;
    let sat_itrf = ITRFCoord::from_slice(sat_itrf_matix.as_slice()).unwrap();

    // Convert to ENU at the ground station (to_enu takes the satellite's absolute ITRF position).
    let enu = sat_itrf.to_enu(gs_itrf);
    let east = enu[0];
    let north = enu[1];
    let up = enu[2];

    let horiz = (east * east + north * north).sqrt();
    let elev_rad = up.atan2(horiz);
    // Azimuth: 0° = North, 90° = East, clockwise
    let az_rad = east.atan2(north);

    let elev_deg = elev_rad.to_degrees();
    trace!("elev_deg: {elev_deg} @ {t:?}");
    let az_deg = az_rad.to_degrees().rem_euclid(360.0);
    (elev_deg, az_deg)
}

/// Propagate TLE at time t, return TEME position in metres.
fn propagate_teme(tle: &mut TLE, t: &SatInstant) -> Result<[f64; 3]> {
    let states = sgp4(tle, &[*t]).map_err(|e| anyhow!("SGP4 error: {e}"))?;
    Ok([states.pos[(0, 0)], states.pos[(1, 0)], states.pos[(2, 0)]])
}

fn unixtime_to_instant(u: f64) -> SatInstant {
    SatInstant::from_unixtime(u)
}

fn instant_to_datetime(t: &SatInstant) -> DateTime<Utc> {
    let unix = t.as_unixtime();
    Utc.timestamp_opt(unix as i64, (unix.fract() * 1e9) as u32)
        .single()
        .unwrap_or_else(Utc::now)
}

// ---------------------------------------------------------------------------
// Core pass-finding logic
// ---------------------------------------------------------------------------

/// Tracks the in-progress state of a pass being accumulated.
struct PassBuilder {
    aos_unix: f64,
    aos_elev: f64,
    aos_az: f64,
    max_unix: f64,
    max_elev: f64,
    max_az: f64,
    last_unix: f64,
    last_elev: f64,
    last_az: f64,
}

impl PassBuilder {
    fn new(t: f64, elev: f64, az: f64) -> Self {
        Self {
            aos_unix: t,
            aos_elev: elev,
            aos_az: az,
            max_unix: t,
            max_elev: elev,
            max_az: az,
            last_unix: t,
            last_elev: elev,
            last_az: az,
        }
    }

    fn update(&mut self, t: f64, elev: f64, az: f64) {
        if elev > self.max_elev {
            self.max_elev = elev;
            self.max_az = az;
            self.max_unix = t;
        }
        self.last_unix = t;
        self.last_elev = elev;
        self.last_az = az;
    }

    fn finish(self, station_name: &str) -> Pass {
        let duration = (self.last_unix - self.aos_unix).max(0.0) as u64;
        Pass {
            station_name: station_name.to_string(),
            aos_utc: instant_to_datetime(&unixtime_to_instant(self.aos_unix)),
            aos_elev_deg: self.aos_elev,
            aos_az_deg: self.aos_az,
            max_utc: instant_to_datetime(&unixtime_to_instant(self.max_unix)),
            max_elev_deg: self.max_elev,
            max_az_deg: self.max_az,
            los_utc: instant_to_datetime(&unixtime_to_instant(self.last_unix)),
            los_elev_deg: self.last_elev,
            los_az_deg: self.last_az,
            duration_secs: duration,
        }
    }
}

fn predict_passes_for_station(
    tle: &mut TLE,
    gs_itrf: &ITRFCoord,
    station_name: &str,
    t_start: &SatInstant,
    t_end: &SatInstant,
    min_elev_deg: f64,
) -> Result<Vec<Pass>> {
    const STEP_TIME_SEC: f64 = 2.0;

    let unix_start = t_start.as_unixtime();
    let unix_end = t_end.as_unixtime();

    let mut passes: Vec<Pass> = Vec::new();
    let mut current_pass: Option<PassBuilder> = None;
    let mut t = unix_start;

    while t <= unix_end {
        let inst = unixtime_to_instant(t);
        let (elev, az) = match propagate_teme(tle, &inst) {
            Ok(pos) => look_angles(&pos, &inst, gs_itrf),
            Err(_) => {
                // Propagation failed — close any open pass and skip ahead.
                if let Some(builder) = current_pass.take()
                    && builder.max_elev >= min_elev_deg
                {
                    passes.push(builder.finish(station_name));
                }
                t += STEP_TIME_SEC;
                continue;
            }
        };

        trace!("t={t:.0} elev={elev:.2}° az={az:.1}°");

        match current_pass.as_mut() {
            None if elev >= min_elev_deg => {
                // Rising edge: start a new pass.
                current_pass = Some(PassBuilder::new(t, elev, az));
            }
            Some(builder) if elev >= min_elev_deg => {
                // Still in pass: update running max and last-seen point.
                builder.update(t, elev, az);
            }
            Some(_) => {
                // Falling edge: satellite dropped below threshold.
                if let Some(builder) = current_pass.take()
                    && builder.max_elev >= min_elev_deg
                {
                    passes.push(builder.finish(station_name));
                }
            }
            None => {} // Below threshold, no pass in progress.
        }

        t += STEP_TIME_SEC;
    }

    // Close any pass still open at the end of the window.
    if let Some(builder) = current_pass.take()
        && builder.max_elev >= min_elev_deg
    {
        passes.push(builder.finish(station_name));
    }

    Ok(passes)
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

    const START_UNIX: f64 = 1778618320.0;

    #[test]
    fn detects_passes_over_rao_72h() {
        let passes = find_passes_from(TLE1, TLE2, &[rao()], 72, 5.0, START_UNIX)
            .expect("pass prediction must not error");

        assert_eq!(passes.len(), 16);
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
