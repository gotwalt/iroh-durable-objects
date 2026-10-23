//! The chat's tables and the statements it runs, over [`Sql`].
//!
//! - `log`: the message log (design notes 007 and 008), keeping the newest
//!   [`LOG_RETENTION`] rows and none older than [`LOG_MAX_AGE`]. Room events and the
//!   diagnostic records (`ctl`, `echo`, `callback`) share it. `seq` numbers every row ever
//!   written; see [`LOG_TOTAL`]. A room event's `prev` is the `seq` of the room event before
//!   it.
//! - `members`: the room's members (design note 008): nick, whether the member is joined,
//!   and when it was first and last seen.
//! - `log_meta`: named integers. `log_seq`, the largest `seq` of a pruned log that was left
//!   empty; `room_seq`, the `seq` of the newest room event, which becomes the next room
//!   event's `prev`; `prune_due_ms`, when the prune alarm is due.
//!
//! Every insert prunes. A log that stops growing is pruned once more by an alarm
//! [`PRUNE_DELAY`] after its newest record. An insert does not move that alarm, which would
//! write the schedule and re-arm the alarm per message: it arms it only if none is pending,
//! and the alarm, when it fires, moves itself to [`PRUNE_DELAY`] after the newest record left
//! ([`prune_due`]).
//!
//! The schema is created as is, with no versioning or upgrade path: changing it means
//! resetting the object's state.

use std::time::Duration;

use iroh_do_core::{EndpointId, Sql, SqlError, Value};
use n0_future::time::SystemTime;

use crate::{LogKind, LogRow, Member};

/// Records a host retains; older ones are deleted on insert.
pub const LOG_RETENTION: u64 = 10_000;
/// Age after which a record is deleted, on insert and from the daily prune.
pub const LOG_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 3600);

/// Schedule key of the prune of a log that stopped growing.
pub const PRUNE_KEY: &str = "prune";
/// Delay of that prune after the newest record: when the record expires, and a minute more.
pub const PRUNE_DELAY: Duration = Duration::from_secs(LOG_MAX_AGE.as_secs() + 60);

/// The tables and indexes, created idempotently on every wake.
pub const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS log(
        seq INTEGER PRIMARY KEY,
        ts_ms INTEGER NOT NULL,
        peer TEXT NOT NULL,
        kind TEXT NOT NULL,
        nick TEXT NOT NULL DEFAULT '',
        text TEXT NOT NULL,
        prev INTEGER NOT NULL DEFAULT 0)",
    "CREATE INDEX IF NOT EXISTS log_peer ON log(peer)",
    // The room events among the records, for `/join`, `/history` and `/since`, which would
    // otherwise walk the diagnostic records too: up to the whole log when it holds few room
    // events. SQLite uses a partial index for a query whose WHERE repeats the index's term.
    "CREATE INDEX IF NOT EXISTS log_room ON log(seq) WHERE kind IN ('msg', 'act', 'join', 'part', 'nick')",
    "CREATE TABLE IF NOT EXISTS log_meta(name TEXT PRIMARY KEY, value INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS members(
        peer TEXT PRIMARY KEY,
        nick TEXT NOT NULL,
        joined INTEGER NOT NULL,
        first_ms INTEGER NOT NULL,
        last_ms INTEGER NOT NULL)",
    // Nicks are unique among joined members, ignoring ASCII case. The room checks this
    // before writing; the index keeps the table consistent if it did not.
    "CREATE UNIQUE INDEX IF NOT EXISTS members_joined_nick ON members(nick COLLATE NOCASE) WHERE joined = 1",
];

/// Kinds of room events, as stored in `log.kind`; the term of the `log_room` index.
const ROOM_KINDS: &str = "('msg', 'act', 'join', 'part', 'nick')";

/// Records ever inserted: the largest `seq` in the log, or the one saved in `log_meta` when
/// pruning by age emptied the log. Read from the end of the primary key.
pub const LOG_TOTAL: &str = "SELECT max(coalesce((SELECT max(seq) FROM log), 0),
    coalesce((SELECT value FROM log_meta WHERE name = 'log_seq'), 0)) AS value";
