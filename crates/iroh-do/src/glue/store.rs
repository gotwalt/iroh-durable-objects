//! SQLite access for one `room` object. The schema and statements are in
//! [`crate::schema`].
//!
//! Every call is a synchronous `sql.exec`; nothing awaits storage. Nothing
//! writes until an identity exists or is being minted, so a request for an
//! unminted name leaves no tables behind (spec §13.4.5).

use iroh_base::SecretKey;
use serde::Deserialize;
use worker::{Result, SqlStorage, SqlStorageValue};

use crate::schema;

/// The stored identity.
pub struct Identity {
    /// Name of the app the endpoint runs.
    pub app: String,
    /// Endpoint secret key.
    pub secret: SecretKey,
    /// The relay URL published for this endpoint.
    pub relay_url: String,
    /// pkarr relay base URL used at mint (empty: publishing disabled).
    pub pkarr_relay: String,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("public", &self.secret.public().to_z32())
            .field("app", &self.app)
            .field("relay_url", &self.relay_url)
            .finish_non_exhaustive()
    }
}

/// Thin wrapper over the DO's SQLite handle.
#[derive(Clone)]
pub struct Store {
    sql: SqlStorage,
}

#[derive(Deserialize)]
struct IdentityRow {
    app: String,
    secret_hex: String,
    relay_url: String,
    pkarr_relay: String,
}

#[derive(Deserialize)]
struct CountRow {
    n: i64,
}

#[derive(Deserialize)]
struct ValueRow {
    value: i64,
}

/// A due schedule entry.
#[derive(Debug, Deserialize)]
pub struct Due {
    /// Entry key (`republish`, `sweep`, or an app key).
    pub key: String,
    /// Due time the entry was read with; [`Store::lease`] and [`Store::claim`] act only on
    /// this version.
    pub due_ms: f64,
    /// The payload the entry was scheduled with.
    pub payload: String,
    /// Runs of this entry that did not complete.
    pub attempt: u32,
}

#[derive(Deserialize)]
struct MinRow {
    due: Option<f64>,
}

impl Store {
    /// Wrap a SQLite handle.
    pub fn new(sql: SqlStorage) -> Self {
        Self { sql }
    }

    /// Read-only: is there an identity table (and so, possibly, an identity)?
    fn has_schema(&self) -> Result<bool> {
        let rows: Vec<CountRow> = self
            .sql
            .exec(
                "SELECT count(*) AS n FROM sqlite_master WHERE type = 'table' AND name = 'identity'",
                None,
            )?
            .to_array()?;
        Ok(rows.first().is_some_and(|r| r.n > 0))
    }

