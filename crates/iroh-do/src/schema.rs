//! The object's SQLite schema and the statements the object runs.
//!
//! The text lives here, outside the wasm32-only glue, so that native tests can run it against
//! SQLite and check query plans. Tables:
//!
//! - `identity`: one row, written once at mint: the secret key (hex), the relay URL and the
//!   pkarr relay it was minted with.
//! - `counters`: named integers. `wakes` (constructor runs) and `endpoint_builds`
//!   (`Endpoint` instantiations), reported by `/status` on `iroh-do/ctl/1`; `log_seq`, the
//!   largest `seq` of a pruned log that was left empty; `room_seq`, the `seq` of the newest
//!   room event, which becomes the next room event's `prev`.
//! - `schedule`: pending work keyed by name (`republish`, `sweep`, `callback:<peer z32>`),
//!   multiplexed onto the object's single alarm.
//! - `pkarr_state`: one row with the outcome of the last publish and the count of
//!   consecutive failures, which drives the retry backoff.
//! - `log`: the message log (design notes 007 and 008), keeping the newest
//!   `LOG_RETENTION` rows and none older than `LOG_MAX_AGE`. Room events and the diagnostic
//!   records (`ctl`, `echo`, `callback`) share it. `seq` numbers every row ever written; see
//!   [`LOG_TOTAL`]. A room event's `prev` is the `seq` of the room event before it.
//! - `members`: the chat room's members (design note 008): nick, whether the member is
//!   joined, and when it was first and last seen.
//!
//! The schema is created as is, with no versioning or upgrade path: this is prototype code,
//! and changing the schema means resetting the object's state.

/// The tables and indexes, created idempotently on every wake.
pub const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS identity(
        k INTEGER PRIMARY KEY CHECK (k = 1),
        secret_hex TEXT NOT NULL,
        relay_url TEXT NOT NULL,
        pkarr_relay TEXT NOT NULL,
        minted_at_ms REAL NOT NULL)",
    "CREATE TABLE IF NOT EXISTS counters(name TEXT PRIMARY KEY, value INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS schedule(key TEXT PRIMARY KEY, due_ms REAL NOT NULL, payload TEXT NOT NULL DEFAULT '')",
    "CREATE TABLE IF NOT EXISTS pkarr_state(
        k INTEGER PRIMARY KEY CHECK (k = 1),
        last_ok_ms REAL,
        last_status TEXT NOT NULL,
        failures INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS log(
        seq INTEGER PRIMARY KEY,
        ts_ms INTEGER NOT NULL,
        peer TEXT NOT NULL,
        kind TEXT NOT NULL,
        nick TEXT NOT NULL DEFAULT '',
        text TEXT NOT NULL,
        prev INTEGER NOT NULL DEFAULT 0)",
    "CREATE INDEX IF NOT EXISTS log_peer ON log(peer)",
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

/// Kinds of room events, as stored in `log.kind`.
const ROOM_KINDS: &str = "('msg', 'act', 'join', 'part', 'nick')";

/// Records ever inserted: the largest `seq` in the log, or the one saved in `counters` when
/// pruning by age emptied the log. Read from the end of the primary key.
pub const LOG_TOTAL: &str = "SELECT max(coalesce((SELECT max(seq) FROM log), 0),
    coalesce((SELECT value FROM counters WHERE name = 'log_seq'), 0)) AS value";
/// Appends a record with `seq` = [`LOG_TOTAL`] + 1; binds `ts_ms, peer, kind, nick, text,
/// prev`; returns `seq`.
///
/// `seq` is explicit because SQLite would number a row after an emptied table from 1 again.
pub const LOG_INSERT: &str = "INSERT INTO log(seq, ts_ms, peer, kind, nick, text, prev) VALUES (
    max(coalesce((SELECT max(seq) FROM log), 0),
        coalesce((SELECT value FROM counters WHERE name = 'log_seq'), 0)) + 1,
    ?1, ?2, ?3, ?4, ?5, ?6) RETURNING seq AS value";
/// The newest room event's `seq`, 0 if none; a primary-key lookup in `counters`.
pub const ROOM_SEQ: &str =
    "SELECT coalesce((SELECT value FROM counters WHERE name = 'room_seq'), 0) AS value";
/// Records `?` as the newest room event's `seq`.
pub const SET_ROOM_SEQ: &str = "INSERT INTO counters(name, value) VALUES ('room_seq', ?)
 ON CONFLICT(name) DO UPDATE SET value = excluded.value";
/// Saves the newest `seq` in `counters` if the newest record is older than `?1` (ms since the
/// UNIX epoch), that is, if [`LOG_PRUNE_AGE`] is about to empty the log. Writes nothing
/// otherwise.
pub const LOG_SAVE_SEQ: &str = "INSERT INTO counters(name, value)
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
pub const LOG_RECENT: &str = "SELECT seq, ts_ms, peer, kind, nick, text FROM
   (SELECT seq, ts_ms, peer, kind, nick, text FROM log ORDER BY seq DESC LIMIT ?)
 ORDER BY seq";

