//! A runtime-agnostic iroh node for a Durable Object: endpoint lifecycle, the demo chat room,
//! diagnostic protocols and dial-back.
//!
//! A [`Node`] owns at most one iroh [`Endpoint`], whose only transport is
//! [`iroh_ingress_transport`]. The node does not speak the relay protocol, hold sockets or
//! store anything. The host does those things and drives the node with events:
//!
//! - [`Node::socket_authenticated`] when a peer's relay socket completes authentication, and
//!   for every socket restored after hibernation.
//! - [`Node::datagrams`] for each relay datagram frame addressed to the object.
//! - [`Node::socket_closed`] when a peer's relay socket closes.
//! - [`Node::dial_back`] when a callback the node requested through
//!   [`Host::schedule_callback`] is due.
//!
//! The node calls back into the host through [`Host`]: outgoing frames through its
//! [`FrameSink`] supertrait, callback scheduling, the message log and the room's members.
//! These methods are synchronous. Apart from frame output they run once per application event
//! (a room request, a ctl line, an echo stream, a dial-back), never per packet, so a host can
//! back them with synchronous storage.
//!
//! `Host` is `Send + Sync` because iroh requires those bounds of a custom transport's sender.
//! A Durable Object runs on a single thread and its runtime handles are not `Send`; a wasm32
//! host meets the bounds by keeping those handles in a thread-local and storing only a key in
//! the `Host` value.
//!
//! # Lifecycle
//!
//! [`Node::state`] reports one of three states:
//!
//! - [`State::Cold`]: no authenticated socket and no endpoint.
//! - [`State::Warm`]: at least one authenticated socket, no endpoint, no tasks and no timers.
//!   The host may hibernate the object.
//! - [`State::Active`]: the endpoint is binding, bound or closing.
//!
//! The first datagram or a dial-back moves the node to Active. Binding is asynchronous;
//! frames that arrive meanwhile wait in a FIFO bounded by [`Config::preactivation_queue`],
//! and frames beyond the bound are dropped. Once no connection has existed for
//! [`Config::t_linger`], the node closes and drops the endpoint, returns to Warm or Cold, and
//! records how many of its tasks outlived the endpoint in [`Stats::teardown_leftover_tasks`].
//! Work that arrives while the endpoint is closing activates it again.
//!
//! # Idle connections
//!
//! QUIC's idle timeout never fires here: iroh 1.4.0 sends a keep-alive on every path every
//! 5 s, its public API cannot disable that, and stock peers do the same. The node therefore
//! closes connections itself, with application error code 0:
//!
//! - after [`Config::t_idle`] with no open streams and no application bytes (reason `idle`);
//! - after [`Config::t_silent`] with streams open but no application bytes (reason
//!   `silent`), so a peer cannot keep the object Active by holding a stream open.
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
//!
//! # Protocols
//!
//! The endpoint accepts [`ROOM_ALPN`], [`CTL_ALPN`] and [`ECHO_ALPN`], and dials peers with
//! [`ROOM_ALPN`] and [`NOTIFY_ALPN`]. `docs/design-notes/008-chat-room.md` defines the room;
//! `006-crates-and-demo-protocols.md` and `007-durable-message-log.md` define the others, which
//! exist to exercise the lifecycle.
//!
//! # The room
//!
//! Members do not hold connections, so that the object can hibernate between messages. A
//! member opens a connection to send a request, and the node opens one to deliver events,
//! each reusing an open connection to the other side if there is one. Both kinds close after
//! [`Config::t_idle`] like any other connection, and the endpoint is dropped
//! [`Config::t_linger`] later.
//!
//! A member is present while it has an authenticated relay socket to the object: that socket
//! is the only path the node can dial. The node delivers each new event to every joined,
//! present member except its author, batching events produced within [`DELIVERY_BATCH`] on
//! one unidirectional stream. A failed delivery is not retried; members catch up with
//! `/since`.
#![deny(missing_docs)]

mod endpoint;
mod protocols;
mod room;

