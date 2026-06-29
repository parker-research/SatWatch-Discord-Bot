// db.rs – SQLite persistence for ground stations, tracked satellites,
// subscriptions, and TLE cache for triggering pass notifications.
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

pub struct PassMessage {
    pub id: i64,
    pub channel_id: u64,
    pub message_id: u64,
    pub content: String,
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// Bump this whenever the schema changes.
#[allow(dead_code)]
const SCHEMA_VERSION: i64 = 5;

/// Full rebuild for any version < 2.  Creates the V2 schema (still includes
/// notified_passes; the V3 migration immediately replaces it on the same open).
const MIGRATION_V2_SQL: &str = "
    DROP TABLE IF EXISTS notified_passes;
    DROP TABLE IF EXISTS tracked_satellites;
    DROP TABLE IF EXISTS ground_stations;
    DROP TABLE IF EXISTS settings;
    DROP TABLE IF EXISTS tle_cache;
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

/// Incremental V2 → V3: replace per-pass de-duplication with TLE-based triggering.
const MIGRATION_V3_SQL: &str = "
    DROP TABLE IF EXISTS notified_passes;

    -- Stores the last TLE fetched per (subscription, satellite).
    -- A new TLE (different tle_updated value) triggers a fresh pass announcement.
    CREATE TABLE IF NOT EXISTS tle_cache (
        subscription_id INTEGER NOT NULL REFERENCES subscriptions(id) ON DELETE CASCADE,
        norad_id        INTEGER NOT NULL,
        tle_updated     TEXT    NOT NULL,
        tle_line1       TEXT    NOT NULL,
        tle_line2       TEXT    NOT NULL,
        cached_at       INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
        PRIMARY KEY (subscription_id, norad_id)
    );

    UPDATE schema_version SET version = 3;
";

/// V3 → V4: add pass_messages table for per-pass Discord message tracking.
const MIGRATION_V4_SQL: &str = "
    CREATE TABLE IF NOT EXISTS pass_messages (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        channel_id  INTEGER NOT NULL,
        message_id  INTEGER NOT NULL,
        los_unix    INTEGER NOT NULL,
        content     TEXT    NOT NULL,
        struck      INTEGER NOT NULL DEFAULT 0,
        UNIQUE(channel_id, message_id)
    );
    UPDATE schema_version SET version = 4;
";

/// V4 → V5: rebuild pass_messages with pass-identity columns so the background
/// task can find and edit individual-pass messages when TLEs are updated.
/// Identity columns (subscription_id, norad_id, station) are nullable to
/// accommodate ad-hoc slash-command messages that have no subscription context.
const MIGRATION_V5_SQL: &str = "
    DROP TABLE IF EXISTS pass_messages;
    CREATE TABLE pass_messages (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        subscription_id INTEGER REFERENCES subscriptions(id) ON DELETE CASCADE,
        norad_id        INTEGER,
        station         TEXT,
        aos_unix        INTEGER NOT NULL,
        channel_id      INTEGER NOT NULL,
        message_id      INTEGER NOT NULL,
        los_unix        INTEGER NOT NULL,
        content         TEXT    NOT NULL,
        struck          INTEGER NOT NULL DEFAULT 0,
        UNIQUE(subscription_id, norad_id, station, aos_unix),
        UNIQUE(channel_id, message_id)
    );
    UPDATE schema_version SET version = 5;
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

