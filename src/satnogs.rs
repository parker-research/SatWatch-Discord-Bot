// satnogs.rs – Fetch the latest TLE for a satellite from SatNOGS DB API
//
// SatNOGS public API endpoint:
//   https://db.satnogs.org/api/tle/?norad_cat_id=<ID>&format=json
//
// Returns the most recent TLE record(s) for that NORAD ID.

use anyhow::{anyhow, Result};
use serde::Deserialize;

/// Minimal representation of what SatNOGS returns for a TLE entry.
#[derive(Debug, Deserialize, Clone)]
pub struct SatNogsTleEntry {
    /// Satellite name from SatNOGS
    pub tle0: String,
    pub tle1: String,
    pub tle2: String,
    /// ISO-8601 timestamp of when this TLE was updated in SatNOGS
    pub updated: String,
}

/// Friendly wrapper around the raw SatNOGS response.
#[derive(Debug, Clone)]
pub struct TleInfo {
    pub name: String,
    pub line1: String,
    pub line2: String,
    /// Human-readable update timestamp
    pub updated: String,
}

/// Fetch the latest TLE for `norad_id` from the SatNOGS public API.
///
/// SatNOGS sorts results newest-first, so we take index [0].
pub async fn fetch_tle(norad_id: u64) -> Result<TleInfo> {
    let url = format!(
        "https://db.satnogs.org/api/tle/?norad_cat_id={norad_id}&format=json"
    );

    let response = reqwest::get(&url).await?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "SatNOGS returned HTTP {} for NORAD {norad_id}",
            response.status()
        ));
    }

    let entries: Vec<SatNogsTleEntry> = response.json().await?;

    let entry = entries
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("No TLE found in SatNOGS for NORAD {norad_id}"))?;

    // tle0 is typically "SATELLITE NAME" or "0 SATELLITE NAME" depending on SatNOGS version
    let name = entry.tle0.trim_start_matches("0 ").trim().to_string();

    Ok(TleInfo {
        name,
        line1: entry.tle1,
        line2: entry.tle2,
        updated: entry.updated,
    })
}