use std::{
    collections::{HashSet, VecDeque},
    fmt,
    future::Future,
    sync::{
        Arc, Mutex, MutexGuard,
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
use tracing::{Instrument, Span, debug, error, info, info_span, warn};

/// ALPN of the echo protocol: each bi stream is echoed back.
pub const ECHO_ALPN: &[u8] = b"iroh-do/echo/1";
/// ALPN of the chat room: requests on member-opened bi streams, deliveries on node-opened
/// uni streams.
pub const ROOM_ALPN: &[u8] = b"iroh-do/room/1";
/// ALPN of the diagnostic protocol: one reply line per request line, plus `/status`,
/// `/count`, `/log` and `/callback`.
pub const CTL_ALPN: &[u8] = b"iroh-do/ctl/1";
/// ALPN the node dials a peer with for a dial-back.
pub const NOTIFY_ALPN: &[u8] = b"iroh-do/notify/1";
/// Largest delay, in seconds, a peer may request with `/callback`.
///
/// A dial-back needs the peer's relay socket, and iroh's relay client closes a socket to a
/// relay other than its home relay 60 s after its last send.
pub const MAX_CALLBACK_SECS: u64 = 50;
/// How long the node collects new room events for one member before opening a delivery
/// stream.
pub const DELIVERY_BATCH: Duration = Duration::from_millis(50);

/// Node configuration.
pub struct Config {
    /// The endpoint's secret key, persisted by the host.
    pub secret_key: SecretKey,
    /// Times the host has constructed this object, persisted by the host.
    ///
    /// Reported by `/status` and in the dial-back line; an increase shows the object was
    /// evicted or hibernated in between.
    pub wakes: u64,
    /// Endpoints bound before this node was created, persisted by the host through
    /// [`Host::endpoint_built`].
    ///
    /// `/status` reports this plus the node's own builds, so the count survives hibernation.
    pub endpoint_builds: u64,
    /// Build identifier, such as the bundle's SHA-256; `/status` reports its first 12
    /// characters.
    pub bundle: String,
    /// Time without open streams or application bytes after which a connection is closed.
    pub t_idle: Duration,
    /// Time without application bytes after which a connection is closed even while it has
    /// open streams.
    pub t_silent: Duration,
    /// Time without connections after which the endpoint is closed.
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

/// Services the node needs from its host: frame output, dial-back scheduling, the message log
/// and the room's members.
pub trait Host: FrameSink {
    /// Schedules a dial-back to `peer` after `delay`.
    ///
    /// The host persists the request, arms an alarm, and when it fires calls
    /// [`Node::dial_back`], possibly on a new node created after hibernation.
    fn schedule_callback(&self, peer: EndpointId, delay: Duration);

    /// Called each time the node binds an endpoint.
    ///
    /// A host that persists the count passes it back as [`Config::endpoint_builds`].
    fn endpoint_built(&self) {}

    /// Appends a record with an empty nick to the message log and returns its sequence number
    /// and the running totals.
    ///
    /// The node has already truncated `text` to [`MAX_LOG_TEXT`] bytes.
    fn log_event(&self, peer: EndpointId, kind: LogKind, text: &str) -> LogStats;

    /// Appends a room event to the message log and returns the stored record, or `None` if it
    /// could not be stored.
    ///
    /// The record's `prev` is [`Host::room_seq`] before the append.
    ///
    /// For [`LogKind::Nick`], `nick` is the new nick and `text` the old one; for
    /// [`LogKind::Part`], `text` is the reason.
    fn append(&self, peer: EndpointId, kind: LogKind, nick: &str, text: &str) -> Option<LogRow>;

    /// Returns the `seq` of the newest room event ever stored, 0 if none.
    ///
    /// Unaffected by retention: it is the `prev` of the next room event.
    fn room_seq(&self) -> u64;

    /// Returns up to `limit` room events with a sequence number above `seq`, oldest first.
    fn events_after(&self, seq: u64, limit: usize) -> Vec<LogRow>;

    /// Returns the newest `n` room events, oldest first.
    fn events_recent(&self, n: usize) -> Vec<LogRow>;

    /// Returns the stored member record of `peer`, joined or not.
    fn member(&self, peer: EndpointId) -> Option<Member>;

    /// Returns the joined members.
    fn members(&self) -> Vec<Member>;

    /// Inserts or replaces the member record of `member.peer`.
    fn set_member(&self, member: &Member);

    /// Returns the message log's totals, counting `peer`'s own records.
    fn count(&self, peer: EndpointId) -> Counts;

    /// Returns the number of records ever inserted, as in [`Counts::total`].
    ///
    /// Callers that need only the total (`/status`, the dial-back line) use this to spare the
    /// host the per-peer counts.
    fn total(&self) -> u64;

    /// Returns the newest `n` records, oldest first.
    fn recent(&self, n: usize) -> Vec<LogRow>;
}

/// Kind of a message-log record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    /// A room message (`/say`).
    Msg,
    /// A room action (`/me`).
    Act,
    /// A member joined the room.
    Join,
    /// A member left the room (`/quit`).
    Part,
    /// A member changed its nick.
    Nick,
    /// A request line on [`CTL_ALPN`].
    Ctl,
    /// An echo stream.
    Echo,
    /// A dial-back attempt.
    Callback,
}

impl LogKind {
    /// Returns the name stored in a record's `kind` field.
    pub fn as_str(self) -> &'static str {
        match self {
            LogKind::Msg => "msg",
            LogKind::Act => "act",
            LogKind::Join => "join",
            LogKind::Part => "part",
            LogKind::Nick => "nick",
            LogKind::Ctl => "ctl",
            LogKind::Echo => "echo",
            LogKind::Callback => "callback",
        }
    }

    /// Returns whether records of this kind are room events.
    pub fn is_room_event(self) -> bool {
        matches!(
            self,
            LogKind::Msg | LogKind::Act | LogKind::Join | LogKind::Part | LogKind::Nick
        )
    }
}

/// Kinds of room events, as stored in a record's `kind` field.
pub const ROOM_EVENT_KINDS: [&str; 5] = ["msg", "act", "join", "part", "nick"];

/// A member of the room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The member's EndpointId.
    pub peer: EndpointId,
    /// Its nick: 1–24 characters of `[A-Za-z0-9_.-]`, unique among joined members ignoring
    /// ASCII case.
    pub nick: String,
    /// Whether it is joined. A member that sent `/quit` keeps its record, not joined.
    pub joined: bool,
}

