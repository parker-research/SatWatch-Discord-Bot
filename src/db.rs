// db.rs – SQLite persistence for ground stations, tracked satellites,
// and de-duplication of already-notified passes, scoped per Discord channel subscription.
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

pub struct Subscription {
    pub id: i64,
    #[allow(dead_code)]
    pub guild_id: u64,
    pub channel_id: u64,
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// Bump this whenever the schema changes incompatibly.
const SCHEMA_VERSION: i64 = 2;

/// Drops the old V1 tables (if any) and creates the V2 schema from scratch.
/// Called once on open when the stored version is below SCHEMA_VERSION.
const MIGRATION_SQL: &str = "
    DROP TABLE IF EXISTS notified_passes;
    DROP TABLE IF EXISTS tracked_satellites;
    DROP TABLE IF EXISTS ground_stations;
    DROP TABLE IF EXISTS settings;
    DROP TABLE IF EXISTS schema_version;

    CREATE TABLE schema_version (version INTEGER NOT NULL);
    INSERT INTO schema_version VALUES (2);

    -- One row per Discord channel that has opted into pass notifications.
    CREATE TABLE subscriptions (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        guild_id   INTEGER NOT NULL,
        channel_id INTEGER NOT NULL,
        created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
        UNIQUE(channel_id)
    );

    -- Ground stations are scoped to a single channel subscription.
    CREATE TABLE ground_stations (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        subscription_id INTEGER NOT NULL REFERENCES subscriptions(id) ON DELETE CASCADE,
        name            TEXT    NOT NULL,
        lat_deg         REAL    NOT NULL,
        lon_deg         REAL    NOT NULL,
        elevation_m     REAL    NOT NULL,
        altitude_m      REAL    NOT NULL,
        UNIQUE(subscription_id, name)
    );

    -- Tracked satellites are scoped to a single channel subscription.
    CREATE TABLE tracked_satellites (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        subscription_id INTEGER NOT NULL REFERENCES subscriptions(id) ON DELETE CASCADE,
        norad_id        INTEGER NOT NULL,
        label           TEXT,
        UNIQUE(subscription_id, norad_id)
    );

    -- De-duplication: (subscription, sat, station, AOS) tuples that have already been announced.
    CREATE TABLE notified_passes (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        subscription_id INTEGER NOT NULL REFERENCES subscriptions(id) ON DELETE CASCADE,
        norad_id        INTEGER NOT NULL,
        station         TEXT    NOT NULL,
        aos_unix        INTEGER NOT NULL,
        notified_at     INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
        UNIQUE(subscription_id, norad_id, station, aos_unix)
    );
";

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
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;

        let current_version: i64 = conn
            .query_row(
                "SELECT version FROM schema_version LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if current_version < SCHEMA_VERSION {
            conn.execute_batch(MIGRATION_SQL)?;
        }

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    // -----------------------------------------------------------------------
    // Subscriptions
    // -----------------------------------------------------------------------

    /// Get or create the subscription for a (guild, channel) pair.
    /// Returns the subscription's row id.
    pub fn ensure_subscription(&self, guild_id: u64, channel_id: u64) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO subscriptions (guild_id, channel_id) VALUES (?1, ?2)",
            params![guild_id as i64, channel_id as i64],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM subscriptions WHERE channel_id = ?1",
            params![channel_id as i64],
            |row| row.get(0),
        )?)
    }

    /// Return the subscription id for a channel, or `None` if it has never been configured.
    pub fn get_subscription_id(&self, channel_id: u64) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT id FROM subscriptions WHERE channel_id = ?1",
                params![channel_id as i64],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// List every subscription (used by the background pass checker).
    pub fn list_subscriptions(&self) -> Result<Vec<Subscription>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, guild_id, channel_id FROM subscriptions ORDER BY id")?;
        let rows = stmt
            .query_map([], |row| {
                let guild_id: i64 = row.get(1)?;
                let channel_id: i64 = row.get(2)?;
                Ok(Subscription {
                    id: row.get(0)?,
                    guild_id: guild_id as u64,
                    channel_id: channel_id as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // -----------------------------------------------------------------------
    // Ground stations
    // -----------------------------------------------------------------------

    pub fn add_station(
        &self,
        subscription_id: i64,
        name: &str,
        lat_deg: f64,
        lon_deg: f64,
        elevation_m: f64,
        altitude_m: f64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        match conn.execute(
            "INSERT INTO ground_stations (subscription_id, name, lat_deg, lon_deg, elevation_m, altitude_m)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![subscription_id, name, lat_deg, lon_deg, elevation_m, altitude_m],
        ) {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == ErrorCode::ConstraintViolation =>
            {
                Err(anyhow!("A station named '{}' already exists in this channel.", name))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Returns `true` if a row was deleted, `false` if the name wasn't found.
    pub fn remove_station(&self, subscription_id: i64, name: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM ground_stations WHERE subscription_id = ?1 AND name = ?2",
            params![subscription_id, name],
        )?;
        Ok(n > 0)
    }

    pub fn list_stations(&self, subscription_id: i64) -> Result<Vec<GroundStation>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, lat_deg, lon_deg, elevation_m, altitude_m
             FROM ground_stations WHERE subscription_id = ?1 ORDER BY name",
        )?;
        let rows = stmt
            .query_map(params![subscription_id], |row| {
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

    pub fn add_satellite(
        &self,
        subscription_id: i64,
        norad_id: u64,
        label: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        match conn.execute(
            "INSERT INTO tracked_satellites (subscription_id, norad_id, label) VALUES (?1, ?2, ?3)",
            params![subscription_id, norad_id as i64, label],
        ) {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == ErrorCode::ConstraintViolation =>
            {
                Err(anyhow!(
                    "NORAD {} is already being tracked in this channel.",
                    norad_id
                ))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Returns `true` if a row was deleted.
    pub fn remove_satellite(&self, subscription_id: i64, norad_id: u64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM tracked_satellites WHERE subscription_id = ?1 AND norad_id = ?2",
            params![subscription_id, norad_id as i64],
        )?;
        Ok(n > 0)
    }

    pub fn list_satellites(&self, subscription_id: i64) -> Result<Vec<TrackedSatellite>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT norad_id, label FROM tracked_satellites
             WHERE subscription_id = ?1 ORDER BY norad_id",
        )?;
        let rows = stmt
            .query_map(params![subscription_id], |row| {
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
    // Pass de-duplication
    // -----------------------------------------------------------------------

    pub fn is_pass_notified(
        &self,
        subscription_id: i64,
        norad_id: u64,
        station: &str,
        aos_unix: i64,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM notified_passes
             WHERE subscription_id = ?1 AND norad_id = ?2 AND station = ?3 AND aos_unix = ?4",
            params![subscription_id, norad_id as i64, station, aos_unix],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn mark_pass_notified(
        &self,
        subscription_id: i64,
        norad_id: u64,
        station: &str,
        aos_unix: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO notified_passes (subscription_id, norad_id, station, aos_unix)
             VALUES (?1, ?2, ?3, ?4)",
            params![subscription_id, norad_id as i64, station, aos_unix],
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