/// Appends a record with `seq` = [`LOG_TOTAL`] + 1; binds `ts_ms, peer, kind, nick, text,
/// prev`; returns `seq`.
///
/// `seq` is explicit because SQLite would number a row after an emptied table from 1 again.
pub const LOG_INSERT: &str = "INSERT INTO log(seq, ts_ms, peer, kind, nick, text, prev) VALUES (
    max(coalesce((SELECT max(seq) FROM log), 0),
        coalesce((SELECT value FROM log_meta WHERE name = 'log_seq'), 0)) + 1,
    ?1, ?2, ?3, ?4, ?5, ?6) RETURNING seq AS value";
/// The newest room event's `seq`, 0 if none; a primary-key lookup in `log_meta`.
pub const ROOM_SEQ: &str =
    "SELECT coalesce((SELECT value FROM log_meta WHERE name = 'room_seq'), 0) AS value";
/// Records `?` as the newest room event's `seq`.
pub const SET_ROOM_SEQ: &str = "INSERT INTO log_meta(name, value) VALUES ('room_seq', ?)
 ON CONFLICT(name) DO UPDATE SET value = excluded.value";
/// When the prune alarm is due, ms since the UNIX epoch; no row if none is pending.
pub const PRUNE_DUE: &str = "SELECT value FROM log_meta WHERE name = 'prune_due_ms'";
/// Records `?` as when the prune alarm is due.
pub const SET_PRUNE_DUE: &str = "INSERT INTO log_meta(name, value) VALUES ('prune_due_ms', ?)
 ON CONFLICT(name) DO UPDATE SET value = excluded.value";
/// Records that no prune alarm is pending.
pub const CLEAR_PRUNE_DUE: &str = "DELETE FROM log_meta WHERE name = 'prune_due_ms'";
/// Time of the newest record.
pub const LOG_NEWEST_TS: &str = "SELECT ts_ms FROM log ORDER BY seq DESC LIMIT 1";
/// Saves the newest `seq` in `log_meta` if the newest record is older than `?1` (ms since
/// the UNIX epoch), that is, if [`LOG_PRUNE_AGE`] is about to empty the log. Writes nothing
/// otherwise.
pub const LOG_SAVE_SEQ: &str = "INSERT INTO log_meta(name, value)
    SELECT 'log_seq', seq FROM log WHERE seq = (SELECT max(seq) FROM log) AND ts_ms < ?1
    ON CONFLICT(name) DO UPDATE SET value = max(value, excluded.value)";
/// Deletes records at or below `?1`, enforcing the row cap.
pub const LOG_PRUNE_COUNT: &str = "DELETE FROM log WHERE seq <= ?1";
/// Deletes records older than `?1` (ms since the UNIX epoch).
///
/// Records are appended in time order, so expired records are a prefix of the primary key:
/// the subquery walks it from the oldest record and stops at the first one to keep, and the
/// statement reads one row more than it deletes. A record whose timestamp went backwards
/// with the clock goes once the records before it have.
pub const LOG_PRUNE_AGE: &str = "DELETE FROM log WHERE seq < coalesce(
    (SELECT seq FROM log WHERE ts_ms >= ?1 ORDER BY seq LIMIT 1),
    (SELECT max(seq) FROM log) + 1)";
/// Retained records of one peer, counted on the `log_peer` index.
pub const LOG_YOURS: &str = "SELECT count(*) AS value FROM log WHERE peer = ?";
/// Distinct peers in retained records (`/count` only; scans the `log_peer` index).
pub const LOG_PEERS: &str = "SELECT count(DISTINCT peer) AS value FROM log";
/// Newest `?` records of any kind, oldest first.
pub const LOG_RECENT: &str = "SELECT seq, prev, ts_ms, peer, kind, nick, text FROM
   (SELECT seq, prev, ts_ms, peer, kind, nick, text FROM log ORDER BY seq DESC LIMIT ?)
 ORDER BY seq";

/// Room events after a `seq`, oldest first, at most `?`.
pub fn events_after_sql() -> String {
    format!(
        "SELECT seq, prev, ts_ms, peer, kind, nick, text FROM log
         WHERE seq > ? AND kind IN {ROOM_KINDS} ORDER BY seq LIMIT ?"
    )
}

