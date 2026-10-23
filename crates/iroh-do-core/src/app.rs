//! What an application gives the node: protocol handlers, a schema and alarm work.

use std::{any::Any, collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use iroh::{
    endpoint::{Accepting, Connection},
    protocol::{AcceptError, ProtocolHandler},
};

use crate::{Ctx, SqlError};

/// An application hosted by the node.
///
/// The host builds one app value per object construction, so the value lives across
/// activations but not across hibernation or eviction. State that must survive those goes
/// through [`Ctx::with_sql`].
pub trait App: Send + Sync + 'static {
    /// Name given at mint and stored with the endpoint's identity. Unique among the apps a
    /// host is built with.
    const NAME: &'static str;

    /// Creates the app; called once per object construction.
    fn new(ctx: &Ctx) -> Self
    where
        Self: Sized;

    /// Registers the protocol handlers; called on every activation, before the endpoint
    /// binds. The endpoint accepts exactly the ALPNs registered here.
    fn protocols(&self, ctx: &Ctx, protocols: &mut Protocols);

    /// SQL statements run on every construction of a minted object, before [`App::new`].
    /// They must be idempotent (`CREATE TABLE IF NOT EXISTS`).
    fn schema() -> &'static [&'static str]
    where
        Self: Sized,
    {
        &[]
    }

    /// Runs work scheduled with [`Ctx::schedule`] once it is due, possibly on an object
    /// constructed after hibernation.
    ///
    /// The host keeps the work until the returned future completes with `Ok`. An error, or the
    /// end of the object while the future runs (eviction, a panic), leaves the work to run
    /// again later, with [`Alarm::attempt`] one higher. Work can therefore run more than once.
    /// The endpoint stays bound while the future runs.
    fn on_alarm(
        &self,
        _ctx: &Ctx,
        _alarm: Alarm<'_>,
    ) -> impl Future<Output = Result<(), AlarmError>> + Send {
        async { Ok(()) }
    }

    /// Extra fields for the endpoint's status JSON.
    fn status(&self, _ctx: &Ctx) -> serde_json::Map<String, serde_json::Value> {
        serde_json::Map::new()
    }

    /// The command shown for this endpoint on the endpoint list; `id` is z-base-32.
    fn join_hint(id: &str) -> String
    where
        Self: Sized,
    {
        id.to_string()
    }
}

/// An [`App`] behind a pointer, as the node holds it.
pub trait DynApp: Send + Sync + 'static {
    /// See [`App::NAME`].
    fn name(&self) -> &'static str;
    /// See [`App::protocols`].
    fn protocols(&self, ctx: &Ctx, protocols: &mut Protocols);
    /// See [`App::on_alarm`].
    fn on_alarm<'a>(
        &'a self,
        ctx: &'a Ctx,
        alarm: Alarm<'a>,
    ) -> BoxFuture<'a, Result<(), AlarmError>>;
    /// See [`App::status`].
    fn status(&self, ctx: &Ctx) -> serde_json::Map<String, serde_json::Value>;
    /// Returns the app for downcasting to its concrete type.
    fn as_any(&self) -> &dyn Any;
}

impl<A: App> DynApp for A {
    fn name(&self) -> &'static str {
        A::NAME
    }

    fn protocols(&self, ctx: &Ctx, protocols: &mut Protocols) {
        App::protocols(self, ctx, protocols)
    }

    fn on_alarm<'a>(
        &'a self,
        ctx: &'a Ctx,
        alarm: Alarm<'a>,
    ) -> BoxFuture<'a, Result<(), AlarmError>> {
        Box::pin(App::on_alarm(self, ctx, alarm))
    }

    fn status(&self, ctx: &Ctx) -> serde_json::Map<String, serde_json::Value> {
        App::status(self, ctx)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Work scheduled with [`Ctx::schedule`] that is due, as [`App::on_alarm`] receives it.
#[derive(Debug, Clone, Copy)]
pub struct Alarm<'a> {
    /// The key the work was scheduled with.
    pub key: &'a str,
    /// The payload the work was scheduled with.
    pub payload: &'a str,
    /// How many earlier runs of this work did not complete: 0 on the first run.
    pub attempt: u32,
}

/// Why alarm work failed. The host runs the work again later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlarmError(pub String);

impl AlarmError {
    /// Creates an error from anything printable.
    pub fn new(e: impl std::fmt::Display) -> Self {
        Self(e.to_string())
    }
}

impl std::fmt::Display for AlarmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AlarmError {}

impl From<SqlError> for AlarmError {
    fn from(e: SqlError) -> Self {
        Self(e.to_string())
    }
}