/// Result of [`Host::log_event`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LogStats {
    /// Sequence number of the inserted record.
    pub seq: u64,
    /// Records ever inserted; unaffected by retention.
    pub total: u64,
    /// Retained records from the same peer.
    pub yours: u64,
}

/// Result of [`Host::count`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Records ever inserted.
    pub total: u64,
    /// Retained records from the asking peer.
    pub yours: u64,
    /// Distinct peers in retained records.
    pub peers: u64,
}

/// One message-log record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRow {
    /// Sequence number.
    pub seq: u64,
    /// For a room event, the `seq` of the room event before it (0 if none); 0 for other
    /// records.
    ///
    /// Room events share `seq` with the diagnostic records, so consecutive room events need
    /// not have consecutive numbers. `prev` lets a member that last saw room event `L` tell a
    /// missed event (`prev > L`) from a diagnostic record in between.
    pub prev: u64,
    /// Insert time in milliseconds since the UNIX epoch.
    pub ts_ms: u64,
    /// The peer's EndpointId, z-base-32.
    pub peer: String,
    /// One of [`LogKind::as_str`].
    pub kind: String,
    /// The author's nick for room events (the new nick for `nick`), empty otherwise.
    pub nick: String,
    /// The message for `msg` and `act`, the reason for `part`, the old nick for `nick`, the
    /// request line for `ctl`, `"<n> bytes"` for `echo`, and `"delivered"` or
    /// `"failed: <reason>"` for `callback`.
    pub text: String,
}

/// Longest record text stored in the log, in bytes.
pub const MAX_LOG_TEXT: usize = 1024;
/// Records a host retains; older ones are deleted on insert.
pub const LOG_RETENTION: u64 = 10_000;
/// Age after which a host deletes a record, on insert and at least daily.
pub const LOG_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 3600);

/// Lifecycle state of a node; see the [crate docs](crate#lifecycle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// No authenticated socket, no endpoint.
    Cold,
    /// At least one authenticated socket, no endpoint and no tasks; hibernatable.
    Warm,
    /// Endpoint binding, bound or closing.
    Active,
}