/// Newest `?` room events, oldest first.
pub fn events_recent_sql() -> String {
    format!(
        "SELECT seq, prev, ts_ms, peer, kind, nick, text FROM
           (SELECT seq, prev, ts_ms, peer, kind, nick, text FROM log
            WHERE kind IN {ROOM_KINDS} ORDER BY seq DESC LIMIT ?)
         ORDER BY seq"
    )
}

/// One member by peer.
pub const MEMBER_GET: &str = "SELECT peer, nick, joined FROM members WHERE peer = ?";
/// Joined members, by nick.
pub const MEMBERS_JOINED: &str =
    "SELECT peer, nick, joined FROM members WHERE joined = 1 ORDER BY nick COLLATE NOCASE";
/// Inserts or updates a member; binds `peer, nick, joined, now_ms`.
pub const MEMBER_UPSERT: &str = "INSERT INTO members(peer, nick, joined, first_ms, last_ms) VALUES (?1, ?2, ?3, ?4, ?4)
 ON CONFLICT(peer) DO UPDATE SET nick = excluded.nick, joined = excluded.joined, last_ms = excluded.last_ms";

/// Milliseconds since the UNIX epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn int(sql: &dyn Sql, query: &str, params: &[Value]) -> Result<i64, SqlError> {
    Ok(sql
        .value(query, params)?
        .and_then(|v| v.as_i64())
        .unwrap_or(0))
}

fn log_row(r: Vec<Value>) -> LogRow {
    let mut it = r.into_iter();
    let mut next = || it.next().unwrap_or(Value::Null);
    LogRow {
        seq: next().as_u64(),
        prev: next().as_u64(),
        ts_ms: next().as_u64(),
        peer: next().into_string(),
        kind: next().into_string(),
        nick: next().into_string(),
        text: next().into_string(),
    }
}

fn member_row(r: Vec<Value>) -> Option<Member> {
    let mut it = r.into_iter();
    let peer = EndpointId::from_z32(it.next()?.as_str()?).ok()?;
    let nick = it.next()?.into_string();
    let joined = it.next()?.as_i64()? != 0;
    Some(Member { peer, nick, joined })
}

/// Appends a log record at `ts_ms` and enforces retention; returns its `seq` and `prev`.
///
/// A room event's `prev` is the previous room event's `seq`, and the event becomes the new
/// `room_seq`; other records have `prev` 0 and leave `room_seq` alone.
pub fn log_insert(
    sql: &dyn Sql,
    ts_ms: i64,
    peer: &str,
    kind: LogKind,
    nick: &str,
    text: &str,
) -> Result<(u64, u64), SqlError> {
    let room_event = kind.is_room_event();
    let prev = if room_event { room_seq(sql)? } else { 0 };
    let seq = int(
        sql,
        LOG_INSERT,
        &[
            ts_ms.into(),
            peer.into(),
            kind.as_str().into(),
            nick.into(),
            text.into(),
            prev.into(),
        ],
    )?;
    if room_event {
        sql.exec(SET_ROOM_SEQ, &[seq.into()])?;
    }
    log_prune(sql, ts_ms)?;
    Ok((u64::try_from(seq).unwrap_or(0), prev))
}

/// Records that the prune alarm is due [`PRUNE_DELAY`] after `ts_ms` unless one is pending;
/// returns whether the caller must schedule it. A due time already past is an alarm that
/// never ran or did not move itself on, and is replaced.
pub fn arm_prune(sql: &dyn Sql, ts_ms: i64) -> Result<bool, SqlError> {
    let pending = sql.value(PRUNE_DUE, &[])?.and_then(|v| v.as_i64());
    if pending.is_some_and(|due| due >= ts_ms) {
        return Ok(false);
    }
    sql.exec(
        SET_PRUNE_DUE,
        &[(ts_ms + PRUNE_DELAY.as_millis() as i64).into()],
    )?;
    Ok(true)
}

