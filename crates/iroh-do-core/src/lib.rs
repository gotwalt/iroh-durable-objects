//! A runtime-agnostic iroh node for a Durable Object: the endpoint lifecycle, and the
//! protocol handlers of an [`App`].
//!
//! A [`Node`] owns at most one iroh [`Endpoint`], whose only transport is
//! [`iroh_ingress_transport`]. The node does not speak the relay protocol, hold sockets or
//! store anything. The host does those things and drives the node with events:
//!
//! - [`Node::socket_authenticated`] when a peer's relay socket completes authentication, and
//!   for every socket restored after hibernation.
//! - [`Node::datagrams`] for each relay datagram frame addressed to the object.
//! - [`Node::socket_closed`] when a peer's relay socket closes.
//! - [`Node::alarm`] when work scheduled through [`Host::schedule`] is due.
//!
//! The node calls back into the host through [`Host`]: outgoing frames through its
//! [`FrameSink`] supertrait, scheduling, and the object's SQL database. These methods are
//! synchronous. Apart from frame output they run once per application event, never per
//! packet, so a host can back them with synchronous storage.
//!
//! `Host` is `Send + Sync` because iroh requires those bounds of a custom transport's sender.
//! A Durable Object runs on a single thread and its runtime handles are not `Send`; a wasm32
//! host meets the bounds by keeping those handles in a thread-local and storing only a key in
//! the `Host` value.
//!
//! # Apps
//!
//! The node hosts one [`App`], chosen by the host. On every activation the app registers
//! iroh [`ProtocolHandler`](iroh::protocol::ProtocolHandler)s by ALPN in [`Protocols`], and
//! the node dispatches accepted connections to them as iroh's `Router` does. The app reaches
//! the node through [`Ctx`]: SQL, scheduling, the peers it can dial, and dialing.
//!
//! # Lifecycle
//!
//! [`Node::state`] reports one of three states:
//!
//! - [`State::Cold`]: no authenticated socket and no endpoint.
//! - [`State::Warm`]: at least one authenticated socket, no endpoint, no tasks and no timers.
//!   The host may hibernate the object.
//! - [`State::Active`]: the endpoint is binding, bound or closing, or an app task spawned
//!   with [`Ctx::spawn`] is running.
//!
//! The first datagram or a [`Ctx::connect`] moves the node to Active. Binding is
//! asynchronous; frames that arrive meanwhile wait in a FIFO bounded by
//! [`Config::preactivation_queue`], and frames beyond the bound are dropped. The endpoint
//! stays bound while a connection is open or an app task spawned with [`Ctx::spawn`] runs.
//! Once neither has existed for [`Config::t_linger`], the node closes and drops the endpoint,
//! returns to Warm or Cold, and records how many of its tasks outlived the endpoint in
//! [`Stats::teardown_leftover_tasks`]. Work that arrives while the endpoint is closing
//! activates it again.
//!
//! # Idle connections
//!
//! QUIC's idle timeout never fires here: iroh 1.4.0 sends a keep-alive on every path every
//! 5 s, its public API cannot disable that, and stock peers do the same. The node therefore
//! closes connections itself, with application error code 0, once they carry no application
//! data for their idle limit ([`Config::t_idle`] unless the app set one with
//! [`Protocols::accept_with`]). See [`Activity`] for how activity is observed.
//!
//! The endpoint still advertises [`Config::max_idle_timeout`]. It bounds how long a peer waits
//! on a connection whose object was evicted before it learns the connection is gone.
//!
//! # Runtime
//!
//! The node spawns its tasks with [`n0_future::task::spawn`]. Natively that is tokio, and
//! every event method must be called from within a tokio runtime. On wasm32 it is
//! `wasm_bindgen_futures::spawn_local`, and the Durable Object's thread runs the tasks between
//! events.
#![deny(missing_docs)]

