//! The tables the library keeps in every object's SQLite database.
//!
//! The text lives here, outside the wasm32-only glue, so that native tests can run it against
//! SQLite. Tables:
//!
//! - `identity`: one row, written once at mint: the app the endpoint runs, the secret key
//!   (hex), the relay URL and the pkarr relay it was minted with.
//! - `counters`: named integers. `wakes` (constructor runs) and `endpoint_builds`
//!   (`Endpoint` instantiations), reported in the endpoint's status; `registered`, set once
//!   the registry has the endpoint; `bound_at_ms`, present while an `Endpoint` is bound, so
//!   that the next constructor can tell that the previous instance ended with connections
//!   open.
//! - `schedule`: pending work keyed by name (`republish`, `sweep`, and the app's keys under
//!   `iroh_do_core::APP_KEY_PREFIX`), multiplexed onto the object's single alarm. `attempt`
//!   counts the runs of the work that did not complete; a row is deleted only after its work
//!   succeeds.
//! - `pkarr_state`: one row with the outcome of the last publish and the count of
//!   consecutive failures, which drives the retry backoff.
//!
//! The app's own tables are created from `App::schema` after these. The schema is created as
//! is, with no versioning or upgrade path: this is prototype code, and changing the schema
//! means resetting the object's state.

/// The tables, created idempotently on every wake.
pub const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS identity(
        k INTEGER PRIMARY KEY CHECK (k = 1),
        app TEXT NOT NULL,
        secret_hex TEXT NOT NULL,
        relay_url TEXT NOT NULL,
        pkarr_relay TEXT NOT NULL,
        minted_at_ms REAL NOT NULL)",
    "CREATE TABLE IF NOT EXISTS counters(name TEXT PRIMARY KEY, value INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS schedule(
        key TEXT PRIMARY KEY,
        due_ms REAL NOT NULL,
        payload TEXT NOT NULL DEFAULT '',
        attempt INTEGER NOT NULL DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS pkarr_state(
        k INTEGER PRIMARY KEY CHECK (k = 1),
        last_ok_ms REAL,
        last_status TEXT NOT NULL,
        failures INTEGER NOT NULL)",
];

/// Adds or replaces work under `?1`, due at `?2` with payload `?3`. Work scheduled again
/// starts over: its `attempt` is 0.
pub const SCHEDULE_UPSERT: &str = "INSERT INTO schedule(key, due_ms, payload) VALUES (?1, ?2, ?3)
     ON CONFLICT(key) DO UPDATE SET due_ms = excluded.due_ms, payload = excluded.payload, attempt = 0";
/// Adds work under `?1` due at `?2`, or moves existing work there if that is earlier.
pub const SCHEDULE_EARLIEST: &str = "INSERT INTO schedule(key, due_ms, payload) VALUES (?1, ?2, '')
     ON CONFLICT(key) DO UPDATE SET due_ms = min(due_ms, excluded.due_ms)";
/// Work due at or before `?1`, earliest first.
pub const SCHEDULE_DUE: &str =
    "SELECT key, due_ms, payload, attempt FROM schedule WHERE due_ms <= ?1 ORDER BY due_ms";
/// Leases the work `?2` read as due at `?3` before it runs: moves it to `?1`, when it runs
/// again if this run does not complete, and counts the attempt. Returns the leased row, or
/// nothing if the work changed since it was read.
pub const SCHEDULE_LEASE: &str = "UPDATE schedule SET due_ms = ?1, attempt = attempt + 1
     WHERE key = ?2 AND due_ms = ?3 RETURNING key, due_ms, payload, attempt";
/// Deletes the work `?1` leased for `?2` after it succeeded, unless it was scheduled again
/// while it ran.
pub const SCHEDULE_CLAIM: &str = "DELETE FROM schedule WHERE key = ?1 AND due_ms = ?2";

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    /// Creating the schema on every wake is idempotent.
    #[test]
    fn schema_is_idempotent() {
        let db = Connection::open_in_memory().unwrap();
        for _ in 0..2 {
            for s in SCHEMA {
                db.execute(s, []).unwrap();
            }
        }
        db.execute("INSERT INTO counters(name, value) VALUES ('wakes', 1)", [])
            .unwrap();
        for s in SCHEMA {
            db.execute(s, []).unwrap();
        }
        let n: i64 = db
            .query_row("SELECT value FROM counters WHERE name = 'wakes'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 1);
    }

    fn create() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        for s in SCHEMA {
            db.execute(s, []).unwrap();
        }
        db
    }

    fn row(db: &Connection, key: &str) -> Option<(f64, i64)> {
        db.query_row(
            "SELECT due_ms, attempt FROM schedule WHERE key = ?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
    }

    fn lease(db: &Connection, key: &str, read_due: f64, retry: f64) -> Option<(f64, i64)> {
        db.query_row(
            SCHEDULE_LEASE,
            rusqlite::params![retry, key, read_due],
            |r| Ok((r.get(1)?, r.get(3)?)),
        )
        .ok()
    }

    /// Work stays scheduled until a run succeeds: a lease moves it to the retry time and
    /// counts the attempt, and only the claim of an unchanged lease deletes it.
    #[test]
    fn leased_work_survives_until_claimed() {
        let db = create();
        db.execute(SCHEDULE_UPSERT, rusqlite::params!["app:x", 100.0, "p"])
            .unwrap();
        // First run fails: the lease is left in place, due at the retry time.
        assert_eq!(lease(&db, "app:x", 100.0, 130.0), Some((130.0, 1)));
        assert_eq!(row(&db, "app:x"), Some((130.0, 1)));
        // A stale read leases nothing.
        assert_eq!(lease(&db, "app:x", 100.0, 999.0), None);
        // Second run succeeds: the claim deletes it.
        assert_eq!(lease(&db, "app:x", 130.0, 190.0), Some((190.0, 2)));
        db.execute(SCHEDULE_CLAIM, rusqlite::params!["app:x", 190.0])
            .unwrap();
        assert_eq!(row(&db, "app:x"), None);
    }

    /// Work scheduled again while it runs is kept by the claim, and starts over at attempt 0.
    #[test]
    fn rescheduled_work_is_not_claimed() {
        let db = create();
        db.execute(SCHEDULE_UPSERT, rusqlite::params!["republish", 100.0, ""])
            .unwrap();
        assert_eq!(lease(&db, "republish", 100.0, 130.0), Some((130.0, 1)));
        db.execute(SCHEDULE_UPSERT, rusqlite::params!["republish", 5000.0, ""])
            .unwrap();
        db.execute(SCHEDULE_CLAIM, rusqlite::params!["republish", 130.0])
            .unwrap();
        assert_eq!(row(&db, "republish"), Some((5000.0, 0)));
        // `SCHEDULE_EARLIEST` only moves work earlier and keeps its attempt count.
        db.execute(SCHEDULE_EARLIEST, rusqlite::params!["republish", 6000.0])
            .unwrap();
        assert_eq!(row(&db, "republish"), Some((5000.0, 0)));
        db.execute(SCHEDULE_EARLIEST, rusqlite::params!["republish", 50.0])
            .unwrap();
        assert_eq!(row(&db, "republish"), Some((50.0, 0)));
    }
}
