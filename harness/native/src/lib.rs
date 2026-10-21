//! Native host for the crates the Durable Object hosts, for testing against stock iroh peers.
//!
//! The harness hosts `iroh-relay-ingress` and `iroh-do-core` the same way the Durable Object
//! does, but under tokio, so their behaviour is tested natively against unmodified iroh 1.4.0
//! peers over loopback. It is test scaffolding and is not deployed.
//!
//! [`Harness`] is a WebSocket server. It routes each upgrade by the first label of its `Host`
//! header (`<z32>.localhost`) to the [`DoObject`] minted for that EndpointId, as the router
//! Worker routes a request to a Durable Object.
//!
//! It also answers `GET /ping` and `GET /generate_204` as the router Worker does, because a
//! stock client probes its home relay with them, and the room's members use the room as their
//! home relay.
//!
//! A [`DoObject`] divides its state as a Durable Object does. What the object keeps in SQLite
//! (identity, `wakes`, endpoint builds, scheduled callbacks, message log, members) and what the runtime
//! keeps across hibernation (sockets, with session snapshots as their attachments) survive
//! [`DoObject::hibernate`] and [`DoObject::evict`]. The in-memory part, the [`Node`] and the
//! live `IngressSession`s, does not. The next event rebuilds it as a constructor would:
//! `wakes` is incremented, sessions are restored from attachments, and their peers are
//! announced to the node again.

use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    num::NonZeroU16,
    sync::{
        Arc, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
pub use iroh_do_core::{
    Config, Counts, EndpointId, FrameSink, Host, LogKind, LogRow, LogStats, Member, Node,
    ROOM_EVENT_KINDS, SecretKey, State, Stats,
};
use iroh_relay_ingress::{
    DatagramBatch, Ecn, IngressConfig, IngressSession, Output, SessionSnapshot, Status,
    UpgradeRequest, encode_datagrams,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::JoinHandle,
    time::Instant,
};
use tokio_tungstenite::tungstenite::{
    Message,
    handshake::server::{ErrorResponse, Request, Response},
    http::{HeaderValue, StatusCode},
    protocol::{CloseFrame, frame::coding::CloseCode},
};
use tracing::{debug, info};

/// Node timings an object is minted with.
#[derive(Debug, Clone, Copy)]
pub struct Timings {
    /// See [`Config::t_idle`].
    pub t_idle: Duration,
    /// See [`Config::t_silent`].
    pub t_silent: Duration,
    /// See [`Config::t_linger`].
    pub t_linger: Duration,
    /// See [`Config::max_idle_timeout`].
    pub max_idle_timeout: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            t_idle: Config::DEFAULT_T_IDLE,
            t_silent: Config::DEFAULT_T_SILENT,
            t_linger: Config::DEFAULT_T_LINGER,
            max_idle_timeout: Config::DEFAULT_MAX_IDLE_TIMEOUT,
        }
    }
}

/// The bundle id the harness reports in `/status`.
pub const BUNDLE: &str = "native-harness";

type Objects = Arc<Mutex<HashMap<EndpointId, Arc<DoObject>>>>;