    /// Load the identity without writing anything.
    pub fn load_identity(&self) -> Result<Option<Identity>> {
        if !self.has_schema()? {
            return Ok(None);
        }
        let rows: Vec<IdentityRow> = self
            .sql
            .exec(
                "SELECT app, secret_hex, relay_url, pkarr_relay FROM identity WHERE k = 1",
                None,
            )?
            .to_array()?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let bytes = data_encoding::HEXLOWER
            .decode(row.secret_hex.as_bytes())
            .map_err(|_| worker::Error::RustError("stored secret is not hex".into()))?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| worker::Error::RustError("stored secret has wrong length".into()))?;
        Ok(Some(Identity {
            app: row.app,
            secret: SecretKey::from_bytes(&arr),
            relay_url: row.relay_url,
            pkarr_relay: row.pkarr_relay,
        }))
    }

    /// Creates the library's tables and then `app`'s, if they do not exist. Only call with
    /// an identity.
    ///
    /// The library's one upgrade, [`schema::SCHEDULE_ADD_ATTEMPT`], runs before the app's
    /// tables; the app's tables have no upgrade path (design note 008, Storage).
    pub fn create_schema(&self, app: &[&str]) -> Result<()> {
        for stmt in schema::SCHEMA {
            self.sql.exec(stmt, None)?;
        }
        if let Err(e) = self.sql.exec(schema::SCHEDULE_ADD_ATTEMPT, None)
            && !e.to_string().contains(schema::DUPLICATE_COLUMN)
        {
            return Err(e);
        }
        for stmt in app {
            self.sql.exec(stmt, None)?;
        }
        Ok(())
    }

    /// The SQLite handle, for the app's [`iroh_do_core::Sql`].
    pub fn sql(&self) -> &SqlStorage {
        &self.sql
    }

    fn value(&self, sql: &str, params: Option<Vec<SqlStorageValue>>) -> Result<i64> {
        let rows: Vec<ValueRow> = self.sql.exec(sql, params)?.to_array()?;
        Ok(rows.first().map_or(0, |r| r.value))
    }

    /// Mint time of the stored identity, in ms since the UNIX epoch.
    pub fn minted_at(&self) -> Result<i64> {
        self.value(
            "SELECT CAST(minted_at_ms AS INTEGER) AS value FROM identity WHERE k = 1",
            None,
        )
    }

    /// Whether this endpoint is recorded in the registry (design note 009).
    pub fn registered(&self) -> Result<bool> {
        Ok(self.value(
            "SELECT count(*) AS value FROM counters WHERE name = 'registered'",
            None,
        )? > 0)
    }

    /// Records that the registry has this endpoint.
    pub fn set_registered(&self) -> Result<()> {
        self.sql.exec(
            "INSERT INTO counters(name, value) VALUES ('registered', 1) ON CONFLICT(name) DO NOTHING",
            None,
        )?;
        Ok(())
    }

    /// Store a fresh identity. Fails if one exists (`INSERT`, not upsert).
    pub fn insert_identity(&self, id: &Identity, now_ms: f64) -> Result<()> {
        self.create_schema(&[])?;
        let hex = data_encoding::HEXLOWER.encode(&id.secret.to_bytes());
        self.sql.exec(
            "INSERT INTO identity(k, app, secret_hex, relay_url, pkarr_relay, minted_at_ms) VALUES (1, ?, ?, ?, ?, ?)",
            vec![
                id.app.clone().into(),
                hex.into(),
                id.relay_url.clone().into(),
                id.pkarr_relay.clone().into(),
                now_ms.into(),
            ],
        )?;
        Ok(())
    }

    /// Increment a counter and return the new value. Only call with an identity.
    pub fn bump(&self, name: &str) -> Result<i64> {
        let rows: Vec<ValueRow> = self
            .sql
            .exec(
                "INSERT INTO counters(name, value) VALUES (?, 1)
                 ON CONFLICT(name) DO UPDATE SET value = value + 1
                 RETURNING value",
                vec![name.into()],
            )?
            .to_array()?;
        Ok(rows.first().map_or(0, |r| r.value))
    }

    /// Read a counter (0 if unset).
    pub fn counter(&self, name: &str) -> Result<i64> {
        let rows: Vec<ValueRow> = self
            .sql
            .exec(
                "SELECT value FROM counters WHERE name = ?",
                vec![name.into()],
            )?
            .to_array()?;
        Ok(rows.first().map_or(0, |r| r.value))
    }

    /// Upsert a schedule entry; its attempt count starts over.
    pub fn schedule(&self, key: &str, due_ms: f64, payload: &str) -> Result<()> {
        self.sql.exec(
            schema::SCHEDULE_UPSERT,
            vec![key.into(), due_ms.into(), payload.into()],
        )?;
        Ok(())
    }

    /// Upsert an entry, keeping the earlier due time if one exists.
    pub fn schedule_earliest(&self, key: &str, due_ms: f64) -> Result<()> {
        self.sql
            .exec(schema::SCHEDULE_EARLIEST, vec![key.into(), due_ms.into()])?;
        Ok(())
    }

    /// Entries due at or before `now_ms`, left in place: [`Store::lease`] moves each to its
    /// retry time before it runs, and [`Store::claim`] removes it after it succeeded.
    pub fn due(&self, now_ms: f64) -> Result<Vec<Due>> {
        self.sql
            .exec(schema::SCHEDULE_DUE, vec![SqlStorageValue::Float(now_ms)])?
            .to_array()
    }

    /// Moves `d` to `retry_ms` and counts the attempt, so that it runs again if this run
    /// fails or never completes. Returns the leased entry, `None` if `d` changed since it
    /// was read.
    pub fn lease(&self, d: &Due, retry_ms: f64) -> Result<Option<Due>> {
        let rows: Vec<Due> = self
            .sql
            .exec(
                schema::SCHEDULE_LEASE,
                vec![
                    SqlStorageValue::Float(retry_ms),
                    d.key.as_str().into(),
                    SqlStorageValue::Float(d.due_ms),
                ],
            )?
            .to_array()?;
        Ok(rows.into_iter().next())
    }

    /// Removes the leased entry `d` after its work succeeded, unless it was scheduled again
    /// meanwhile.
    pub fn claim(&self, d: &Due) -> Result<()> {
        self.sql.exec(
            schema::SCHEDULE_CLAIM,
            vec![d.key.as_str().into(), SqlStorageValue::Float(d.due_ms)],
        )?;
        Ok(())
    }

    /// Deletes the entry for `key`, if any.
    pub fn unschedule(&self, key: &str) -> Result<()> {
        self.sql
            .exec("DELETE FROM schedule WHERE key = ?", vec![key.into()])?;
        Ok(())
    }

    /// Earliest pending entry.
    pub fn next_due(&self) -> Result<Option<f64>> {
        let rows: Vec<MinRow> = self
            .sql
            .exec("SELECT min(due_ms) AS due FROM schedule", None)?
            .to_array()?;
        Ok(rows.first().and_then(|r| r.due))
    }

    /// Sets counter `name` to `value`.
    pub fn set_counter(&self, name: &str, value: i64) -> Result<()> {
        self.sql.exec(
            "INSERT INTO counters(name, value) VALUES (?, ?)
             ON CONFLICT(name) DO UPDATE SET value = excluded.value",
            vec![name.into(), SqlStorageValue::Integer(value)],
        )?;
        Ok(())
    }

    /// Reads counter `name`, `None` if it is unset.
    pub fn counter_opt(&self, name: &str) -> Result<Option<i64>> {
        let rows: Vec<ValueRow> = self
            .sql
            .exec(
                "SELECT value FROM counters WHERE name = ?",
                vec![name.into()],
            )?
            .to_array()?;
        Ok(rows.first().map(|r| r.value))
    }

    /// Deletes counter `name`.
    pub fn delete_counter(&self, name: &str) -> Result<()> {
        self.sql
            .exec("DELETE FROM counters WHERE name = ?", vec![name.into()])?;
        Ok(())
    }

    /// Record the outcome of a pkarr publish attempt; returns consecutive failures.
    pub fn pkarr_result(&self, ok: bool, status: &str, now_ms: f64) -> Result<i64> {
        let rows: Vec<ValueRow> = self
            .sql
            .exec(
                "INSERT INTO pkarr_state(k, last_ok_ms, last_status, failures) VALUES (1, ?, ?, ?)
                 ON CONFLICT(k) DO UPDATE SET
                   last_ok_ms = coalesce(excluded.last_ok_ms, last_ok_ms),
                   last_status = excluded.last_status,
                   failures = CASE WHEN excluded.failures = 0 THEN 0 ELSE failures + 1 END
                 RETURNING failures AS value",
                vec![
                    if ok {
                        SqlStorageValue::Float(now_ms)
                    } else {
                        SqlStorageValue::Null
                    },
                    status.into(),
                    SqlStorageValue::Integer(if ok { 0 } else { 1 }),
                ],
            )?
            .to_array()?;
        Ok(rows.first().map_or(0, |r| r.value))
    }
}
