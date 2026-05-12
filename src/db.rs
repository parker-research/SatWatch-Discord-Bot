// db.rs – SQLite persistence for ground stations, tracked satellites,
// bot settings, and de-duplication of already-notified passes.
//
// All public methods are synchronous and should be called inside
// `tokio::task::spawn_blocking` from async code.

use anyhow::{Result, anyhow};
use rusqlite::{Connection, ErrorCode, OptionalExtension, params};
use std::sync::Mutex;

use crate::passes::GroundStation;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub struct TrackedSatellite {
    pub norad_id: u64,
    /// Friendly label stored at insert time; may be None.
    pub label: Option<String>,
}

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    /// Open (or create) the SQLite file at `path` and run schema migrations.
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;

            CREATE TABLE IF NOT EXISTS ground_stations (
                id      INTEGER PRIMARY KEY AUTOINCREMENT,
                name    TEXT    NOT NULL UNIQUE,
                lat_deg REAL    NOT NULL,
                lon_deg REAL    NOT NULL,
                elevation_m   REAL    NOT NULL,
                altitude_m    REAL    NOT NULL
            );

            CREATE TABLE IF NOT EXISTS tracked_satellites (
                id       INTEGER PRIMARY KEY AUTOINCREMENT,
                norad_id INTEGER NOT NULL UNIQUE,
                label    TEXT
            );

            CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            -- Tracks which (satellite, station, pass-AOS) tuples have already
            -- been announced so we never spam the same pass twice.
            CREATE TABLE IF NOT EXISTS notified_passes (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                norad_id    INTEGER NOT NULL,
                station     TEXT    NOT NULL,
                aos_unix    INTEGER NOT NULL,
                notified_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
                UNIQUE(norad_id, station, aos_unix)
            );
            ",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    // -----------------------------------------------------------------------
    // Ground stations
    // -----------------------------------------------------------------------

    pub fn add_station(
        &self,
        name: &str,
        lat_deg: f64,
        lon_deg: f64,
        elevation_m: f64,
        altitude_m: f64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        match conn.execute(
            "INSERT INTO ground_stations (name, lat_deg, lon_deg, elevation_m, altitude_m) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![name, lat_deg, lon_deg, elevation_m, altitude_m],
        ) {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == ErrorCode::ConstraintViolation =>
            {
                Err(anyhow!("A station named '{}' already exists.", name))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Returns `true` if a row was deleted, `false` if the name wasn't found.
    pub fn remove_station(&self, name: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM ground_stations WHERE name = ?1", params![name])?;
        Ok(n > 0)
    }

    pub fn list_stations(&self) -> Result<Vec<GroundStation>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT name, lat_deg, lon_deg, elevation_m, altitude_m FROM ground_stations ORDER BY name")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(GroundStation::new(
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // -----------------------------------------------------------------------
    // Tracked satellites
    // -----------------------------------------------------------------------

    pub fn add_satellite(&self, norad_id: u64, label: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        match conn.execute(
            "INSERT INTO tracked_satellites (norad_id, label) VALUES (?1, ?2)",
            params![norad_id as i64, label],
        ) {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == ErrorCode::ConstraintViolation =>
            {
                Err(anyhow!("NORAD {} is already being tracked.", norad_id))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Returns `true` if a row was deleted.
    pub fn remove_satellite(&self, norad_id: u64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM tracked_satellites WHERE norad_id = ?1",
            params![norad_id as i64],
        )?;
        Ok(n > 0)
    }

    pub fn list_satellites(&self) -> Result<Vec<TrackedSatellite>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT norad_id, label FROM tracked_satellites ORDER BY norad_id")?;
        let rows = stmt
            .query_map([], |row| {
                let norad_id: i64 = row.get(0)?;
                Ok(TrackedSatellite {
                    norad_id: norad_id as u64,
                    label: row.get(1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // -----------------------------------------------------------------------
    // Settings (key-value store)
    // -----------------------------------------------------------------------

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT value FROM settings WHERE key = ?1")?;
        Ok(stmt.query_row(params![key], |row| row.get(0)).optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Pass de-duplication
    // -----------------------------------------------------------------------

    pub fn is_pass_notified(&self, norad_id: u64, station: &str, aos_unix: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM notified_passes
             WHERE norad_id = ?1 AND station = ?2 AND aos_unix = ?3",
            params![norad_id as i64, station, aos_unix],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn mark_pass_notified(&self, norad_id: u64, station: &str, aos_unix: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO notified_passes (norad_id, station, aos_unix)
             VALUES (?1, ?2, ?3)",
            params![norad_id as i64, station, aos_unix],
        )?;
        Ok(())
    }

    /// Delete notified-pass records whose AOS is older than `cutoff_unix`.
    pub fn cleanup_old_notified_passes(&self, cutoff_unix: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM notified_passes WHERE aos_unix < ?1",
            params![cutoff_unix],
        )?;
        Ok(n)
    }
}