/// A WebSocket server hosting minted objects.
#[derive(Debug)]
pub struct Harness {
    addr: SocketAddr,
    objects: Objects,
    accept: JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl Harness {
    /// Binds `127.0.0.1:0` and starts accepting connections.
    pub async fn start() -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let objects: Objects = Default::default();
        let objs = objects.clone();
        let accept = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let objs = objs.clone();
                tokio::spawn(async move { serve_socket(stream, objs).await });
            }
        });
        Ok(Self {
            addr,
            objects,
            accept,
        })
    }

    /// Returns the listening port.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Creates an object with a fresh identity.
    pub fn mint(&self, timings: Timings) -> Arc<DoObject> {
        let secret = SecretKey::generate();
        let id = secret.public();
        let obj = Arc::new_cyclic(|me| DoObject {
            me: me.clone(),
            id,
            secret,
            timings,
            wakes: AtomicU64::new(0),
            endpoint_builds: AtomicU64::new(0),
            callbacks: Mutex::new(Vec::new()),
            log: Mutex::new(MessageLog::default()),
            members: Mutex::new(HashMap::new()),
            sockets: Mutex::new(HashMap::new()),
            next_sid: AtomicU64::new(0),
            live: Mutex::new(None),
        });
        lock(&self.objects).insert(id, obj.clone());
        obj
    }

    /// Returns the object's relay URL, `http://<z32>.localhost:<port>/`.
    pub fn relay_url(&self, id: &EndpointId) -> String {
        format!("http://{}.localhost:{}/", id.to_z32(), self.port())
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

enum WsOut {
    Binary(Vec<u8>),
    Close(u16, &'static str),
}

/// A socket as the runtime holds it; it survives hibernation together with its attachment.
struct Socket {
    tx: mpsc::UnboundedSender<WsOut>,
    attachment: Option<SessionSnapshot>,
}

/// In-memory object state, dropped on hibernation and eviction.
struct Live {
    node: Node,
    host: Arc<HostImpl>,
    sessions: HashMap<u64, IngressSession>,
}

/// One simulated Durable Object.
pub struct DoObject {
    me: Weak<DoObject>,
    id: EndpointId,
    // Persisted, as in the object's SQLite.
    secret: SecretKey,
    timings: Timings,
    wakes: AtomicU64,
    endpoint_builds: AtomicU64,
    callbacks: Mutex<Vec<(EndpointId, Instant)>>,
    log: Mutex<MessageLog>,
    members: Mutex<HashMap<EndpointId, Member>>,
    // Runtime state.
    sockets: Mutex<HashMap<u64, Socket>>,
    next_sid: AtomicU64,
    live: Mutex<Option<Live>>,
}

impl std::fmt::Debug for DoObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DoObject")
            .field("id", &self.id.fmt_short().to_string())
            .finish_non_exhaustive()
    }
}

impl DoObject {
    /// Returns the object's EndpointId.
    pub fn id(&self) -> EndpointId {
        self.id
    }

    /// Returns the number of constructor runs so far.
    pub fn wakes(&self) -> u64 {
        self.wakes.load(Ordering::SeqCst)
    }

    /// Returns the live node, or `None` while hibernated or evicted.
    pub fn node(&self) -> Option<Node> {
        lock(&self.live).as_ref().map(|l| l.node.clone())
    }

    /// Returns the lifecycle state; a hibernated object with sockets is Warm.
    pub fn state(&self) -> State {
        match self.node() {
            Some(n) => n.state(),
            None if lock(&self.sockets).is_empty() => State::Cold,
            None => State::Warm,
        }
    }

    /// Returns the number of sockets the runtime holds.
    pub fn sockets(&self) -> usize {
        lock(&self.sockets).len()
    }

    /// Returns the destination-violation count of each socket, from its last attachment.
    pub fn dst_violations(&self) -> Vec<u32> {
        let cfg = IngressConfig::new(self.id);
        lock(&self.sockets)
            .values()
            .filter_map(|s| s.attachment.clone())
            .map(|a| IngressSession::restore(a, &cfg).dst_violations())
            .collect()
    }

    /// Returns the stored member records.
    pub fn members(&self) -> Vec<Member> {
        lock(&self.members).values().cloned().collect()
    }

    /// Hibernates the object: drops the node and sessions, keeping sockets and attachments.
    ///
    /// # Errors
    ///
    /// Fails if the node is Active or has tasks, which would keep a Durable Object resident.
    pub fn hibernate(&self) -> Result<(), String> {
        let mut live = lock(&self.live);
        let Some(l) = live.as_ref() else {
            return Ok(());
        };
        let (state, stats) = (l.node.state(), l.node.stats());
        if state == State::Active || stats.tasks != 0 {
            return Err(format!(
                "not hibernatable: {state:?}, {} tasks",
                stats.tasks
            ));
        }
        self.drop_live(&mut live);
        info!(id = %self.id.fmt_short(), "hibernated");
        Ok(())
    }

    /// Abrupt eviction: drops the node whatever its state. The old endpoint can no longer send
    /// (its sink is cut) and receives nothing more, like an isolate that is gone.
    pub fn evict(&self) {
        let mut live = lock(&self.live);
        self.drop_live(&mut live);
        info!(id = %self.id.fmt_short(), "evicted");
    }

    /// Drops the in-memory part.
    ///
    /// As in a Durable Object, nothing is saved here: sockets keep only the attachments written
    /// while handling events (at upgrade, on authentication and when the destination-violation
    /// count changes).
    fn drop_live(&self, live: &mut Option<Live>) {
        let Some(l) = live.take() else { return };
        l.host.alive.store(false, Ordering::SeqCst);
    }

