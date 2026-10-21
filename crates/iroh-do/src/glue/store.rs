//! SQLite access for one `room` object. The schema and statements are in
//! [`crate::schema`].
//!
//! Every call is a synchronous `sql.exec`; nothing awaits storage. Nothing
//! writes until an identity exists or is being minted, so a request for an
//! unminted name leaves no tables behind (spec §13.4.5).

use iroh_base::SecretKey;
use serde::Deserialize;
use worker::{Result, SqlStorage, SqlStorageValue};

use iroh_do_core::{LOG_MAX_AGE, LOG_RETENTION};

use crate::schema;

/// One row of the `log` table.
#[derive(Debug, Deserialize)]
pub struct LogRowSql {
    /// Sequence number.
    pub seq: i64,
    /// `seq` of the previous room event; absent from queries over all records.
    #[serde(default)]
    pub prev: i64,
    /// Insert time (ms since the UNIX epoch).
    pub ts_ms: i64,
    /// Peer z32.
    pub peer: String,
    /// Record kind.
    pub kind: String,
    /// Author's nick (room events only).
    pub nick: String,
    /// Payload.
    pub text: String,
}

/// One row of the `members` table.
#[derive(Debug, Deserialize)]
pub struct MemberSql {
    /// Peer z32.
    pub peer: String,
    /// Nick.
    pub nick: String,
    /// 1 if joined.
    pub joined: i64,
}