mod app;
mod conn;
mod ctx;
mod endpoint;
mod sql;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    future::Future,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use iroh::Endpoint;
pub use iroh::{EndpointId, SecretKey};
pub use iroh_ingress_transport::FrameSink;
use iroh_ingress_transport::IngressTransport;
use n0_future::time::Instant;
use tokio::sync::oneshot;
use tracing::{Instrument, Span, debug, error, info, info_span, warn};

use crate::{app::Registered, conn::Activity as ConnActivity};
pub use crate::{
    app::{App, AppEntry, DynApp, Idle, Protocols},
    conn::{Activity, CLOSE_OK, StreamGuard},
    ctx::{APP_KEY_PREFIX, ConnectError, Ctx},
    sql::{Row, Sql, SqlError, Value},
};

/// Node configuration.
pub struct Config {
    /// The endpoint's secret key, persisted by the host.
    pub secret_key: SecretKey,
    /// Times the host has constructed this object, persisted by the host.
    ///
    /// Reported in the status; an increase shows the object was evicted or hibernated in
    /// between.
    pub wakes: u64,
    /// Endpoints bound before this node was created, persisted by the host through
    /// [`Host::endpoint_built`].
    ///
    /// The status reports this plus the node's own builds, so the count survives hibernation.
    pub endpoint_builds: u64,
    /// Build identifier, such as the bundle's SHA-256; the status reports its first 12
    /// characters.
    pub bundle: String,
    /// Default time without application data after which a connection is closed.
    pub t_idle: Duration,
    /// Time without application data after which a connection is closed while a handler
    /// reports an open stream through [`Activity::stream`].
    pub t_silent: Duration,
    /// Time without connections or app tasks after which the endpoint is closed.
    pub t_linger: Duration,
    /// QUIC `max_idle_timeout` the endpoint advertises.
    pub max_idle_timeout: Duration,
    /// Datagram frames held while the endpoint binds; further frames are dropped.
    pub preactivation_queue: usize,
}

impl Config {
    /// Default [`Config::t_idle`].
    pub const DEFAULT_T_IDLE: Duration = Duration::from_secs(10);
    /// Default [`Config::t_silent`].
    pub const DEFAULT_T_SILENT: Duration = Duration::from_secs(60);
    /// Default [`Config::t_linger`]; long enough to absorb an immediate redial.
    pub const DEFAULT_T_LINGER: Duration = Duration::from_secs(2);
    /// Default [`Config::max_idle_timeout`].
    pub const DEFAULT_MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
    /// Default [`Config::preactivation_queue`].
    pub const DEFAULT_PREACTIVATION_QUEUE: usize = 128;

    /// Creates a config with default timings and no previous endpoint builds.
    pub fn new(secret_key: SecretKey, wakes: u64, bundle: impl Into<String>) -> Self {
        Self {
            secret_key,
            wakes,
            endpoint_builds: 0,
            bundle: bundle.into(),
            t_idle: Self::DEFAULT_T_IDLE,
            t_silent: Self::DEFAULT_T_SILENT,
            t_linger: Self::DEFAULT_T_LINGER,
            max_idle_timeout: Self::DEFAULT_MAX_IDLE_TIMEOUT,
            preactivation_queue: Self::DEFAULT_PREACTIVATION_QUEUE,
        }
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("id", &self.secret_key.public().fmt_short().to_string())
            .field("wakes", &self.wakes)
            .field("endpoint_builds", &self.endpoint_builds)
            .field("bundle", &self.bundle)
            .field("t_idle", &self.t_idle)
            .field("t_silent", &self.t_silent)
            .field("t_linger", &self.t_linger)
            .field("max_idle_timeout", &self.max_idle_timeout)
            .field("preactivation_queue", &self.preactivation_queue)
            .finish()
    }
}

/// Services the node needs from its host: frame output, scheduling and SQL.
pub trait Host: FrameSink {
    /// Persists work under `key`, due after `delay`, replacing any pending work with the same
    /// key, and arms an alarm. When it is due the host calls [`Node::alarm`] with `key` and
    /// `payload`, possibly on a new node created after hibernation.
    fn schedule(&self, key: &str, delay: Duration, payload: &str);