impl State {
    fn as_str(self) -> &'static str {
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
    /// Open connections, including handshakes in progress and dial-backs.
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
    /// Dial-backs whose notify stream was delivered.
    pub dial_backs_ok: u64,
    /// Dial-backs that failed: no socket, bind, connect or stream error.
    pub dial_backs_failed: u64,
    /// Room delivery streams written to members.
    pub deliveries_ok: u64,
    /// Room delivery streams that failed: no socket, connect or stream error.
    pub deliveries_failed: u64,
    /// Connections the node dialed to deliver room events.
    pub delivery_dials: u64,
    /// Delivery streams opened on a connection that was already open.
    pub delivery_reuses: u64,
    /// Room requests refused by the rate limit.
    pub rate_limited: u64,
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

enum Phase {
    Idle,
    Activating,
    Active {
        endpoint: Endpoint,
        transport: IngressTransport,
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
    pending_dials: Vec<EndpointId>,
    linger_epoch: u64,
    stats: Stats,
}

pub(crate) struct Shared {
    config: Config,
    id: EndpointId,
    host: Arc<dyn Host>,
    created: Instant,
    tasks: AtomicUsize,
    inner: Mutex<Inner>,
    room: Mutex<room::Room>,
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
    /// Creates a node in the Cold state.
    ///
    /// After hibernation, call [`Node::socket_authenticated`] for every restored socket.
    pub fn new(config: Config, host: Arc<dyn Host>) -> Self {
        let id = config.secret_key.public();
        let span = info_span!("node", endpoint = %id.to_z32());
        debug!(parent: &span, wakes = config.wakes, "node created");
        Self {
            shared: Arc::new(Shared {
                config,
                id,
                host,
                created: Instant::now(),
                tasks: AtomicUsize::new(0),
                inner: Mutex::new(Inner {
                    phase: Phase::Idle,
                    state: State::Cold,
                    state_since: Instant::now(),
                    active_since: None,
                    sockets: HashSet::new(),
                    queue: VecDeque::new(),
                    pending_dials: Vec::new(),
                    linger_epoch: 0,
                    stats: Stats::default(),
                }),
                room: Mutex::new(room::Room::default()),
                span,
            }),
        }
    }

    /// Returns the node's EndpointId.
    pub fn id(&self) -> EndpointId {
        self.shared.id
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
    /// Later dial-backs to `peer` fail without activating the endpoint.
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

    /// Dials `peer` with [`NOTIFY_ALPN`] over its relay socket, activating the endpoint if
    /// needed.
    ///
    /// If `peer` has no authenticated socket the dial-back fails at once. Every attempt is
    /// logged through [`Host::log_event`] and counted in [`Stats`].
    pub fn dial_back(&self, peer: EndpointId) {
        let _span = self.shared.span.enter();
        let mut inner = self.shared.lock();
        if !inner.sockets.contains(&peer) {
            warn!(
                event = "dial_back",
                peer = %peer.to_z32(),
                ok = false,
                error = "no socket for peer",
            );
            inner.stats.dial_backs_failed += 1;
            drop(inner);
            protocols::log_callback(&self.shared, peer, "failed: no socket for peer");
            return;
        }
        if let Phase::Active { endpoint, .. } = &inner.phase {
            let endpoint = endpoint.clone();
            self.shared.start_dial_locked(&mut inner, endpoint, peer);
        } else {
            inner.pending_dials.push(peer);
            self.shared.activate_locked(&mut inner);
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

    /// Returns the reply to `/status` on [`CTL_ALPN`]: one line of JSON with the state,
    /// counters, records, joined members and present members.
    pub fn status_json(&self) -> String {
        protocols::status_json(&self.shared)
    }
}

/// `Send` on native targets (tokio's `spawn`), nothing on wasm (`spawn_local`).
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(crate) trait MaybeSend: Send {}
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
impl<T: Send> MaybeSend for T {}
/// `Send` on native targets (tokio's `spawn`), nothing on wasm (`spawn_local`).
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub(crate) trait MaybeSend {}
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl<T> MaybeSend for T {}

/// Decrements the task counter when a node task ends (completes or is dropped).
struct TaskGuard(Arc<Shared>);

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.tasks.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
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

    pub(crate) fn id(&self) -> EndpointId {
        self.id
    }

    pub(crate) fn host(&self) -> &Arc<dyn Host> {
        &self.host
    }

    pub(crate) fn since_wake(&self) -> Duration {
        self.created.elapsed()
    }

    pub(crate) fn room(&self) -> MutexGuard<'_, room::Room> {
        self.room.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Returns whether `peer` has an authenticated relay socket.
    pub(crate) fn has_socket(&self, peer: &EndpointId) -> bool {
        self.lock().sockets.contains(peer)
    }

    /// Returns the endpoint if it is bound.
    pub(crate) fn endpoint(&self) -> Option<Endpoint> {
        match &self.lock().phase {
            Phase::Active { endpoint, .. } => Some(endpoint.clone()),
            _ => None,
        }
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
        let transport = IngressTransport::new(self.id, self.host.clone());
        let started = Instant::now();
        let res = endpoint::bind(&self.config, transport.clone()).await;
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
                let failed: Vec<_> = inner.pending_dials.drain(..).collect();
                inner.stats.dial_backs_failed += failed.len() as u64;
                drop(inner);
                let outcome = format!("failed: endpoint bind failed: {e}");
                for peer in failed {
                    protocols::log_callback(&self, peer, &outcome);
                }
                return;
            }
        };
        inner.stats.endpoint_builds += 1;
        inner.active_since = Some(Instant::now());
        self.host.endpoint_built();
        info!(
            event = "endpoint_bind",
            ok = true,
            ms,
            builds = inner.stats.endpoint_builds,
            queued = inner.queue.len(),
            dials = inner.pending_dials.len(),
        );
        for d in inner.queue.drain(..) {
            transport.push_datagrams(d.from, d.ecn, d.contents, d.segment_size);
        }
        inner.phase = Phase::Active {
            endpoint: endpoint.clone(),
            transport,
        };
        let this = self.clone();
        let ep = endpoint.clone();
        self.spawn(async move { this.accept_loop(ep).await });
        let dials: Vec<_> = inner.pending_dials.drain(..).collect();
        for peer in dials {
            self.start_dial_locked(&mut inner, endpoint.clone(), peer);
        }
        if inner.stats.connections == 0 {
            self.start_linger_locked(&mut inner);
        }
    }