/// The stored identity.
pub struct Identity {
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
    /// Entry key (`republish`, `callback:<z32>`, ...).
    pub key: String,
    /// Due time the entry was read with; [`Store::claim`] removes only this version.
    pub due_ms: f64,
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
                "SELECT secret_hex, relay_url, pkarr_relay FROM identity WHERE k = 1",
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
            secret: SecretKey::from_bytes(&arr),
            relay_url: row.relay_url,
            pkarr_relay: row.pkarr_relay,
        }))
    }

    /// Creates the schema if it does not exist. Only call with an identity.
    ///
    /// There is no upgrade path: an object created with an older schema keeps it (design
    /// note 008, Storage).
    pub fn create_schema(&self) -> Result<()> {
        for stmt in schema::SCHEMA {
            self.sql.exec(stmt, None)?;
        }
        Ok(())
    }

    /// Appends a log record and enforces retention; returns its `seq` and `prev`.
    ///
    /// A room event's `prev` is the previous room event's `seq`, and the event becomes the
    /// new `room_seq`; other records have `prev` 0 and leave `room_seq` alone.
    pub fn log_insert(
        &self,
        ts_ms: f64,
        peer: &str,
        kind: &str,
        nick: &str,
        text: &str,
    ) -> Result<(i64, i64)> {
        let room_event = iroh_do_core::ROOM_EVENT_KINDS.contains(&kind);
        let prev = if room_event { self.room_seq()? } else { 0 };
        let seq = self.value(
            schema::LOG_INSERT,
            Some(vec![
                SqlStorageValue::Integer(ts_ms as i64),
                peer.into(),
                kind.into(),
                nick.into(),
                text.into(),
                SqlStorageValue::Integer(prev),
            ]),
        )?;
        if room_event {
            self.sql
                .exec(schema::SET_ROOM_SEQ, vec![SqlStorageValue::Integer(seq)])?;
        }
        // Same event as the INSERT, so they commit together.
        self.log_prune(ts_ms)?;
        Ok((seq, prev))
    }

    /// The newest room event's `seq`, 0 if none.
    pub fn room_seq(&self) -> Result<i64> {
        self.value(schema::ROOM_SEQ, None)
    }

    /// Deletes records beyond [`LOG_RETENTION`] or older than [`LOG_MAX_AGE`] at `now_ms`.
    /// `seq` and the total stay monotonic.
    pub fn log_prune(&self, now_ms: f64) -> Result<()> {
        let newest = self.log_total()?;
        self.sql.exec(
            schema::LOG_PRUNE_COUNT,
            vec![SqlStorageValue::Integer(newest - LOG_RETENTION as i64)],
        )?;
        let cutoff = SqlStorageValue::Integer(now_ms as i64 - LOG_MAX_AGE.as_millis() as i64);
        self.sql.exec(schema::LOG_SAVE_SEQ, vec![cutoff.clone()])?;
        self.sql.exec(schema::LOG_PRUNE_AGE, vec![cutoff])?;
        Ok(())
    }

    fn value(&self, sql: &str, params: Option<Vec<SqlStorageValue>>) -> Result<i64> {
        let rows: Vec<ValueRow> = self.sql.exec(sql, params)?.to_array()?;
        Ok(rows.first().map_or(0, |r| r.value))
    }

    /// Records ever inserted (`max(seq)`).
    pub fn log_total(&self) -> Result<i64> {
        self.value(schema::LOG_TOTAL, None)
    }

    /// Retained records of `peer`, counted on the `log_peer` index.
    pub fn log_yours(&self, peer: &str) -> Result<i64> {
        self.value(schema::LOG_YOURS, Some(vec![peer.into()]))
    }

    /// Distinct peers in retained records (`/count` only).
    pub fn log_peers(&self) -> Result<i64> {
        self.value(schema::LOG_PEERS, None)
    }

    /// Newest `n` records, oldest first.
    pub fn log_recent(&self, n: usize) -> Result<Vec<LogRowSql>> {
        self.sql
            .exec(schema::LOG_RECENT, vec![SqlStorageValue::Integer(n as i64)])?
            .to_array()
    }

    /// Up to `limit` room events after `seq`, oldest first.
    pub fn events_after(&self, seq: i64, limit: usize) -> Result<Vec<LogRowSql>> {
        self.sql
            .exec(
                &schema::events_after(),
                vec![
                    SqlStorageValue::Integer(seq),
                    SqlStorageValue::Integer(limit as i64),
                ],
            )?
            .to_array()
    }

    /// Newest `n` room events, oldest first.
    pub fn events_recent(&self, n: usize) -> Result<Vec<LogRowSql>> {
        self.sql
            .exec(
                &schema::events_recent(),
                vec![SqlStorageValue::Integer(n as i64)],
            )?
            .to_array()
    }

    /// One member, joined or not.
    pub fn member(&self, peer: &str) -> Result<Option<MemberSql>> {
        let rows: Vec<MemberSql> = self
            .sql
            .exec(schema::MEMBER_GET, vec![peer.into()])?
            .to_array()?;
        Ok(rows.into_iter().next())
    }

    /// Joined members.
    pub fn members(&self) -> Result<Vec<MemberSql>> {
        self.sql.exec(schema::MEMBERS_JOINED, None)?.to_array()
    }

    /// Inserts or updates a member.
    pub fn set_member(&self, peer: &str, nick: &str, joined: bool, now_ms: f64) -> Result<()> {
        self.sql.exec(
            schema::MEMBER_UPSERT,
            vec![
                peer.into(),
                nick.into(),
                SqlStorageValue::Integer(i64::from(joined)),
                SqlStorageValue::Integer(now_ms as i64),
            ],
        )?;
        Ok(())
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
        self.create_schema()?;
        let hex = data_encoding::HEXLOWER.encode(&id.secret.to_bytes());
        self.sql.exec(
            "INSERT INTO identity(k, secret_hex, relay_url, pkarr_relay, minted_at_ms) VALUES (1, ?, ?, ?, ?)",
            vec![
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

    /// Upsert a schedule entry.
    pub fn schedule(&self, key: &str, due_ms: f64, payload: &str) -> Result<()> {
        self.sql.exec(
            "INSERT INTO schedule(key, due_ms, payload) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET due_ms = excluded.due_ms, payload = excluded.payload",
            vec![key.into(), due_ms.into(), payload.into()],
        )?;
        Ok(())
    }

    /// Upsert an entry, keeping the earlier due time if one exists.
    pub fn schedule_earliest(&self, key: &str, due_ms: f64) -> Result<()> {
        self.sql.exec(
            "INSERT INTO schedule(key, due_ms, payload) VALUES (?, ?, '')
             ON CONFLICT(key) DO UPDATE SET due_ms = min(due_ms, excluded.due_ms)",
            vec![key.into(), due_ms.into()],
        )?;
        Ok(())
    }

    /// Entries due at or before `now_ms`, left in place: each is removed by
    /// [`Store::claim`] just before it runs, so a failure part-way loses nothing.
    pub fn due(&self, now_ms: f64) -> Result<Vec<Due>> {
        self.sql
            .exec(
                "SELECT key, due_ms FROM schedule WHERE due_ms <= ? ORDER BY due_ms",
                vec![SqlStorageValue::Float(now_ms)],
            )?
            .to_array()
    }

    /// Remove `d` if it is still the version that was read (a handler may have
    /// rescheduled the key since).
    pub fn claim(&self, d: &Due) -> Result<()> {
        self.sql.exec(
            "DELETE FROM schedule WHERE key = ? AND due_ms = ?",
            vec![d.key.as_str().into(), SqlStorageValue::Float(d.due_ms)],
        )?;
        Ok(())
    }

    /// Whether an entry exists for `key`.
    pub fn scheduled(&self, key: &str) -> Result<bool> {
        let rows: Vec<ValueRow> = self
            .sql
            .exec(
                "SELECT count(*) AS value FROM schedule WHERE key = ?",
                vec![key.into()],
            )?
            .to_array()?;
        Ok(rows.first().is_some_and(|r| r.value > 0))
    }

    /// Earliest pending entry.
    pub fn next_due(&self) -> Result<Option<f64>> {
        let rows: Vec<MinRow> = self
            .sql
            .exec("SELECT min(due_ms) AS due FROM schedule", None)?
            .to_array()?;
        Ok(rows.first().and_then(|r| r.due))
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