    /// Deletes pending work under `key`.
    fn cancel(&self, key: &str);

    /// Calls `f` with the object's database, or does not call it if the object has none
    /// any more.
    fn with_sql(&self, f: &mut dyn FnMut(&dyn Sql));

    /// Called each time the node binds an endpoint.
    ///
    /// A host that persists the count passes it back as [`Config::endpoint_builds`].
    fn endpoint_built(&self) {}
}

/// Lifecycle state of a node; see the [crate docs](crate#lifecycle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// No authenticated socket, no endpoint.
    Cold,
    /// At least one authenticated socket, no endpoint and no tasks; hibernatable.
    Warm,
    /// Endpoint binding, bound or closing, or an app task running.
    Active,
}

impl State {
    /// Returns the state's name.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Cold => "Cold",
            State::Warm => "Warm",
            State::Active => "Active",
        }
    }
}

/// Node counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// Endpoints bound by this node.
    pub endpoint_builds: u64,
    /// Endpoint teardowns completed.
    pub endpoint_closes: u64,
    /// Open connections, accepted and dialed.
    pub connections: usize,
    /// Node tasks currently alive. Every node timer runs inside a node task.
    pub tasks: usize,
    /// Node tasks still alive after the last teardown, or `None` before the first.
    ///
    /// Anything other than `Some(0)` would keep a Durable Object from hibernating.
    pub teardown_leftover_tasks: Option<usize>,
    /// Connections the node closed as idle or silent.
    pub idle_closes: u64,
    /// Datagram frames dropped because the pre-activation queue was full.
    pub preactivation_dropped: u64,
}

/// An iroh endpoint node driven by host events.
///
/// Clones share state.
#[derive(Clone, Debug)]
pub struct Node {
    shared: Arc<Shared>,
}

struct Datagrams {
    from: EndpointId,
    ecn: u8,
    contents: Bytes,
    segment_size: Option<usize>,
}

type Handlers = Arc<HashMap<Vec<u8>, Registered>>;

enum Phase {
    Idle,
    Activating,
    Active {
        endpoint: Endpoint,
        transport: IngressTransport,
        handlers: Handlers,
    },
    Deactivating,
}

struct Inner {
    phase: Phase,
    /// The state last logged, and since when.
    state: State,
    state_since: Instant,
    /// When the endpoint was last bound.
    active_since: Option<Instant>,
    sockets: HashSet<EndpointId>,
    queue: VecDeque<Datagrams>,
    /// Dials waiting for the endpoint to bind.
    waiters: Vec<oneshot::Sender<Result<Endpoint, String>>>,
    /// Open connections, handshakes in progress, pending dials and app tasks. The endpoint
    /// lingers only when this is 0.
    busy: usize,
    linger_epoch: u64,
    /// Idle limits by ALPN, from the last activation's [`Protocols`].
    idle_limits: HashMap<Vec<u8>, Duration>,
    stats: Stats,
}

pub(crate) struct Shared {
    config: Config,
    id: EndpointId,
    host: Arc<dyn Host>,
    ctx: Ctx,
    app: OnceLock<Arc<dyn DynApp>>,
    created: Instant,
    tasks: AtomicUsize,
    inner: Mutex<Inner>,
    activities: Mutex<HashMap<usize, Arc<ConnActivity>>>,
    /// Parent of every node task, carrying `endpoint` (z-base-32) into their events.
    span: Span,
}

