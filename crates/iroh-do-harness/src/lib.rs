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
//! A [`DoObject`] divides its state as a Durable Object does. What the object keeps in storage
//! (identity, `wakes`, endpoint builds, scheduled work, and the app's SQLite database, here an
//! in-memory `rusqlite` connection) and what the runtime keeps across hibernation (sockets,
//! with session snapshots as their attachments) survive [`DoObject::hibernate`] and
//! [`DoObject::evict`]. The in-memory part, the [`Node`] with its [`App`] and the live
//! `IngressSession`s, does not. The next event rebuilds it as a constructor would: `wakes` is
//! incremented, the app's schema is run, sessions are restored from attachments, and their
//! peers are announced to the node again.

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
    App, AppEntry, Config, EndpointId, FrameSink, Host, Node, SecretKey, Sql, SqlError, State,
    Stats, Value,
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

    /// Creates an object running `A` with a fresh identity.
    pub fn mint<A: App>(&self, timings: Timings) -> Arc<DoObject> {
        self.mint_entry(AppEntry::of::<A>(), timings)
    }

    /// Creates an object running the app of `entry` with a fresh identity.
    pub fn mint_entry(&self, app: AppEntry, timings: Timings) -> Arc<DoObject> {
        let secret = SecretKey::generate();
        let id = secret.public();
        let db = rusqlite::Connection::open_in_memory().expect("in-memory SQLite");
        let obj = Arc::new_cyclic(|me| DoObject {
            me: me.clone(),
            id,
            secret,
            app,
            timings,
            wakes: AtomicU64::new(0),
            endpoint_builds: AtomicU64::new(0),
            schedule: Mutex::new(HashMap::new()),
            db: Mutex::new(db),
            sockets: Mutex::new(HashMap::new()),
            next_sid: AtomicU64::new(0),
            live: Mutex::new(None),
            bind_callbacks: Mutex::new(Vec::new()),
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
    // Persisted, as in the object's storage.
    secret: SecretKey,
    app: AppEntry,
    timings: Timings,
    wakes: AtomicU64,
    endpoint_builds: AtomicU64,
    /// Scheduled work by key: due time and payload.
    schedule: Mutex<HashMap<String, (Instant, String)>>,
    db: Mutex<rusqlite::Connection>,
    // Runtime state.
    sockets: Mutex<HashMap<u64, Socket>>,
    next_sid: AtomicU64,
    live: Mutex<Option<Live>>,
    /// `endpoint_built` and `endpoint_closed` calls, with the node state each read back.
    bind_callbacks: Mutex<Vec<(&'static str, State)>>,
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

    /// Returns the host's `endpoint_built` and `endpoint_closed` calls in order, each with the
    /// node state the host read back from inside the call.
    pub fn bind_callbacks(&self) -> Vec<(&'static str, State)> {
        lock(&self.bind_callbacks).clone()
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

    /// Runs `f` on the object's database, as the app sees it.
    pub fn with_sql<T>(&self, f: impl FnOnce(&dyn Sql) -> T) -> T {
        let db = lock(&self.db);
        f(&RusqliteSql(&db))
    }

    /// Runs `f` on the live app, if the object is live and runs an `A`.
    pub fn with_app<A: App, T>(&self, f: impl FnOnce(&A) -> T) -> Option<T> {
        let node = self.node()?;
        let app = node.app()?.as_any().downcast_ref::<A>()?;
        Some(f(app))
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
            {
                let db = lock(&self.db);
                for stmt in (self.app.schema)() {
                    if let Err(e) = db.execute_batch(stmt) {
                        tracing::error!(error = %e, "app schema failed");
                    }
                }
            }
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
            let node = Node::new(config, host.clone(), &self.app);
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

    /// The alarm handler: runs every due entry.
    fn alarm(&self) {
        let due: Vec<(String, String)> = {
            let mut sched = lock(&self.schedule);
            let now = Instant::now();
            let keys: Vec<String> = sched
                .iter()
                .filter(|(_, (at, _))| *at <= now)
                .map(|(k, _)| k.clone())
                .collect();
            keys.into_iter()
                .filter_map(|k| sched.remove(&k).map(|(_, p)| (k, p)))
                .collect()
        };
        if due.is_empty() {
            return;
        }
        let node = {
            let mut live = lock(&self.live);
            self.ensure_live(&mut live).node.clone()
        };
        for (key, payload) in due {
            info!(key, "alarm");
            node.alarm(&key, &payload);
        }
    }
}

/// [`Sql`] over a `rusqlite` connection.
struct RusqliteSql<'a>(&'a rusqlite::Connection);

impl Sql for RusqliteSql<'_> {
    fn exec(&self, query: &str, params: &[Value]) -> Result<Vec<Vec<Value>>, SqlError> {
        use rusqlite::types::{ToSqlOutput, Value as V, ValueRef};
        let err = |e: rusqlite::Error| SqlError(e.to_string());
        let mut stmt = self.0.prepare(query).map_err(err)?;
        let params: Vec<ToSqlOutput<'_>> = params
            .iter()
            .map(|p| {
                ToSqlOutput::Owned(match p {
                    Value::Null => V::Null,
                    Value::Integer(i) => V::Integer(*i),
                    Value::Real(f) => V::Real(*f),
                    Value::Text(t) => V::Text(t.clone()),
                    Value::Blob(b) => V::Blob(b.clone()),
                })
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

/// The node's host: frame output to live sockets, scheduling and SQL.
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

impl Host for HostImpl {
    fn with_sql(&self, f: &mut dyn FnMut(&dyn Sql)) {
        // A dropped object (hibernated with no live node left) still has its database, as a
        // Durable Object does; only a destroyed one would not.
        if let Some(obj) = self.object.upgrade() {
            let db = lock(&obj.db);
            f(&RusqliteSql(&db));
        }
    }

    fn endpoint_built(&self) {
        if let Some(obj) = self.object.upgrade() {
            obj.endpoint_builds.fetch_add(1, Ordering::SeqCst);
            // Calls back into the node, as a host is allowed to here (`Host` docs).
            if let Some(n) = obj.node() {
                lock(&obj.bind_callbacks).push(("built", n.state()));
            }
        }
    }

    fn endpoint_closed(&self) {
        if let Some(obj) = self.object.upgrade()
            && let Some(n) = obj.node()
        {
            lock(&obj.bind_callbacks).push(("closed", n.state()));
        }
    }

    fn schedule(&self, key: &str, delay: Duration, payload: &str) {
        let Some(obj) = self.object.upgrade() else {
            return;
        };
        lock(&obj.schedule).insert(
            key.to_string(),
            (Instant::now() + delay, payload.to_string()),
        );
        let weak = self.object.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if let Some(obj) = weak.upgrade() {
                obj.alarm();
            }
        });
    }

    fn cancel(&self, key: &str) {
        if let Some(obj) = self.object.upgrade() {
            lock(&obj.schedule).remove(key);
        }
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
