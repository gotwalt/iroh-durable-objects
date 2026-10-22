//! The tables the library keeps in every object's SQLite database.
//!
//! The text lives here, outside the wasm32-only glue, so that native tests can run it against
//! SQLite. Tables:
//!
//! - `identity`: one row, written once at mint: the secret key (hex), the relay URL and the
//!   pkarr relay it was minted with.
//! - `counters`: named integers. `wakes` (constructor runs) and `endpoint_builds`
//!   (`Endpoint` instantiations), reported in the endpoint's status; `registered`, set once
//!   the registry has the endpoint.
//! - `schedule`: pending work keyed by name (`republish`, `sweep`, and the app's keys under
//!   `iroh_do_core::APP_KEY_PREFIX`), multiplexed onto the object's single alarm.
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
];

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
}