/// Prunes the log at `now_ms`, for the prune alarm, and records when the alarm is next due:
/// [`PRUNE_DELAY`] after the newest record left, or never if the log is empty. Returns that
/// time, which the caller schedules.
pub fn prune_due(sql: &dyn Sql, now_ms: i64) -> Result<Option<i64>, SqlError> {
    log_prune(sql, now_ms)?;
    let newest = sql.value(LOG_NEWEST_TS, &[])?.and_then(|v| v.as_i64());
    match newest {
        Some(ts) => {
            let due = ts + PRUNE_DELAY.as_millis() as i64;
            sql.exec(SET_PRUNE_DUE, &[due.into()])?;
            Ok(Some(due))
        }
        None => {
            sql.exec(CLEAR_PRUNE_DUE, &[])?;
            Ok(None)
        }
    }
}

/// The newest room event's `seq`, 0 if none.
pub fn room_seq(sql: &dyn Sql) -> Result<u64, SqlError> {
    Ok(u64::try_from(int(sql, ROOM_SEQ, &[])?).unwrap_or(0))
}

/// Deletes records beyond [`LOG_RETENTION`] or older than [`LOG_MAX_AGE`] at `now_ms`.
/// `seq` and the total stay monotonic.
pub fn log_prune(sql: &dyn Sql, now_ms: i64) -> Result<(), SqlError> {
    let newest = int(sql, LOG_TOTAL, &[])?;
    sql.exec(LOG_PRUNE_COUNT, &[(newest - LOG_RETENTION as i64).into()])?;
    let cutoff = Value::Integer(now_ms - LOG_MAX_AGE.as_millis() as i64);
    sql.exec(LOG_SAVE_SEQ, std::slice::from_ref(&cutoff))?;
    sql.exec(LOG_PRUNE_AGE, &[cutoff])?;
    Ok(())
}

/// Records ever inserted (`max(seq)`).
pub fn log_total(sql: &dyn Sql) -> Result<u64, SqlError> {
    Ok(u64::try_from(int(sql, LOG_TOTAL, &[])?).unwrap_or(0))
}

/// Retained records of `peer` (z-base-32).
pub fn log_yours(sql: &dyn Sql, peer: &str) -> Result<u64, SqlError> {
    Ok(u64::try_from(int(sql, LOG_YOURS, &[peer.into()])?).unwrap_or(0))
}

/// Distinct peers in retained records.
pub fn log_peers(sql: &dyn Sql) -> Result<u64, SqlError> {
    Ok(u64::try_from(int(sql, LOG_PEERS, &[])?).unwrap_or(0))
}

/// Newest `n` records of any kind, oldest first.
pub fn log_recent(sql: &dyn Sql, n: usize) -> Result<Vec<LogRow>, SqlError> {
    Ok(sql
        .exec(LOG_RECENT, &[n.into()])?
        .into_iter()
        .map(log_row)
        .collect())
}

/// Up to `limit` room events after `seq`, oldest first.
pub fn events_after(sql: &dyn Sql, seq: u64, limit: usize) -> Result<Vec<LogRow>, SqlError> {
    Ok(sql
        .exec(&events_after_sql(), &[seq.into(), limit.into()])?
        .into_iter()
        .map(log_row)
        .collect())
}

/// Newest `n` room events, oldest first.
pub fn events_recent(sql: &dyn Sql, n: usize) -> Result<Vec<LogRow>, SqlError> {
    Ok(sql
        .exec(&events_recent_sql(), &[n.into()])?
        .into_iter()
        .map(log_row)
        .collect())
}

/// One member, joined or not.
pub fn member(sql: &dyn Sql, peer: &EndpointId) -> Result<Option<Member>, SqlError> {
    Ok(sql
        .exec(MEMBER_GET, &[peer.to_z32().into()])?
        .into_iter()
        .next()
        .and_then(member_row))
}

/// Joined members, by nick.
pub fn members(sql: &dyn Sql) -> Result<Vec<Member>, SqlError> {
    Ok(sql
        .exec(MEMBERS_JOINED, &[])?
        .into_iter()
        .filter_map(member_row)
        .collect())
}