    /// Runs the constructor if needed: rebuilds the node from persisted state and the
    /// sessions from the sockets' attachments.
    fn ensure_live<'a>(&self, live: &'a mut Option<Live>) -> &'a mut Live {
        live.get_or_insert_with(|| {
            let wakes = self.wakes.fetch_add(1, Ordering::SeqCst) + 1;
            let host = Arc::new(HostImpl {
                id: self.id,
                alive: AtomicBool::new(true),
                routes: Mutex::new(HashMap::new()),
                object: self.me.clone(),
            });
            let mut config = Config::new(self.secret.clone(), wakes, BUNDLE);
            config.endpoint_builds = self.endpoint_builds.load(Ordering::SeqCst);
            config.t_idle = self.timings.t_idle;
            config.t_silent = self.timings.t_silent;
            config.t_linger = self.timings.t_linger;
            config.max_idle_timeout = self.timings.max_idle_timeout;
            let node = Node::new(config, host.clone());
            let cfg = IngressConfig::new(self.id);
            let mut sessions = HashMap::new();
            for (sid, socket) in lock(&self.sockets).iter() {
                let Some(snap) = socket.attachment.clone() else {
                    continue;
                };
                let session = IngressSession::restore(snap, &cfg);
                if let Some(peer) = session.peer() {
                    lock(&host.routes).insert(peer, (*sid, socket.tx.clone()));
                    node.socket_authenticated(peer);
                }
                sessions.insert(*sid, session);
            }
            info!(id = %self.id.fmt_short(), wakes, restored = sessions.len(), "constructor");
            Live {
                node,
                host,
                sessions,
            }
        })
    }

    fn add_socket(&self, session: IngressSession, tx: mpsc::UnboundedSender<WsOut>) -> u64 {
        let sid = self.next_sid.fetch_add(1, Ordering::SeqCst);
        let mut live = lock(&self.live);
        let l = self.ensure_live(&mut live);
        lock(&self.sockets).insert(
            sid,
            Socket {
                tx,
                attachment: session.snapshot(),
            },
        );
        l.sessions.insert(sid, session);
        sid
    }

    fn on_message(&self, sid: u64, data: &[u8]) {
        let mut live = lock(&self.live);
        let l = self.ensure_live(&mut live);
        let Some(tx) = lock(&self.sockets).get(&sid).map(|s| s.tx.clone()) else {
            return;
        };
        let Some(session) = l.sessions.get_mut(&sid) else {
            return;
        };
        let violations = session.dst_violations();
        let outputs = match session.on_message(data, now()) {
            Ok(o) => o,
            Err(close) => {
                debug!(?close.reason, "closing socket");
                if let Some(f) = &close.frame {
                    let _ = tx.send(WsOut::Binary(f.as_bytes().to_vec()));
                }
                let _ = tx.send(WsOut::Close(close.code, close.reason_text()));
                return;
            }
        };
        let mut superseded = None;
        let mut persist = session.dst_violations() != violations;
        for out in outputs {
            match out {
                Output::Send(f) => {
                    let _ = tx.send(WsOut::Binary(f.into_vec()));
                }
                Output::Authenticated { peer } => {
                    // One route per peer: the newest socket wins and the previous one is
                    // sent `SameEndpointIdConnected`.
                    let previous = lock(&l.host.routes).insert(peer, (sid, tx.clone()));
                    if let Some((old, _)) = previous
                        && old != sid
                    {
                        superseded = Some(old);
                    }
                    l.node.socket_authenticated(peer);
                    persist = true;
                }
                Output::Datagrams { from, batch } => {
                    l.node.datagrams(
                        from,
                        Ecn::to_bits(batch.ecn),
                        Bytes::copy_from_slice(batch.contents),
                        batch.segment_size.map(|s| usize::from(s.get())),
                    );
                }
            }
        }
        if persist && let Some(s) = lock(&self.sockets).get_mut(&sid) {
            s.attachment = l.sessions.get(&sid).and_then(|s| s.snapshot());
        }
        if let Some(old) = superseded {
            let frame = l
                .sessions
                .get(&old)
                .and_then(|s| s.status_frame(Status::SameEndpointIdConnected));
            if let (Some(frame), Some(s)) = (frame, lock(&self.sockets).get(&old)) {
                let _ = s.tx.send(WsOut::Binary(frame.into_vec()));
            }
        }
    }

    fn on_text(&self, sid: u64) {
        let mut live = lock(&self.live);
        let l = self.ensure_live(&mut live);
        if let (Some(session), Some(s)) = (l.sessions.get_mut(&sid), lock(&self.sockets).get(&sid))
        {
            let close = session.on_text_message();
            let _ = s.tx.send(WsOut::Close(close.code, close.reason_text()));
        }
    }

    fn on_close(&self, sid: u64) {
        let mut live = lock(&self.live);
        let l = self.ensure_live(&mut live);
        lock(&self.sockets).remove(&sid);
        let Some(session) = l.sessions.remove(&sid) else {
            return;
        };
        let Some(peer) = session.peer() else { return };
        let mut routes = lock(&l.host.routes);
        if routes.get(&peer).map(|(s, _)| *s) != Some(sid) {
            return;
        }
        routes.remove(&peer);
        // Route to another socket of the same peer, if any, as iroh's relay server does.
        let other = l
            .sessions
            .iter()
            .find(|(_, s)| s.peer() == Some(peer))
            .map(|(sid, _)| *sid);
        if let Some(other) = other
            && let Some(s) = lock(&self.sockets).get(&other)
        {
            routes.insert(peer, (other, s.tx.clone()));
            return;
        }
        drop(routes);
        l.node.socket_closed(peer);
    }

    /// The alarm handler: dial back every due callback.
    fn alarm(&self) {
        let due: Vec<EndpointId> = {
            let mut cbs = lock(&self.callbacks);
            let now = Instant::now();
            let (due, rest): (Vec<_>, Vec<_>) = cbs.drain(..).partition(|(_, at)| *at <= now);
            *cbs = rest;
            due.into_iter().map(|(p, _)| p).collect()
        };
        if due.is_empty() {
            return;
        }
        let mut live = lock(&self.live);
        let l = self.ensure_live(&mut live);
        for peer in due {
            info!(peer = %peer.fmt_short(), "alarm: dial-back");
            l.node.dial_back(peer);
        }
    }
}

