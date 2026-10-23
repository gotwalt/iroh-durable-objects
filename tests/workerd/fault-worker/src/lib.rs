//! A deployment for `tests/workerd/fault-test.mjs`: iroh's echo app, and apps that fail on
//! purpose so that the test can check how the host recovers. Never deployed.
//!
//! - `broken`: its schema is invalid SQL, so every mint of it fails.
//! - `panic`: its constructor panics, so a mint of it traps inside the mint's transaction.
//! - `fault`: schedules one piece of alarm work at mint. The first run fails, the second
//!   panics, the third succeeds; each run that returns is recorded in `fault_runs`. Its
//!   handler for [`TRAP_ALPN`], the ALPN of iroh's echo example, panics, so that the stock
//!   echo client can make the object trap while its endpoint is bound.

use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use iroh_do::{
    App, Ctx, Protocols,
    core::{Alarm, AlarmError, Value},
};

iroh_do::worker! { apps: [iroh_do_echo::EchoApp, Fault, Broken, PanicOnNew] }

/// ALPN whose handler panics: iroh's echo example's.
pub const TRAP_ALPN: &[u8] = iroh_do_echo::ALPN;

/// See the crate docs.
pub struct Fault;

impl App for Fault {
    const NAME: &'static str = "fault";

    fn new(ctx: &Ctx) -> Self {
        // Once per endpoint: the first construction is the mint's.
        let first = ctx
            .with_sql(|s| {
                s.exec(
                    "INSERT INTO fault_meta(name) VALUES ('scheduled')
                     ON CONFLICT(name) DO NOTHING RETURNING name",
                    &[],
                )
            })
            .and_then(Result::ok)
            .is_some_and(|rows| !rows.is_empty());
        if first {
            ctx.schedule("work", std::time::Duration::ZERO, "");
        }
        Fault
    }

    fn protocols(&self, _ctx: &Ctx, p: &mut Protocols) {
        p.accept(TRAP_ALPN, Trap);
    }

    fn schema() -> &'static [&'static str] {
        &[
            "CREATE TABLE IF NOT EXISTS fault_meta(name TEXT PRIMARY KEY)",
            "CREATE TABLE IF NOT EXISTS fault_runs(attempt INTEGER NOT NULL)",
        ]
    }

    async fn on_alarm(&self, ctx: &Ctx, alarm: Alarm<'_>) -> Result<(), AlarmError> {
        if alarm.attempt == 1 {
            panic!("fault app: alarm work traps on purpose");
        }
        let attempt = Value::Integer(i64::from(alarm.attempt));
        ctx.with_sql(|s| s.exec("INSERT INTO fault_runs(attempt) VALUES (?)", &[attempt]))
            .unwrap_or(Ok(Vec::new()))?;
        if alarm.attempt == 0 {
            return Err(AlarmError::new("fault app: alarm work fails on purpose"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Trap;

impl ProtocolHandler for Trap {
    async fn accept(&self, _conn: Connection) -> Result<(), AcceptError> {
        panic!("fault app: handler traps on purpose");
    }
}

/// See the crate docs.
pub struct Broken;

impl App for Broken {
    const NAME: &'static str = "broken";

    fn new(_ctx: &Ctx) -> Self {
        Broken
    }

    fn protocols(&self, _ctx: &Ctx, _p: &mut Protocols) {}

    fn schema() -> &'static [&'static str] {
        &["CREATE TABLE broken("]
    }
}

/// See the crate docs.
pub struct PanicOnNew;

impl App for PanicOnNew {
    const NAME: &'static str = "panic";

    fn new(_ctx: &Ctx) -> Self {
        panic!("fault app: constructor traps on purpose");
    }

    fn protocols(&self, _ctx: &Ctx, _p: &mut Protocols) {}
}