impl fmt::Debug for Shared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("id", &self.id.fmt_short().to_string())
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl Node {
    /// Creates a node in the Cold state, running the app `app`.
    ///
    /// After hibernation, call [`Node::socket_authenticated`] for every restored socket.
    pub fn new(config: Config, host: Arc<dyn Host>, app: &AppEntry) -> Self {
        let id = config.secret_key.public();
        let span = info_span!("node", endpoint = %id.to_z32());
        debug!(parent: &span, wakes = config.wakes, app = app.name, "node created");
        let shared = Arc::new_cyclic(|weak| Shared {
            config,
            id,
            host,
            ctx: Ctx::new(id, weak.clone()),
            app: OnceLock::new(),
            created: Instant::now(),
            tasks: AtomicUsize::new(0),
            inner: Mutex::new(Inner {
                phase: Phase::Idle,
                state: State::Cold,
                state_since: Instant::now(),
                active_since: None,
                sockets: HashSet::new(),
                queue: VecDeque::new(),
                waiters: Vec::new(),
                busy: 0,
                linger_epoch: 0,
                idle_limits: HashMap::new(),
                stats: Stats::default(),
            }),
            activities: Mutex::new(HashMap::new()),
            span,
        });
        let built = {
            let _span = shared.span.enter();
            (app.new)(&shared.ctx)
        };
        let _ = shared.app.set(built);
        Self { shared }
    }

    /// Returns the node's EndpointId.
    pub fn id(&self) -> EndpointId {
        self.shared.id
    }

    /// Returns the app's handle to this node.
    pub fn ctx(&self) -> Ctx {
        self.shared.ctx.clone()
    }

    /// Returns the app, for downcasting with [`DynApp::as_any`].
    pub fn app(&self) -> Option<&Arc<dyn DynApp>> {
        self.shared.app.get()
    }

    /// Records that `peer` has an authenticated relay socket.
    pub fn socket_authenticated(&self, peer: EndpointId) {
        let _span = self.shared.span.enter();
        let mut inner = self.shared.lock();
        inner.sockets.insert(peer);
        self.shared.note_state_locked(&mut inner);
    }

    /// Records that `peer` no longer has a relay socket.
    ///
    /// Later dials to `peer` fail without activating the endpoint.
    pub fn socket_closed(&self, peer: EndpointId) {
        let _span = self.shared.span.enter();
        let mut inner = self.shared.lock();
        inner.sockets.remove(&peer);
        self.shared.note_state_locked(&mut inner);
    }

    /// Delivers one relay datagram frame from `from`, activating the endpoint if needed.
    ///
    /// The host has already checked that the frame is addressed to this node. `ecn` is the
    /// frame's ECN byte; `contents` holds one datagram, or several of `segment_size` bytes.
    pub fn datagrams(
        &self,
        from: EndpointId,
        ecn: u8,
        contents: Bytes,
        segment_size: Option<usize>,
    ) {
        let d = Datagrams {
            from,
            ecn,
            contents,
            segment_size,
        };
        let _span = self.shared.span.enter();
        let mut inner = self.shared.lock();
        if let Phase::Active { transport, .. } = &inner.phase {
            transport.push_datagrams(d.from, d.ecn, d.contents, d.segment_size);
            return;
        }
        if inner.queue.len() >= self.shared.config.preactivation_queue {
            inner.stats.preactivation_dropped += 1;
            debug!("pre-activation queue full, dropping frame");
            return;
        }
        inner.queue.push_back(d);
        self.shared.activate_locked(&mut inner);
    }

    /// Runs work scheduled through [`Host::schedule`] that is now due.
    ///
    /// Keys the node gave the host for the app carry [`APP_KEY_PREFIX`]; the app sees them
    /// without it. Other keys are ignored.
    pub fn alarm(&self, key: &str, payload: &str) {
        let _span = self.shared.span.enter();
        let Some(key) = key.strip_prefix(APP_KEY_PREFIX) else {
            warn!(key, "alarm for a key the node did not schedule");
            return;
        };
        if let Some(app) = self.shared.app.get() {
            app.on_alarm(&self.shared.ctx, key, payload);
        }
    }

    /// Returns the current lifecycle state.
    pub fn state(&self) -> State {
        self.shared.state()
    }

    /// Returns the current counters.
    pub fn stats(&self) -> Stats {
        self.shared.stats()
    }

    /// Returns the endpoint's status: the node's fields (`id`, `app`, `state`, `wakes`,
    /// `endpoint_builds`, `connections`, `bundle`, `since_wake_ms`) and the app's.
    pub fn status(&self) -> serde_json::Value {
        self.shared.status()
    }

    /// Returns [`Node::status`] as one line of JSON.
    pub fn status_json(&self) -> String {
        self.shared.status().to_string()
    }
}