/// The node's host: frame output to live sockets, callback scheduling and the message log.
#[derive(Debug)]
struct HostImpl {
    id: EndpointId,
    alive: AtomicBool,
    routes: Mutex<HashMap<EndpointId, (u64, mpsc::UnboundedSender<WsOut>)>>,
    object: Weak<DoObject>,
}

impl FrameSink for HostImpl {
    fn send_datagrams(
        &self,
        peer: &EndpointId,
        contents: &[u8],
        segment_size: Option<usize>,
    ) -> io::Result<()> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(io::ErrorKind::NotConnected.into());
        }
        let routes = lock(&self.routes);
        let Some((_, tx)) = routes.get(peer) else {
            return Err(io::ErrorKind::NotConnected.into());
        };
        let batch = DatagramBatch {
            ecn: None,
            segment_size: segment_size
                .and_then(|s| u16::try_from(s).ok())
                .and_then(NonZeroU16::new),
            contents,
        };
        let frame = encode_datagrams(&self.id, &batch).map_err(io::Error::other)?;
        tx.send(WsOut::Binary(frame.into_vec()))
            .map_err(|_| io::Error::from(io::ErrorKind::NotConnected))
    }
}

/// In-memory equivalent of the object's SQLite `log` table.
#[derive(Debug, Default)]
struct MessageLog {
    rows: std::collections::VecDeque<LogRow>,
    total: u64,
    /// `seq` of the newest room event, as the object's `room_seq` counter.
    room_seq: u64,
}

impl MessageLog {
    fn append(&mut self, peer: &EndpointId, kind: LogKind, nick: &str, text: &str) -> LogRow {
        self.total += 1;
        let prev = if kind.is_room_event() {
            std::mem::replace(&mut self.room_seq, self.total)
        } else {
            0
        };
        let row = LogRow {
            seq: self.total,
            prev,
            ts_ms: now().as_millis() as u64,
            peer: peer.to_z32(),
            kind: kind.as_str().to_string(),
            nick: nick.to_string(),
            text: text.to_string(),
        };
        self.rows.push_back(row.clone());
        let cutoff = row
            .ts_ms
            .saturating_sub(iroh_do_core::LOG_MAX_AGE.as_millis() as u64);
        while self.rows.len() as u64 > iroh_do_core::LOG_RETENTION
            || self.rows.front().is_some_and(|r| r.ts_ms < cutoff)
        {
            self.rows.pop_front();
        }
        row
    }