/// Delay before the next run of alarm work that has failed `attempt` + 1 times:
/// [`ALARM_RETRY_BASE`] doubled per failure, at most [`ALARM_RETRY_MAX`].
pub fn alarm_retry_delay(base: Duration, attempt: u32) -> Duration {
    base.saturating_mul(1u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX))
        .min(ALARM_RETRY_MAX)
}

/// Delay before alarm work that failed once runs again.
pub const ALARM_RETRY_BASE: Duration = Duration::from_secs(30);
/// Longest delay between runs of alarm work that keeps failing.
pub const ALARM_RETRY_MAX: Duration = Duration::from_secs(3600);

/// The functions of one [`App`] type, for hosts that choose the app at run time.
#[derive(Clone, Copy)]
pub struct AppEntry {
    /// See [`App::NAME`].
    pub name: &'static str,
    /// See [`App::schema`].
    pub schema: fn() -> &'static [&'static str],
    /// See [`App::join_hint`].
    pub join_hint: fn(&str) -> String,
    /// Creates the app.
    pub new: fn(&Ctx) -> Arc<dyn DynApp>,
}

impl AppEntry {
    /// Returns the entry of `A`. A `const fn`, so that a host can keep its apps in a
    /// `static`.
    pub const fn of<A: App>() -> Self {
        Self {
            name: A::NAME,
            schema: A::schema,
            join_hint: A::join_hint,
            new: new_app::<A>,
        }
    }

    /// Returns the entry named `name` in `apps`.
    pub fn find(apps: &[AppEntry], name: &str) -> Option<AppEntry> {
        apps.iter().find(|a| a.name == name).copied()
    }
}

fn new_app<A: App>(ctx: &Ctx) -> Arc<dyn DynApp> {
    Arc::new(A::new(ctx))
}

impl std::fmt::Debug for AppEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppEntry")
            .field("name", &self.name)
            .finish()
    }
}

/// How long a connection may go without application data before the node closes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Idle(pub(crate) Duration);

impl Idle {
    /// Closes the connection after `d` without STREAM frames in either direction.
    pub fn after(d: Duration) -> Self {
        Self(d)
    }
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A [`ProtocolHandler`] behind a pointer.
pub(crate) trait DynHandler: Send + Sync + 'static {
    fn on_accepting(&self, accepting: Accepting) -> BoxFuture<'_, Result<Connection, AcceptError>>;
    fn accept(&self, conn: Connection) -> BoxFuture<'_, Result<(), AcceptError>>;
    fn shutdown(&self) -> BoxFuture<'_, ()>;
}

impl<T: ProtocolHandler> DynHandler for T {
    fn on_accepting(&self, accepting: Accepting) -> BoxFuture<'_, Result<Connection, AcceptError>> {
        Box::pin(ProtocolHandler::on_accepting(self, accepting))
    }

    fn accept(&self, conn: Connection) -> BoxFuture<'_, Result<(), AcceptError>> {
        Box::pin(ProtocolHandler::accept(self, conn))
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(ProtocolHandler::shutdown(self))
    }
}

pub(crate) struct Registered {
    pub(crate) handler: Arc<dyn DynHandler>,
    pub(crate) idle: Option<Idle>,
}

/// The protocol handlers of one activation, by ALPN.
///
/// [`Protocols::accept`] takes the same handlers as iroh's `RouterBuilder::accept`.
#[derive(Default)]
pub struct Protocols {
    pub(crate) map: HashMap<Vec<u8>, Registered>,
}

impl Protocols {
    /// Accepts connections for `alpn` with `handler`. A later call for the same ALPN
    /// replaces the earlier one.
    pub fn accept(&mut self, alpn: impl AsRef<[u8]>, handler: impl ProtocolHandler) -> &mut Self {
        self.map.insert(
            alpn.as_ref().to_vec(),
            Registered {
                handler: Arc::new(handler),
                idle: None,
            },
        );
        self
    }

    /// Like [`Protocols::accept`], with this ALPN's idle limit instead of
    /// [`Config::t_idle`](crate::Config::t_idle). The limit also applies to connections the
    /// app dials with this ALPN.
    pub fn accept_with(
        &mut self,
        alpn: impl AsRef<[u8]>,
        handler: impl ProtocolHandler,
        idle: Idle,
    ) -> &mut Self {
        self.map.insert(
            alpn.as_ref().to_vec(),
            Registered {
                handler: Arc::new(handler),
                idle: Some(idle),
            },
        );
        self
    }

    pub(crate) fn alpns(&self) -> Vec<Vec<u8>> {
        self.map.keys().cloned().collect()
    }
}

impl std::fmt::Debug for Protocols {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(
                self.map
                    .keys()
                    .map(|a| String::from_utf8_lossy(a).into_owned()),
            )
            .finish()
    }
}