/// Inserts or updates a member.
pub fn set_member(sql: &dyn Sql, m: &Member, now_ms: i64) -> Result<(), SqlError> {
    sql.exec(
        MEMBER_UPSERT,
        &[
            m.peer.to_z32().into(),
            m.nick.as_str().into(),
            m.joined.into(),
            now_ms.into(),
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    fn create(db: &Connection) {
        for s in SCHEMA {
            db.execute(s, []).unwrap();
        }
    }

    fn plan(db: &Connection, sql: &str) -> String {
        let mut stmt = db.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let n = stmt.parameter_count();
        let params: Vec<i64> = vec![0; n];
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), |r| {
                r.get::<_, String>(3)
            })
            .unwrap();
        rows.map(|r| r.unwrap()).collect::<Vec<_>>().join("; ")
    }

    /// Counting one peer's records reads the `log_peer` index, not the table.
    #[test]
    fn yours_uses_peer_index() {
        let db = Connection::open_in_memory().unwrap();
        create(&db);
        let p = plan(&db, LOG_YOURS);
        assert!(p.contains("USING COVERING INDEX log_peer"), "{p}");
        assert!(!p.contains("SCAN log"), "{p}");
        let p = plan(&db, LOG_TOTAL);
        assert!(!p.contains("SCAN log"), "{p}");
    }

    /// Room history walks the `log_room` partial index, which holds room events only, from
    /// `seq` or backwards from the end; `/log` walks the primary key. Only the `LIMIT`ed result
    /// is sorted, never the table.
    #[test]
    fn history_walks_room_index() {
        let db = Connection::open_in_memory().unwrap();
        create(&db);
        let p = plan(&db, &events_after_sql());
        assert_eq!(p, "SEARCH log USING INDEX log_room (seq>?)");
        let p = plan(&db, &events_recent_sql());
        assert_eq!(p.matches("TEMP B-TREE").count(), 1, "{p}");
        assert!(
            p.starts_with("CO-ROUTINE (subquery-1); SCAN log USING INDEX log_room;"),
            "{p}"
        );
        let p = plan(&db, LOG_RECENT);
        assert_eq!(p.matches("TEMP B-TREE").count(), 1, "{p}");
        assert!(p.starts_with("CO-ROUTINE (subquery-1); SCAN log;"), "{p}");
    }

    /// An insert arms the prune alarm only when none is pending; the alarm moves itself to
    /// the newest record left, and clears itself when the log is empty.
    #[test]
    fn prune_alarm_armed_once_per_quiet_period() {
        let db = Connection::open_in_memory().unwrap();
        create(&db);
        let sql = Rusqlite(&db);
        let delay = PRUNE_DELAY.as_millis() as i64;
        let due = || sql.value(PRUNE_DUE, &[]).unwrap().and_then(|v| v.as_i64());
        assert!(arm_prune(&sql, 1_000).unwrap());
        assert_eq!(due(), Some(1_000 + delay));
        insert(&db, 1_000, "a");
        // Later inserts while it is pending leave it alone.
        assert!(!arm_prune(&sql, 100_000).unwrap());
        insert(&db, 100_000, "b");
        assert_eq!(due(), Some(1_000 + delay));
        // The alarm fires, prunes "a", and moves itself to the newest record left.
        assert_eq!(prune_due(&sql, 1_000 + delay).unwrap(), Some(100_000 + delay));
        assert_eq!(due(), Some(100_000 + delay));
        assert_eq!(seqs(&db), [2]);
        // Once everything has expired the log is empty and no alarm is pending.
        assert_eq!(prune_due(&sql, 100_000 + delay).unwrap(), None);
        assert_eq!(due(), None);
        // A due time already past is replaced.
        sql.exec(SET_PRUNE_DUE, &[10i64.into()]).unwrap();
        assert!(arm_prune(&sql, 20).unwrap());
    }

    /// [`Sql`] over `rusqlite`, for the functions above.
    struct Rusqlite<'a>(&'a Connection);

    impl Sql for Rusqlite<'_> {
        fn exec(&self, query: &str, params: &[Value]) -> Result<Vec<Vec<Value>>, SqlError> {
            use rusqlite::types::{Value as V, ValueRef};
            let err = |e: rusqlite::Error| SqlError(e.to_string());
            let mut stmt = self.0.prepare(query).map_err(err)?;
            let params: Vec<V> = params
                .iter()
                .map(|p| match p {
                    Value::Null => V::Null,
                    Value::Integer(i) => V::Integer(*i),
                    Value::Real(f) => V::Real(*f),
                    Value::Text(t) => V::Text(t.clone()),
                    Value::Blob(b) => V::Blob(b.clone()),
                })
                .collect();
            let n = stmt.column_count();
            let mut rows = stmt
                .query(rusqlite::params_from_iter(params))
                .map_err(err)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(err)? {
                let mut r = Vec::with_capacity(n);
                for i in 0..n {
                    r.push(match row.get_ref(i).map_err(err)? {
                        ValueRef::Null => Value::Null,
                        ValueRef::Integer(i) => Value::Integer(i),
                        ValueRef::Real(f) => Value::Real(f),
                        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
                        ValueRef::Blob(b) => Value::Blob(b.to_vec()),
                    });
                }
                out.push(r);
            }
            Ok(out)
        }
    }

    fn insert(db: &Connection, ts: i64, text: &str) -> i64 {
        db.query_row(
            LOG_INSERT,
            rusqlite::params![ts, "p", "msg", "n", text, 0],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn prune(db: &Connection, cap: i64, cutoff: i64) {
        let newest: i64 = db.query_row(LOG_TOTAL, [], |r| r.get(0)).unwrap();
        db.execute(LOG_PRUNE_COUNT, [newest - cap]).unwrap();
        db.execute(LOG_SAVE_SEQ, [cutoff]).unwrap();
        db.execute(LOG_PRUNE_AGE, [cutoff]).unwrap();
    }

    fn seqs(db: &Connection) -> Vec<i64> {
        db.prepare("SELECT seq FROM log ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    /// Records past the row cap or older than the age limit are deleted on insert and on the
    /// prune, and `seq` and the total never go back, even when the log is emptied.
    #[test]
    fn retention_by_count_and_age() {
        let db = Connection::open_in_memory().unwrap();
        create(&db);
        let day = 86_400_000;
        for ts in [1, 2, 3, 10 * day, 11 * day] {
            insert(&db, ts, "x");
        }
        let s = insert(&db, 15 * day, "now");
        prune(&db, 10_000, 15 * day - 14 * day);
        assert_eq!(s, 6);
        assert_eq!(seqs(&db), [4, 5, 6]);
        prune(&db, 2, 0);
        assert_eq!(seqs(&db), [5, 6]);
        prune(&db, 10_000, 40 * day);
        assert!(seqs(&db).is_empty());
        let total: i64 = db.query_row(LOG_TOTAL, [], |r| r.get(0)).unwrap();
        assert_eq!(total, 6);
        assert_eq!(insert(&db, 41 * day, "again"), 7);
        let changes = db.execute(LOG_SAVE_SEQ, [0]).unwrap();
        assert_eq!(changes, 0);
        let p = plan(&db, LOG_PRUNE_AGE);
        assert!(!p.contains("TEMP B-TREE"), "{p}");
    }

    /// Nicks are unique among joined members, ignoring case; a member that left frees its nick.
    #[test]
    fn nick_unique_among_joined() {
        let db = Connection::open_in_memory().unwrap();
        create(&db);
        db.execute(MEMBER_UPSERT, rusqlite::params!["a", "Ana", 1, 5])
            .unwrap();
        db.execute(MEMBER_UPSERT, rusqlite::params!["a", "Ana", 1, 6])
            .unwrap();
        assert!(
            db.execute(MEMBER_UPSERT, rusqlite::params!["b", "ana", 1, 7])
                .is_err()
        );
        db.execute(MEMBER_UPSERT, rusqlite::params!["b", "ana", 0, 7])
            .unwrap();
        let joined: i64 = db
            .query_row(
                &format!("SELECT count(*) FROM ({MEMBERS_JOINED})"),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(joined, 1);
    }

    /// Creating the schema on every wake is idempotent.
    #[test]
    fn schema_is_idempotent() {
        let db = Connection::open_in_memory().unwrap();
        create(&db);
        insert(&db, 1, "x");
        create(&db);
        assert_eq!(seqs(&db), [1]);
    }
}