    fn insert(&mut self, peer: &EndpointId, kind: LogKind, text: &str) -> LogStats {
        self.append(peer, kind, "", text);
        let c = self.counts(peer);
        LogStats {
            seq: self.total,
            total: c.total,
            yours: c.yours,
        }
    }

    fn counts(&self, peer: &EndpointId) -> Counts {
        let z = peer.to_z32();
        let peers: std::collections::HashSet<&str> =
            self.rows.iter().map(|r| r.peer.as_str()).collect();
        Counts {
            total: self.total,
            yours: self.rows.iter().filter(|r| r.peer == z).count() as u64,
            peers: peers.len() as u64,
        }
    }
}

impl DoObject {
    /// Returns the number of records ever inserted in the message log.
    pub fn log_total(&self) -> u64 {
        lock(&self.log).total
    }

    /// The retained message-log records, oldest first.
    pub fn log_rows(&self) -> Vec<LogRow> {
        lock(&self.log).rows.iter().cloned().collect()
    }
}

impl Host for HostImpl {
    fn log_event(&self, peer: EndpointId, kind: LogKind, text: &str) -> LogStats {
        match self.object.upgrade() {
            Some(obj) => lock(&obj.log).insert(&peer, kind, text),
            None => LogStats::default(),
        }
    }

    fn count(&self, peer: EndpointId) -> Counts {
        match self.object.upgrade() {
            Some(obj) => lock(&obj.log).counts(&peer),
            None => Counts::default(),
        }
    }

    fn total(&self) -> u64 {
        self.object.upgrade().map_or(0, |obj| lock(&obj.log).total)
    }

    fn append(&self, peer: EndpointId, kind: LogKind, nick: &str, text: &str) -> Option<LogRow> {
        let obj = self.object.upgrade()?;
        Some(lock(&obj.log).append(&peer, kind, nick, text))
    }

    fn room_seq(&self) -> u64 {
        self.object
            .upgrade()
            .map_or(0, |obj| lock(&obj.log).room_seq)
    }

    fn events_after(&self, seq: u64, limit: usize) -> Vec<LogRow> {
        let Some(obj) = self.object.upgrade() else {
            return Vec::new();
        };
        let log = lock(&obj.log);
        log.rows
            .iter()
            .filter(|r| r.seq > seq && ROOM_EVENT_KINDS.contains(&r.kind.as_str()))
            .take(limit)
            .cloned()
            .collect()
    }

    fn events_recent(&self, n: usize) -> Vec<LogRow> {
        let Some(obj) = self.object.upgrade() else {
            return Vec::new();
        };
        let log = lock(&obj.log);
        let mut rows: Vec<LogRow> = log
            .rows
            .iter()
            .rev()
            .filter(|r| ROOM_EVENT_KINDS.contains(&r.kind.as_str()))
            .take(n)
            .cloned()
            .collect();
        rows.reverse();
        rows
    }

    fn member(&self, peer: EndpointId) -> Option<Member> {
        let obj = self.object.upgrade()?;
        lock(&obj.members).get(&peer).cloned()
    }

    fn members(&self) -> Vec<Member> {
        let Some(obj) = self.object.upgrade() else {
            return Vec::new();
        };
        let mut m: Vec<Member> = lock(&obj.members)
            .values()
            .filter(|m| m.joined)
            .cloned()
            .collect();
        m.sort_by_key(|m| m.nick.to_ascii_lowercase());
        m
    }

    fn set_member(&self, member: &Member) {
        if let Some(obj) = self.object.upgrade() {
            lock(&obj.members).insert(member.peer, member.clone());
        }
    }

    fn recent(&self, n: usize) -> Vec<LogRow> {
        let Some(obj) = self.object.upgrade() else {
            return Vec::new();
        };
        let log = lock(&obj.log);
        let skip = log.rows.len().saturating_sub(n);
        log.rows.iter().skip(skip).cloned().collect()
    }

    fn endpoint_built(&self) {
        if let Some(obj) = self.object.upgrade() {
            obj.endpoint_builds.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn schedule_callback(&self, peer: EndpointId, delay: Duration) {
        let Some(obj) = self.object.upgrade() else {
            return;
        };
        lock(&obj.callbacks).push((peer, Instant::now() + delay));
        let weak = self.object.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if let Some(obj) = weak.upgrade() {
                obj.alarm();
            }
        });
    }
}