        let version: i64 = conn
            .query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
                row.get(0)
            })
            .unwrap_or(0);

        if version < 2 {
            conn.execute_batch(MIGRATION_V2_SQL)?;
        }
        if version < 3 {
            conn.execute_batch(MIGRATION_V3_SQL)?;
        }
        if version < 4 {
            conn.execute_batch(MIGRATION_V4_SQL)?;
        }
        if version < 5 {
            conn.execute_batch(MIGRATION_V5_SQL)?;
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

    /// List every subscription (used by the background fresh TLE checker).
    pub fn list_subscriptions(&self) -> Result<Vec<Subscription>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, guild_id, channel_id FROM subscriptions ORDER BY id")?;
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
    // TLE cache
    // -----------------------------------------------------------------------

    /// Return the `tle_updated` string of the last cached TLE for this
    /// (subscription, satellite) pair, or `None` if never cached.
    pub fn get_cached_tle_updated(
        &self,
        subscription_id: i64,
        norad_id: u64,
    ) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT tle_updated FROM tle_cache
                 WHERE subscription_id = ?1 AND norad_id = ?2",
                params![subscription_id, norad_id as i64],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Insert or update the cached TLE for this (subscription, satellite) pair.
    pub fn upsert_tle_cache(
        &self,
        subscription_id: i64,
        norad_id: u64,
        tle_updated: &str,
        tle_line1: &str,
        tle_line2: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tle_cache (subscription_id, norad_id, tle_updated, tle_line1, tle_line2)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(subscription_id, norad_id) DO UPDATE SET
                 tle_updated = excluded.tle_updated,
                 tle_line1   = excluded.tle_line1,
                 tle_line2   = excluded.tle_line2,
                 cached_at   = strftime('%s', 'now')",
            params![
                subscription_id,
                norad_id as i64,
                tle_updated,
                tle_line1,
                tle_line2
            ],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Pass message tracking (for strikethrough edits and TLE-update edits)
    // -----------------------------------------------------------------------

    /// Record a Discord message that displays a single pass.
    ///
    /// `subscription_id`, `norad_id`, and `station` are the pass identity used
    /// by the background task to match and edit messages when TLEs change.
    /// Pass `None` for all three when saving ad-hoc slash-command messages that
    /// have no subscription context (they will still be struck through later).
    #[allow(clippy::too_many_arguments)]
    pub fn save_pass_message(
        &self,
        subscription_id: Option<i64>,
        norad_id: Option<u64>,
        station: Option<&str>,
        aos_unix: i64,
        channel_id: u64,
        message_id: u64,
        los_unix: i64,
        content: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO pass_messages
             (subscription_id, norad_id, station, aos_unix, channel_id, message_id, los_unix, content)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                subscription_id,
                norad_id.map(|id| id as i64),
                station,
                aos_unix,
                channel_id as i64,
                message_id as i64,
                los_unix,
                content
            ],
        )?;
        Ok(())
    }

    /// Find a non-struck pass message by pass identity, matching AOS within
    /// `tolerance_secs` seconds.  Returns the closest match, or `None`.
    pub fn find_pass_message_near_aos(
        &self,
        subscription_id: i64,
        norad_id: u64,
        station: &str,
        aos_unix: i64,
        tolerance_secs: i64,
    ) -> Result<Option<PassMessage>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT id, channel_id, message_id, content
                 FROM pass_messages
                 WHERE subscription_id = ?1 AND norad_id = ?2 AND station = ?3
                   AND ABS(aos_unix - ?4) <= ?5
                   AND struck = 0
                 ORDER BY ABS(aos_unix - ?4) ASC
                 LIMIT 1",
                params![
                    subscription_id,
                    norad_id as i64,
                    station,
                    aos_unix,
                    tolerance_secs
                ],
                |row| {
                    let channel_id: i64 = row.get(1)?;
                    let message_id: i64 = row.get(2)?;
                    Ok(PassMessage {
                        id: row.get(0)?,
                        channel_id: channel_id as u64,
                        message_id: message_id as u64,
                        content: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Update a pass message's content and timing after a TLE revision shifts the pass.
    pub fn update_pass_message_content(
        &self,
        id: i64,
        new_aos_unix: i64,
        new_los_unix: i64,
        new_content: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE pass_messages SET aos_unix = ?1, los_unix = ?2, content = ?3 WHERE id = ?4",
            params![new_aos_unix, new_los_unix, new_content, id],
        )?;
        Ok(())
    }

    /// Return all pass messages whose LOS has already passed and that have not
    /// yet been struck through.
    pub fn get_expired_pass_messages(&self, now_unix: i64) -> Result<Vec<PassMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, channel_id, message_id, content
             FROM pass_messages
             WHERE struck = 0 AND los_unix <= ?1",
        )?;
        let rows = stmt
            .query_map(params![now_unix], |row| {
                let channel_id: i64 = row.get(1)?;
                let message_id: i64 = row.get(2)?;
                Ok(PassMessage {
                    id: row.get(0)?,
                    channel_id: channel_id as u64,
                    message_id: message_id as u64,
                    content: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Mark a pass message as struck through so the background checker won't
    /// try to edit it again.
    pub fn mark_pass_message_struck(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE pass_messages SET struck = 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }
}