/// `Send` on native targets (tokio's `spawn`), nothing on wasm (`spawn_local`).
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub trait MaybeSend: Send {}
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
impl<T: Send> MaybeSend for T {}
/// `Send` on native targets (tokio's `spawn`), nothing on wasm (`spawn_local`).
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub trait MaybeSend {}
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl<T> MaybeSend for T {}

/// Decrements the task counter when a node task ends (completes or is dropped).
struct TaskGuard(Arc<Shared>);

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.tasks.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Keeps the endpoint bound until dropped; see [`Inner::busy`].
pub(crate) struct Busy(Arc<Shared>);

impl Drop for Busy {
    fn drop(&mut self) {
        let mut inner = self.0.lock();
        inner.busy = inner.busy.saturating_sub(1);
        if inner.busy == 0 && matches!(inner.phase, Phase::Active { .. }) {
            self.0.start_linger_locked(&mut inner);
        }
        self.0.note_state_locked(&mut inner);
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn activities(&self) -> MutexGuard<'_, HashMap<usize, Arc<ConnActivity>>> {
        self.activities.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn state(&self) -> State {
        self.state_locked(&self.lock())
    }

    pub(crate) fn stats(&self) -> Stats {
        let mut s = self.lock().stats.clone();
        s.tasks = self.tasks.load(Ordering::SeqCst);
        s
    }

    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    pub(crate) fn host(&self) -> &Arc<dyn Host> {
        &self.host
    }

    pub(crate) fn has_socket(&self, peer: &EndpointId) -> bool {
        self.lock().sockets.contains(peer)
    }

    pub(crate) fn peers(&self) -> Vec<EndpointId> {
        self.lock().sockets.iter().copied().collect()
    }

    pub(crate) fn idle_limit(&self, alpn: &[u8]) -> Duration {
        self.lock()
            .idle_limits
            .get(alpn)
            .copied()
            .unwrap_or(self.config.t_idle)
    }

    pub(crate) fn status(&self) -> serde_json::Value {
        let stats = self.stats();
        let bundle: String = self.config.bundle.chars().take(12).collect();
        let mut out = serde_json::Map::new();
        out.insert("id".into(), self.id.to_z32().into());
        if let Some(app) = self.app.get() {
            out.insert("app".into(), app.name().into());
        }
        out.insert("state".into(), self.state().as_str().into());
        out.insert("wakes".into(), self.config.wakes.into());
        out.insert(
            "endpoint_builds".into(),
            (self.config.endpoint_builds + stats.endpoint_builds).into(),
        );
        out.insert("connections".into(), stats.connections.into());
        out.insert("bundle".into(), bundle.into());
        out.insert(
            "since_wake_ms".into(),
            (self.created.elapsed().as_millis() as u64).into(),
        );
        if let Some(app) = self.app.get() {
            out.extend(app.status(&self.ctx));
        }
        out.into()
    }

    /// Spawns a counted node task.
    pub(crate) fn spawn(self: &Arc<Self>, fut: impl Future<Output = ()> + MaybeSend + 'static) {
        self.tasks.fetch_add(1, Ordering::SeqCst);
        let guard = TaskGuard(self.clone());
        drop(n0_future::task::spawn(
            async move {
                let _guard = guard;
                fut.await;
            }
            .instrument(self.span.clone()),
        ));
    }

    /// Keeps the endpoint bound until the returned guard is dropped.
    pub(crate) fn busy(self: &Arc<Self>) -> Busy {
        let mut inner = self.lock();
        inner.busy += 1;
        inner.linger_epoch += 1;
        self.note_state_locked(&mut inner);
        Busy(self.clone())
    }

    pub(crate) fn connection_opened(&self) {
        self.lock().stats.connections += 1;
    }

    pub(crate) fn connection_closed(&self) {
        let mut inner = self.lock();
        inner.stats.connections = inner.stats.connections.saturating_sub(1);
    }

    pub(crate) fn record_idle_close(&self) {
        self.lock().stats.idle_closes += 1;
    }

    /// Returns the bound endpoint, binding it first if needed.
    pub(crate) async fn endpoint_ready(self: &Arc<Self>) -> Result<Endpoint, ConnectError> {
        let rx = {
            let mut inner = self.lock();
            if let Phase::Active { endpoint, .. } = &inner.phase {
                return Ok(endpoint.clone());
            }
            let (tx, rx) = oneshot::channel();
            inner.waiters.push(tx);
            self.activate_locked(&mut inner);
            rx
        };
        match rx.await {
            Ok(Ok(endpoint)) => Ok(endpoint),
            Ok(Err(e)) => Err(ConnectError::Bind(e)),
            Err(_) => Err(ConnectError::Gone),
        }
    }

    /// Starts binding the endpoint unless it is binding, bound or closing. A closing endpoint
    /// activates again on its own if work is queued.
    fn activate_locked(self: &Arc<Self>, inner: &mut Inner) {
        if !matches!(inner.phase, Phase::Idle) {
            return;
        }
        inner.phase = Phase::Activating;
        self.note_state_locked(inner);
        let this = self.clone();
        self.spawn(async move { this.activate().await });
    }

    async fn activate(self: Arc<Self>) {
        let mut protocols = Protocols::default();
        if let Some(app) = self.app.get() {
            app.protocols(&self.ctx, &mut protocols);
        }
        let alpns = protocols.alpns();
        let idle_limits: HashMap<_, _> = protocols
            .map
            .iter()
            .filter_map(|(alpn, r)| r.idle.map(|i| (alpn.clone(), i.0)))
            .collect();
        let handlers: Handlers = Arc::new(protocols.map);
        let transport = IngressTransport::new(self.id, self.host.clone());
        let started = Instant::now();
        let res = endpoint::bind(&self.config, transport.clone(), alpns).await;
        let ms = started.elapsed().as_millis() as u64;
        let mut inner = self.lock();
        let endpoint = match res {
            Ok(ep) => ep,
            Err(e) => {
                error!(
                    event = "endpoint_bind",
                    ok = false,
                    ms,
                    error = format!("{e:#}")
                );
                inner.phase = Phase::Idle;
                self.note_state_locked(&mut inner);
                inner.queue.clear();
                for w in inner.waiters.drain(..) {
                    let _ = w.send(Err(e.to_string()));
                }
                return;
            }
        };
        inner.stats.endpoint_builds += 1;
        inner.active_since = Some(Instant::now());
        inner.idle_limits = idle_limits;
        self.host.endpoint_built();
        info!(
            event = "endpoint_bind",
            ok = true,
            ms,
            builds = inner.stats.endpoint_builds,
            queued = inner.queue.len(),
            dials = inner.waiters.len(),
        );
        for d in inner.queue.drain(..) {
            transport.push_datagrams(d.from, d.ecn, d.contents, d.segment_size);
        }
        inner.phase = Phase::Active {
            endpoint: endpoint.clone(),
            transport,
            handlers: handlers.clone(),
        };
        let this = self.clone();
        let ep = endpoint.clone();
        self.spawn(async move { this.accept_loop(ep, handlers).await });
        for w in inner.waiters.drain(..) {
            let _ = w.send(Ok(endpoint.clone()));
        }
        if inner.busy == 0 {
            self.start_linger_locked(&mut inner);
        }
    }

    async fn accept_loop(self: Arc<Self>, endpoint: Endpoint, handlers: Handlers) {
        while let Some(incoming) = endpoint.accept().await {
            let this = self.clone();
            let handlers = handlers.clone();
            self.spawn(async move { this.handle_incoming(incoming, handlers).await });
        }
        debug!("accept loop ended");
    }

    fn start_linger_locked(self: &Arc<Self>, inner: &mut Inner) {
        inner.linger_epoch += 1;
        let epoch = inner.linger_epoch;
        let this = self.clone();
        self.spawn(async move {
            n0_future::time::sleep(this.config.t_linger).await;
            this.linger_expired(epoch).await;
        });
    }

    /// Closes and drops the endpoint unless the node became busy since `epoch`.
    async fn linger_expired(self: Arc<Self>, epoch: u64) {
        let (endpoint, handlers) = {
            let mut inner = self.lock();
            if inner.linger_epoch != epoch || inner.busy != 0 {
                return;
            }
            match std::mem::replace(&mut inner.phase, Phase::Deactivating) {
                Phase::Active {
                    endpoint,
                    transport,
                    handlers,
                } => {
                    drop(transport);
                    (endpoint, handlers)
                }
                other => {
                    inner.phase = other;
                    return;
                }
            }
        };
        debug!("idle for t_linger: closing endpoint");
        let started = Instant::now();
        for r in handlers.values() {
            r.handler.shutdown().await;
        }
        drop(handlers);
        endpoint.close().await;
        drop(endpoint);
        let leftover = self.wait_for_tasks().await;
        if leftover != 0 {
            error!(leftover, "node tasks outstanding after endpoint teardown");
        }
        let mut inner = self.lock();
        info!(
            event = "endpoint_close",
            ms = started.elapsed().as_millis() as u64,
            active_ms = inner
                .active_since
                .take()
                .map_or(0, |t| t.elapsed().as_millis() as u64),
            leftover_tasks = leftover,
        );
        inner.stats.endpoint_closes += 1;
        inner.stats.teardown_leftover_tasks = Some(leftover);
        inner.phase = Phase::Idle;
        if !inner.queue.is_empty() || !inner.waiters.is_empty() {
            debug!("work arrived while closing: re-activating");
            self.activate_locked(&mut inner);
        } else {
            self.note_state_locked(&mut inner);
        }
    }

    /// Logs a `state` event if the state changed since the last one.
    fn note_state_locked(&self, inner: &mut Inner) {
        let now = self.state_locked(inner);
        if now == inner.state {
            return;
        }
        info!(
            event = "state",
            from = inner.state.as_str(),
            to = now.as_str(),
            prev_ms = inner.state_since.elapsed().as_millis() as u64,
            sockets = inner.sockets.len(),
        );
        inner.state = now;
        inner.state_since = Instant::now();
    }

    fn state_locked(&self, inner: &Inner) -> State {
        match inner.phase {
            // Work without an endpoint (an app task) must not be mistaken for an idle object.
            Phase::Idle if inner.busy > 0 => State::Active,
            Phase::Idle if inner.sockets.is_empty() => State::Cold,
            Phase::Idle => State::Warm,
            _ => State::Active,
        }
    }

    /// Waits, for at most about one second, until the calling task is the only node task left;
    /// returns the number of others.
    ///
    /// Tasks of a closed endpoint end on a later turn of the executor (on wasm32, aborts are
    /// delivered on a later microtask), so yield before sampling.
    async fn wait_for_tasks(&self) -> usize {
        let others = || self.tasks.load(Ordering::SeqCst).saturating_sub(1);
        for _ in 0..64 {
            if others() == 0 {
                return 0;
            }
            n0_future::future::yield_now().await;
        }
        for _ in 0..100 {
            if others() == 0 {
                return 0;
            }
            n0_future::time::sleep(Duration::from_millis(10)).await;
        }
        others()
    }
}