/// Answers `GET /ping` and `GET /generate_204` as the router Worker does, without touching an
/// object; returns `false`, consuming nothing, for any other request.
///
/// iroh's net_report probes a client's home relay with `GET <relay URL>/ping` and
/// `GET http://<relay host>/generate_204` (iroh 1.4.0, `src/net_report/reportgen.rs`,
/// `run_https_probe` and `check_captive_portal`).
async fn answer_probe(stream: &mut TcpStream) -> bool {
    use tokio::io::AsyncWriteExt;
    let mut buf = [0u8; 2048];
    let Ok(n) = stream.peek(&mut buf).await else {
        return false;
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    let path = head.split_whitespace().nth(1).unwrap_or_default();
    if !head.starts_with("GET ") || head.to_ascii_lowercase().contains("upgrade: websocket") {
        return false;
    }
    let resp = match path {
        "/ping" => "HTTP/1.1 200 OK\r\ncontent-length: 0\r\naccess-control-allow-origin: *\r\nconnection: close\r\n\r\n".to_string(),
        "/generate_204" => {
            let challenge = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("x-iroh-challenge").then(|| v.trim().to_string())
                })
                .map(|c| format!("x-iroh-response: response {c}\r\n"))
                .unwrap_or_default();
            format!("HTTP/1.1 204 No Content\r\n{challenge}connection: close\r\n\r\n")
        }
        _ => return false,
    };
    info!(path, "probe answered");
    let _ = tokio::io::AsyncReadExt::read(stream, &mut buf).await;
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
    true
}

/// Routes, upgrades and serves one TCP connection.
async fn serve_socket(mut stream: TcpStream, objects: Objects) {
    if answer_probe(&mut stream).await {
        return;
    }
    let mut upgraded: Option<(Arc<DoObject>, IngressSession, Vec<u8>)> = None;
    let callback = |req: &Request, mut resp: Response| -> Result<Response, ErrorResponse> {
        let reject = |code: u16| {
            let mut r = ErrorResponse::new(None);
            *r.status_mut() = StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST);
            r
        };
        // The leftmost `Host` label must be the z-base-32 EndpointId of a minted object.
        let host = req
            .headers()
            .get("host")
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default();
        let label = host.split('.').next().unwrap_or_default();
        let Ok(id) = EndpointId::from_z32(label) else {
            return Err(reject(404));
        };
        let Some(obj) = lock(&objects).get(&id).cloned() else {
            return Err(reject(404));
        };
        let proto = req
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|h| h.to_str().ok());
        let ureq = UpgradeRequest {
            method: req.method().as_str(),
            path: req.uri().path(),
            sec_websocket_protocol: proto,
        };
        let cfg = IngressConfig::new(id);
        match IngressSession::on_upgrade(&ureq, &cfg, &mut rand::rng(), now()) {
            Ok((ur, session)) => {
                let (name, value) = ur.subprotocol_header();
                resp.headers_mut()
                    .insert(name, HeaderValue::from_static(value));
                upgraded = Some((obj, session, ur.challenge.into_vec()));
                Ok(resp)
            }
            Err(e) => {
                debug!("upgrade rejected: {e}");
                Err(reject(e.http_status()))
            }
        }
    };
    let ws = match tokio_tungstenite::accept_hdr_async(stream, callback).await {
        Ok(ws) => ws,
        Err(e) => {
            debug!("ws accept failed: {e}");
            return;
        }
    };
    let Some((obj, session, challenge)) = upgraded else {
        return;
    };
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel();
    // The challenge is the first message, before anything the node could send.
    let _ = tx.send(WsOut::Binary(challenge));
    let sid = obj.add_socket(session, tx);
    let writer = tokio::spawn(async move {
        while let Some(out) = rx.recv().await {
            let res = match out {
                WsOut::Binary(b) => sink.send(Message::Binary(b.into())).await,
                WsOut::Close(code, reason) => {
                    let _ = sink
                        .send(Message::Close(Some(CloseFrame {
                            code: CloseCode::from(code),
                            reason: reason.into(),
                        })))
                        .await;
                    break;
                }
            };
            if res.is_err() {
                break;
            }
        }
    });
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Binary(b)) => obj.on_message(sid, &b),
            Ok(Message::Text(_)) => obj.on_text(sid),
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    obj.on_close(sid);
    writer.abort();
    debug!(sid, "socket closed");
}