/// Room events after a `seq`, oldest first, at most `?`.
pub fn events_after() -> String {
    format!(
        "SELECT seq, prev, ts_ms, peer, kind, nick, text FROM log
         WHERE seq > ? AND kind IN {ROOM_KINDS} ORDER BY seq LIMIT ?"
    )
}

/// Newest `?` room events, oldest first.
pub fn events_recent() -> String {
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

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    /// What `Store::create_schema` runs on every wake.
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
        // The expression this replaced read every row.
        let p = plan(&db, "SELECT coalesce(sum(peer = ?), 0) FROM log");
        assert!(p.contains("SCAN log"), "{p}");
    }

    /// Room history walks the primary key from `seq` or backwards from the end; only the
    /// `LIMIT`ed result is sorted, never the table.
    #[test]
    fn history_walks_primary_key() {
        let db = Connection::open_in_memory().unwrap();
        create(&db);
        let p = plan(&db, &events_after());
        assert!(p.contains("USING INTEGER PRIMARY KEY (rowid>?)"), "{p}");
        assert!(!p.contains("TEMP B-TREE"), "{p}");
        for sql in [events_recent(), LOG_RECENT.to_string()] {
            let p = plan(&db, &sql);
            assert_eq!(p.matches("TEMP B-TREE").count(), 1, "{p}");
            assert!(p.starts_with("CO-ROUTINE (subquery-1); SCAN log;"), "{p}");
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

    /// The pruning the object runs after each insert and from the daily alarm.
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
    /// alarm, and `seq` and the total never go back, even when the log is emptied.
    #[test]
    fn retention_by_count_and_age() {
        let db = Connection::open_in_memory().unwrap();
        create(&db);
        let day = 86_400_000;
        for ts in [1, 2, 3, 10 * day, 11 * day] {
            insert(&db, ts, "x");
        }
        // Insert path at day 15 with a 14-day limit: rows from before day 1 go.
        let s = insert(&db, 15 * day, "now");
        prune(&db, 10_000, 15 * day - 14 * day);
        assert_eq!(s, 6);
        assert_eq!(seqs(&db), [4, 5, 6]);
        // Row cap.
        prune(&db, 2, 0);
        assert_eq!(seqs(&db), [5, 6]);
        // Alarm path in a quiet room: everything expired.
        prune(&db, 10_000, 40 * day);
        assert!(seqs(&db).is_empty());
        let total: i64 = db.query_row(LOG_TOTAL, [], |r| r.get(0)).unwrap();
        assert_eq!(total, 6);
        assert_eq!(insert(&db, 41 * day, "again"), 7);
        // Nothing expired: no write to `counters`.
        let changes = db.execute(LOG_SAVE_SEQ, [0]).unwrap();
        assert_eq!(changes, 0);
        let p = plan(&db, LOG_PRUNE_AGE);
        assert!(!p.contains("TEMP B-TREE"), "{p}");
    }

    /// Appends a record as `Store::log_insert` does: a room event takes `room_seq` as its
    /// `prev` and becomes the new `room_seq`; other records leave it alone.
    fn append(db: &Connection, kind: &str) -> (i64, i64) {
        let room = ["msg", "act", "join", "part", "nick"].contains(&kind);
        let prev: i64 = if room {
            db.query_row(ROOM_SEQ, [], |r| r.get(0)).unwrap()
        } else {
            0
        };
        let seq: i64 = db
            .query_row(
                LOG_INSERT,
                rusqlite::params![1, "p", kind, "n", "x", prev],
                |r| r.get(0),
            )
            .unwrap();
        if room {
            db.execute(SET_ROOM_SEQ, [seq]).unwrap();
        }
        (seq, prev)
    }

    /// Room events link to the previous room event, skipping diagnostic records, and the link
    /// survives pruning of the event it points to.
    #[test]
    fn room_events_link_through_prev() {
        let db = Connection::open_in_memory().unwrap();
        create(&db);
        let got: Vec<_> = ["join", "ctl", "msg", "echo", "callback", "msg"]
            .iter()
            .map(|k| append(&db, k))
            .collect();
        assert_eq!(got, [(1, 0), (2, 0), (3, 1), (4, 0), (5, 0), (6, 3)]);
        prune(&db, 1, 0);
        assert_eq!(seqs(&db), [6]);
        assert_eq!(append(&db, "act"), (7, 6));
        // Reading `room_seq` is a primary-key lookup.
        let p = plan(&db, ROOM_SEQ);
        assert!(p.contains("SEARCH counters USING INDEX"), "{p}");
        assert!(!p.contains("SCAN counters"), "{p}");
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
        append(&db, "msg");
        create(&db);
        assert_eq!(seqs(&db), [1]);
    }
}