    async fn accept_loop(self: Arc<Self>, endpoint: Endpoint) {
        while let Some(incoming) = endpoint.accept().await {
            self.connection_started();
            let this = self.clone();
            self.spawn(async move {
                protocols::handle_incoming(this.clone(), incoming).await;
                this.connection_ended();
            });
        }
        debug!("accept loop ended");
    }

    fn start_dial_locked(
        self: &Arc<Self>,
        inner: &mut Inner,
        endpoint: Endpoint,
        peer: EndpointId,
    ) {
        inner.stats.connections += 1;
        inner.linger_epoch += 1;
        let this = self.clone();
        self.spawn(async move {
            let ok = protocols::dial_notify(&this, &endpoint, peer).await;
            {
                let mut inner = this.lock();
                if ok {
                    inner.stats.dial_backs_ok += 1;
                } else {
                    inner.stats.dial_backs_failed += 1;
                }
            }
            this.connection_ended();
        });
    }

    pub(crate) fn connection_started(&self) {
        let mut inner = self.lock();
        inner.stats.connections += 1;
        inner.linger_epoch += 1;
    }

    pub(crate) fn connection_ended(self: &Arc<Self>) {
        let mut inner = self.lock();
        inner.stats.connections = inner.stats.connections.saturating_sub(1);
        if inner.stats.connections == 0 && matches!(inner.phase, Phase::Active { .. }) {
            self.start_linger_locked(&mut inner);
        }
    }

    pub(crate) fn record_idle_close(&self) {
        self.lock().stats.idle_closes += 1;
    }

    pub(crate) fn update_stats(&self, f: impl FnOnce(&mut Stats)) {
        f(&mut self.lock().stats);
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

    /// Closes and drops the endpoint unless a connection started since `epoch`.
    async fn linger_expired(self: Arc<Self>, epoch: u64) {
        let endpoint = {
            let mut inner = self.lock();
            if inner.linger_epoch != epoch || inner.stats.connections != 0 {
                return;
            }
            match std::mem::replace(&mut inner.phase, Phase::Deactivating) {
                Phase::Active {
                    endpoint,
                    transport,
                } => {
                    drop(transport);
                    endpoint
                }
                other => {
                    inner.phase = other;
                    return;
                }
            }
        };
        debug!("no connections for t_linger: closing endpoint");
        self.room().clear_connections();
        let started = Instant::now();
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
        if !inner.queue.is_empty() || !inner.pending_dials.is_empty() {
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
