//! The handle an app uses to reach the node and its host.

use std::{
    fmt,
    future::Future,
    sync::{Arc, Weak},
    time::Duration,
};

use iroh::{EndpointId, endpoint::Connection};
use iroh_ingress_transport::peer_endpoint_addr;

use crate::{MaybeSend, Shared, State, conn::Activity, sql::Sql};

/// Prefix of the schedule keys of app work, so that they cannot collide with the host's own.
pub const APP_KEY_PREFIX: &str = "app:";

/// The app's handle to its node: storage, scheduling, peers and dialing.
///
/// Cheap to clone. It does not keep the node alive; once the node is dropped (hibernation,
/// eviction, destroy), every method is a no-op or an error.
#[derive(Clone)]
pub struct Ctx {
    id: EndpointId,
    node: Weak<Shared>,
}

impl fmt::Debug for Ctx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ctx")
            .field("id", &self.id.fmt_short().to_string())
            .finish()
    }
}

/// Why [`Ctx::connect`] failed.
#[derive(Debug)]
pub enum ConnectError {
    /// The peer has no authenticated relay socket to this object, so there is no path to it.
    NoSocket,
    /// The node is gone.
    Gone,
    /// The endpoint failed to bind.
    Bind(String),
    /// The QUIC connection failed.
    Connect(iroh::endpoint::ConnectError),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::NoSocket => f.write_str("no socket for peer"),
            ConnectError::Gone => f.write_str("node is gone"),
            ConnectError::Bind(e) => write!(f, "endpoint bind failed: {e}"),
            ConnectError::Connect(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ConnectError {}

impl Ctx {
    pub(crate) fn new(id: EndpointId, node: Weak<Shared>) -> Self {
        Self { id, node }
    }

    fn node(&self) -> Option<Arc<Shared>> {
        self.node.upgrade()
    }

    /// Returns this endpoint's EndpointId.
    pub fn id(&self) -> EndpointId {
        self.id
    }

    /// Runs `f` on the object's database. Returns `None` if the host has no database for
    /// this object any more (it was destroyed).
    pub fn with_sql<T>(&self, f: impl FnOnce(&dyn Sql) -> T) -> Option<T> {
        let node = self.node()?;
        let mut f = Some(f);
        let mut out = None;
        node.host().with_sql(&mut |sql| {
            if let Some(f) = f.take() {
                out = Some(f(sql));
            }
        });
        out
    }

    /// Schedules [`App::on_alarm`](crate::App::on_alarm) with `key` and `payload` after
    /// `delay`, replacing any pending work with the same key. The host persists it, so it
    /// runs even if the object hibernates in between.
    pub fn schedule(&self, key: &str, delay: Duration, payload: &str) {
        if let Some(node) = self.node() {
            node.host()
                .schedule(&format!("{APP_KEY_PREFIX}{key}"), delay, payload);
        }
    }

    /// Cancels pending work scheduled with `key`.
    pub fn cancel(&self, key: &str) {
        if let Some(node) = self.node() {
            node.host().cancel(&format!("{APP_KEY_PREFIX}{key}"));
        }
    }

    /// Returns the peers that have an authenticated relay socket to this object: the only
    /// peers it can dial.
    pub fn peers(&self) -> Vec<EndpointId> {
        self.node().map(|n| n.peers()).unwrap_or_default()
    }

    /// Returns whether `peer` has an authenticated relay socket to this object.
    pub fn has_socket(&self, peer: &EndpointId) -> bool {
        self.node().is_some_and(|n| n.has_socket(peer))
    }

    /// Dials `peer` over its relay socket, binding the endpoint first if needed.
    ///
    /// The connection counts like an accepted one: the endpoint stays bound while it is
    /// open, and the node closes it when idle. Dropping every clone of it closes it.
    pub async fn connect(&self, peer: EndpointId, alpn: &[u8]) -> Result<Connection, ConnectError> {
        let node = self.node().ok_or(ConnectError::Gone)?;
        if !node.has_socket(&peer) {
            return Err(ConnectError::NoSocket);
        }
        let _busy = node.busy();
        let endpoint = node.endpoint_ready().await?;
        let conn = endpoint
            .connect(peer_endpoint_addr(peer), alpn)
            .await
            .map_err(ConnectError::Connect)?;
        node.track(&conn, "out");
        Ok(conn)
    }

    /// Spawns a task on the node. The endpoint stays bound while it runs, and the node
    /// counts it when checking that a closed endpoint left no tasks behind.
    pub fn spawn(&self, fut: impl Future<Output = ()> + MaybeSend + 'static) {
        if let Some(node) = self.node() {
            let busy = node.busy();
            node.spawn(async move {
                let _busy = busy;
                fut.await;
            });
        }
    }

    /// Returns the activity record of `conn`, through which a handler reports open streams
    /// and application activity, or `None` if the node does not track `conn`.
    ///
    /// By default the node closes a connection after its idle limit without STREAM frames.
    /// A handler that holds a [`Activity::stream`] guard for every open stream gets the
    /// longer [`Config::t_silent`](crate::Config::t_silent) while a stream is open.
    pub fn activity(&self, conn: &Connection) -> Option<Arc<Activity>> {
        self.node()?.activity(conn)
    }

    /// Returns how many times the host has constructed this object.
    pub fn wakes(&self) -> u64 {
        self.node().map_or(0, |n| n.config().wakes)
    }

    /// Returns the node's lifecycle state.
    pub fn state(&self) -> State {
        self.node().map_or(State::Cold, |n| n.state())
    }

    /// Returns the endpoint's status as JSON: the node's fields and the app's.
    pub fn status(&self) -> serde_json::Value {
        self.node().map_or(serde_json::Value::Null, |n| n.status())
    }
}
