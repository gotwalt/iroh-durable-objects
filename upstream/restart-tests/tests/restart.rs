//! Restart-state tests for the `upstream/patches/iroh` patch set.
//!
//! Topology (native UDP, loopback):
//!
//! ```text
//! client [::1]:x  <-->  proxy [::1]:q | 127.0.0.1:y  <-->  server 127.0.0.1:p
//! ```
//!
//! The client is IPv6-only and the server IPv4-only, so every packet goes through the
//! proxy even if the server advertises its direct address (QNT). The proxy adds a fixed
//! one-way delay, which gives a known RTT, and can capture the client's datagrams of a
//! handshake (Initial + 0-RTT + Handshake) for the replay tests.
//!
//! "Restart" means: the server endpoint `A` is dropped (abruptly or after `close()`), and a
//! new endpoint `A'` is bound with the same `SecretKey` on the same `127.0.0.1:p`.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use iroh::{
    Endpoint, EndpointAddr, SecretKey,
    endpoint::{
        Builder, ConnectOptions, Connection, ConnectionError, ConnectionState, PortmapperConfig,
        QuicTransportConfig, ZeroRttStatus, presets,
    },
};
use n0_error::{Result, StdResultExt};
use rustls::server::StoresServerSessions;
use tokio::{
    net::UdpSocket,
    sync::mpsc,
    task::{JoinHandle, JoinSet},
};

const ALPN: &[u8] = b"iroh-restart-state/test/0";
const RESET_KEY: [u8; 32] = [0x5a; 32];
/// Client-side idle timeout. Bounds how long a peer waits without stateless resets.
const IDLE_TIMEOUT: Duration = Duration::from_secs(6);

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

fn transport_config() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .max_idle_timeout(Some(IDLE_TIMEOUT.try_into().unwrap()))
        .build()
}

// ---------------------------------------------------------------------------------------
// Server session store with observable single-use semantics.

#[derive(Debug, Default)]
struct TestStore {
    map: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    /// Simulates a broken store whose `take` does not remove (e.g. a lagging replica).
    take_is_get: bool,
    puts: AtomicUsize,
    take_hits: AtomicUsize,
    take_misses: AtomicUsize,
}

impl TestStore {
    fn new(take_is_get: bool) -> Arc<Self> {
        Arc::new(Self {
            take_is_get,
            ..Default::default()
        })
    }
    fn counts(&self) -> (usize, usize, usize) {
        (
            self.puts.load(Ordering::SeqCst),
            self.take_hits.load(Ordering::SeqCst),
            self.take_misses.load(Ordering::SeqCst),
        )
    }
}

impl StoresServerSessions for TestStore {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> bool {
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.map.lock().unwrap().insert(key, value);
        true
    }
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.map.lock().unwrap().get(key).cloned()
    }
    fn take(&self, key: &[u8]) -> Option<Vec<u8>> {
        let mut map = self.map.lock().unwrap();
        let v = if self.take_is_get {
            map.get(key).cloned()
        } else {
            map.remove(key)
        };
        let ctr = if v.is_some() {
            &self.take_hits
        } else {
            &self.take_misses
        };
        ctr.fetch_add(1, Ordering::SeqCst);
        v
    }
    fn can_cache(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------------------
// Delaying, capturing UDP proxy.

#[derive(Default)]
struct Capture {
    armed: bool,
    dgrams: Vec<Vec<u8>>,
}

struct Proxy {
    listen: SocketAddr,
    capture: Arc<Mutex<Capture>>,
    /// Drops server-to-client datagrams, so a crashing server cannot leak anything.
    cut_downstream: Arc<AtomicBool>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Proxy {
    async fn spawn(server: SocketAddr, one_way: Duration) -> Self {
        let front = Arc::new(UdpSocket::bind("[::1]:0").await.unwrap());
        let back = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let listen = front.local_addr().unwrap();
        let client: Arc<Mutex<Option<SocketAddr>>> = Default::default();
        let capture: Arc<Mutex<Capture>> = Default::default();
        let cut_downstream: Arc<AtomicBool> = Default::default();

        let up = tokio::spawn({
            let (front, back, client, capture) =
                (front.clone(), back.clone(), client.clone(), capture.clone());
            async move {
                let mut buf = vec![0u8; 65536];
                loop {
                    let Ok((n, from)) = front.recv_from(&mut buf).await else {
                        continue;
                    };
                    *client.lock().unwrap() = Some(from);
                    let d = buf[..n].to_vec();
                    {
                        let mut c = capture.lock().unwrap();
                        if c.armed {
                            c.dgrams.push(d.clone());
                        }
                    }
                    let back = back.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(one_way).await;
                        let _ = back.send_to(&d, server).await;
                    });
                }
            }
        });
        let down = tokio::spawn({
            let (front, back, client) = (front.clone(), back.clone(), client.clone());
            let cut = cut_downstream.clone();
            async move {
                let mut buf = vec![0u8; 65536];
                loop {
                    let Ok((n, _)) = back.recv_from(&mut buf).await else {
                        continue;
                    };
                    if cut.load(Ordering::SeqCst) {
                        continue;
                    }
                    let Some(to) = *client.lock().unwrap() else {
                        continue;
                    };
                    let d = buf[..n].to_vec();
                    let front = front.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(one_way).await;
                        let _ = front.send_to(&d, to).await;
                    });
                }
            }
        });
        Self {
            listen,
            capture,
            cut_downstream,
            tasks: vec![up, down],
        }
    }

    fn cut_downstream(&self, cut: bool) {
        self.cut_downstream.store(cut, Ordering::SeqCst);
    }

    fn arm_capture(&self) {
        *self.capture.lock().unwrap() = Capture {
            armed: true,
            ..Default::default()
        };
    }

    /// Stops capturing and returns the client-to-server datagrams seen since arming.
    fn stop_capture(&self) -> Vec<Vec<u8>> {
        let mut c = self.capture.lock().unwrap();
        c.armed = false;
        std::mem::take(&mut c.dgrams)
    }
}

// ---------------------------------------------------------------------------------------
// Endpoints.

#[derive(Clone, Default)]
struct RestartState {
    reset_key: Option<[u8; 32]>,
    store: Option<Arc<TestStore>>,
}

fn server_builder(secret: &SecretKey, port: u16, state: &RestartState) -> Builder {
    let mut b = Endpoint::builder(presets::Minimal)
        .secret_key(secret.clone())
        .alpns(vec![ALPN.to_vec()])
        .portmapper_config(PortmapperConfig::Disabled)
        .transport_config(transport_config())
        .clear_ip_transports()
        .bind_addr(SocketAddr::from(([127, 0, 0, 1], port)))
        .unwrap();
    if let Some(key) = state.reset_key {
        b = b.stateless_reset_key(key);
    }
    if let Some(store) = &state.store {
        b = b.tls_server_session_store(store.clone());
    }
    b
}

/// Binds the server, retrying while the previous instance releases the port.
async fn bind_server(secret: &SecretKey, port: u16, state: &RestartState) -> Endpoint {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match server_builder(secret, port, state).bind().await {
            Ok(ep) => return ep,
            Err(err) if Instant::now() < deadline => {
                eprintln!("rebind on port {port} failed ({err}), retrying");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(err) => panic!("bind failed: {err:?}"),
        }
    }
}

async fn client_endpoint() -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .portmapper_config(PortmapperConfig::Disabled)
        .transport_config(transport_config())
        .clear_ip_transports()
        .bind_addr("[::1]:0")
        .unwrap()
        .bind()
        .await
        .unwrap()
}

#[derive(Debug)]
struct Received {
    data: Vec<u8>,
    is_0rtt: bool,
}

/// Echo server: accepts in 0-RTT mode and reports every received stream.
fn serve(ep: Endpoint, events: mpsc::UnboundedSender<Received>) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Dropping the JoinSet (when this task is aborted) aborts every connection task,
        // so no `Connection` keeps the endpoint alive.
        let mut conns = JoinSet::new();
        while let Some(incoming) = ep.accept().await {
            let Ok(accepting) = incoming.accept() else {
                continue;
            };
            let events = events.clone();
            conns.spawn(async move {
                let conn = accepting.into_0rtt();
                // Drive registration with the socket once the handshake completes.
                let hs = conn.clone();
                let _hs = tokio::spawn(async move { hs.handshake_completed().await.ok() });
                while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                    let Ok(data) = recv.read_to_end(1024).await else {
                        break;
                    };
                    let _ = events.send(Received {
                        data: data.clone(),
                        is_0rtt: recv.is_0rtt(),
                    });
                    let _ = send.write_all(&data).await;
                    let _ = send.finish();
                }
            });
        }
    })
}

async fn echo<T: ConnectionState>(conn: &Connection<T>, msg: &[u8]) -> Result<Vec<u8>> {
    let (mut send, mut recv) = conn.open_bi().await.anyerr()?;
    send.write_all(msg).await.anyerr()?;
    send.finish().anyerr()?;
    recv.read_to_end(1024).await.anyerr()
}

struct Server {
    ep: Endpoint,
    task: JoinHandle<()>,
    events: mpsc::UnboundedReceiver<Received>,
}

impl Server {
    async fn bind(secret: &SecretKey, port: u16, state: &RestartState) -> Self {
        let ep = bind_server(secret, port, state).await;
        let (tx, events) = mpsc::unbounded_channel();
        let task = serve(ep.clone(), tx);
        Self { ep, task, events }
    }

    fn port(&self) -> u16 {
        self.ep.bound_sockets()[0].port()
    }

    /// Simulates a crash: no CONNECTION_CLOSE, no graceful shutdown.
    async fn kill(self) {
        self.task.abort();
        let _ = self.task.await;
        drop(self.ep);
    }

    async fn close(self) {
        self.ep.close().await;
        self.task.abort();
        let _ = self.task.await;
    }
}

// ---------------------------------------------------------------------------------------
// (1) Abrupt restart: stateless reset.

struct AbruptOutcome {
    rtt: Duration,
    error: Option<ConnectionError>,
    elapsed: Duration,
    redial_ok: bool,
}

async fn abrupt_restart(state: RestartState, one_way: Duration) -> AbruptOutcome {
    init_tracing();
    let secret = SecretKey::generate();
    let a = Server::bind(&secret, 0, &state).await;
    let port = a.port();
    let proxy = Proxy::spawn(SocketAddr::from(([127, 0, 0, 1], port)), one_way).await;
    let addr = EndpointAddr::new(secret.public()).with_ip_addr(proxy.listen);

    let client = client_endpoint().await;
    let conn = client.connect(addr.clone(), ALPN).await.unwrap();
    assert_eq!(echo(&conn, b"warm-up").await.unwrap(), b"warm-up");
    let t = Instant::now();
    assert_eq!(echo(&conn, b"ping").await.unwrap(), b"ping");
    let rtt = t.elapsed();

    // Crash A, then bring up A' with the same identity on the same address.
    proxy.cut_downstream(true);
    a.kill().await;
    let a2 = Server::bind(&secret, port, &state).await;
    proxy.cut_downstream(false);

    // The next packet the client sends on the old connection reaches A'.
    let t0 = Instant::now();
    let trigger = tokio::spawn({
        let conn = conn.clone();
        async move { echo(&conn, b"after-restart").await }
    });
    let error = tokio::time::timeout(IDLE_TIMEOUT * 2, conn.closed())
        .await
        .ok();
    let elapsed = t0.elapsed();
    trigger.abort();

    let redial_ok = match client.connect(addr, ALPN).await {
        Ok(c) => echo(&c, b"redial").await.ok().as_deref() == Some(&b"redial"[..]),
        Err(_) => false,
    };
    client.close().await;
    a2.close().await;
    AbruptOutcome {
        rtt,
        error,
        elapsed,
        redial_ok,
    }
}

const ONE_WAY: Duration = Duration::from_millis(25);

#[tokio::test(flavor = "multi_thread")]
async fn abrupt_restart_with_reset_key_fails_fast() {
    let out = abrupt_restart(
        RestartState {
            reset_key: Some(RESET_KEY),
            store: None,
        },
        ONE_WAY,
    )
    .await;
    eprintln!(
        "with reset key: rtt={:?} error={:?} elapsed={:?} ({:.2} RTT)",
        out.rtt,
        out.error,
        out.elapsed,
        out.elapsed.as_secs_f64() / out.rtt.as_secs_f64()
    );
    assert!(
        matches!(out.error, Some(ConnectionError::Reset)),
        "expected stateless reset, got {:?}",
        out.error
    );
    assert!(
        out.elapsed < 2 * out.rtt,
        "reset took {:?}, RTT {:?}",
        out.elapsed,
        out.rtt
    );
    assert!(out.redial_ok, "redial to A' failed");
}

/// Today's behaviour (no reset key): the peer waits for its idle timeout.
#[tokio::test(flavor = "multi_thread")]
async fn abrupt_restart_default_waits_for_idle_timeout() {
    let out = abrupt_restart(RestartState::default(), ONE_WAY).await;
    eprintln!(
        "default: rtt={:?} error={:?} elapsed={:?}",
        out.rtt, out.error, out.elapsed
    );
    assert!(
        matches!(out.error, Some(ConnectionError::TimedOut)),
        "expected idle timeout, got {:?}",
        out.error
    );
    assert!(out.elapsed > IDLE_TIMEOUT / 2, "{:?}", out.elapsed);
    assert!(out.redial_ok, "redial to A' failed");
}

/// Reset key on A but a *different* one on A': the peer must ignore the forged reset.
#[tokio::test(flavor = "multi_thread")]
async fn abrupt_restart_with_wrong_reset_key_is_ignored() {
    init_tracing();
    let secret = SecretKey::generate();
    let a = Server::bind(
        &secret,
        0,
        &RestartState {
            reset_key: Some([1; 32]),
            store: None,
        },
    )
    .await;
    let port = a.port();
    let proxy = Proxy::spawn(SocketAddr::from(([127, 0, 0, 1], port)), ONE_WAY).await;
    let addr = EndpointAddr::new(secret.public()).with_ip_addr(proxy.listen);
    let client = client_endpoint().await;
    let conn = client.connect(addr, ALPN).await.unwrap();
    echo(&conn, b"x").await.unwrap();
    proxy.cut_downstream(true);
    a.kill().await;
    let a2 = Server::bind(
        &secret,
        port,
        &RestartState {
            reset_key: Some([2; 32]),
            store: None,
        },
    )
    .await;
    proxy.cut_downstream(false);
    let t0 = Instant::now();
    let trigger = tokio::spawn({
        let conn = conn.clone();
        async move { echo(&conn, b"y").await }
    });
    let err = conn.closed().await;
    eprintln!("wrong key: error={err:?} elapsed={:?}", t0.elapsed());
    trigger.abort();
    assert!(matches!(err, ConnectionError::TimedOut), "{err:?}");
    client.close().await;
    a2.close().await;
}

// ---------------------------------------------------------------------------------------
// (2) 0-RTT after restart, (3) replay of a used ticket.

struct ZeroRttRun {
    secret: SecretKey,
    port: u16,
    accepted: bool,
    server_saw_0rtt: bool,
    /// Every client datagram from the first packet until the handshake completed.
    client_flight: Vec<Vec<u8>>,
    server: Server,
    _client: Endpoint,
}

/// A serves a 1-RTT connection (issuing tickets), is closed, A' comes up; the client then
/// dials A' with 0-RTT. `store_for_a2` decides what A' uses as session store.
async fn zero_rtt_after_restart(
    store_for_a: Option<Arc<TestStore>>,
    store_for_a2: Option<Arc<TestStore>>,
) -> ZeroRttRun {
    init_tracing();
    let secret = SecretKey::generate();
    let state_a = RestartState {
        reset_key: Some(RESET_KEY),
        store: store_for_a,
    };
    let a = Server::bind(&secret, 0, &state_a).await;
    let port = a.port();
    let proxy = Proxy::spawn(
        SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(10),
    )
    .await;
    let addr = EndpointAddr::new(secret.public()).with_ip_addr(proxy.listen);
    let client = client_endpoint().await;

    // Session 1: full handshake; A issues tickets into its store.
    let conn = client.connect(addr.clone(), ALPN).await.unwrap();
    assert_eq!(echo(&conn, b"one").await.unwrap(), b"one");
    // NewSessionTicket is sent after the handshake; give the client time to store it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    conn.close(0u32.into(), b"done");
    a.close().await;

    // Session 2: A' with the same identity on the same address.
    let mut a2 = Server::bind(
        &secret,
        port,
        &RestartState {
            reset_key: Some(RESET_KEY),
            store: store_for_a2,
        },
    )
    .await;
    proxy.arm_capture();
    let connecting = client
        .connect_with_opts(addr, ALPN, ConnectOptions::new())
        .await
        .unwrap();
    let zrtt = connecting
        .into_0rtt()
        .unwrap_or_else(|_| panic!("client holds no ticket for A"));
    let (mut send, mut recv) = zrtt.open_bi().await.unwrap();
    send.write_all(b"zero-rtt hello").await.unwrap();
    send.finish().unwrap();
    let status = zrtt.handshake_completed().await.unwrap();
    let client_flight = proxy.stop_capture();
    let accepted = matches!(status, ZeroRttStatus::Accepted(_));
    if accepted {
        assert_eq!(recv.read_to_end(1024).await.unwrap(), b"zero-rtt hello");
    }
    let server_saw_0rtt = if accepted {
        let ev = tokio::time::timeout(Duration::from_secs(2), a2.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev.data, b"zero-rtt hello");
        ev.is_0rtt
    } else {
        false
    };
    drop(proxy);
    ZeroRttRun {
        secret,
        port,
        accepted,
        server_saw_0rtt,
        client_flight,
        server: a2,
        _client: client,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn zero_rtt_accepted_after_restart_with_persistent_store() {
    let store = TestStore::new(false);
    let run = zero_rtt_after_restart(Some(store.clone()), Some(store.clone())).await;
    eprintln!(
        "persistent store: accepted={} server_saw_0rtt={} store(puts,hits,misses)={:?}",
        run.accepted,
        run.server_saw_0rtt,
        store.counts()
    );
    // `Accepted` is the server's verdict on early data. Whether the stream bytes actually
    // left in 0-RTT packets (`server_saw_0rtt`) depends on client task timing; the replay
    // tests below require and retry for that.
    assert!(run.accepted, "0-RTT rejected after restart");
    run.server.close().await;
}

/// Today's behaviour: a fresh per-bind store, so 0-RTT is rejected after a restart.
#[tokio::test(flavor = "multi_thread")]
async fn zero_rtt_rejected_after_restart_by_default() {
    let run = zero_rtt_after_restart(None, None).await;
    eprintln!("default store: accepted={}", run.accepted);
    assert!(
        !run.accepted,
        "0-RTT unexpectedly accepted with default store"
    );
    run.server.close().await;
}

/// Replays every client datagram of the accepted 0-RTT handshake (Initial, 0-RTT,
/// Handshake) from a different address against A'', a further restart sharing the same
/// store. Returns what A'' delivered to the application.
async fn replay_after_second_restart(take_is_get: bool) -> (Option<Received>, Arc<TestStore>) {
    // Retry until the client's stream bytes actually left in 0-RTT packets, so the
    // captured flight carries early data (see `server_saw_0rtt`).
    let mut attempts = 0;
    let (store, run) = loop {
        attempts += 1;
        let store = TestStore::new(take_is_get);
        let run = zero_rtt_after_restart(Some(store.clone()), Some(store.clone())).await;
        assert!(run.accepted, "0-RTT rejected after restart");
        if run.server_saw_0rtt {
            break (store, run);
        }
        assert!(
            attempts < 10,
            "never captured a flight with 0-RTT stream data"
        );
        run.server.close().await;
    };
    assert!(!run.client_flight.is_empty());
    run.server.close().await;
    let before = store.counts();

    let mut a3 = Server::bind(
        &run.secret,
        run.port,
        &RestartState {
            reset_key: Some(RESET_KEY),
            store: Some(store.clone()),
        },
    )
    .await;
    let attacker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let target = SocketAddr::from(([127, 0, 0, 1], run.port));
    for d in &run.client_flight {
        attacker.send_to(d, target).await.unwrap();
    }
    let delivered = tokio::time::timeout(Duration::from_secs(2), a3.events.recv())
        .await
        .ok()
        .flatten();
    let after = store.counts();
    eprintln!(
        "replay (take_is_get={take_is_get}): {} datagrams, delivered={delivered:?}, store(puts,hits,misses) before={before:?} after={after:?}",
        run.client_flight.len()
    );
    a3.close().await;
    (delivered, store)
}

#[tokio::test(flavor = "multi_thread")]
async fn replayed_0rtt_flight_rejected_after_restart() {
    let (delivered, store) = replay_after_second_restart(false).await;
    assert!(
        delivered.is_none(),
        "replayed 0-RTT data reached the application: {delivered:?}"
    );
    let (_, _, misses) = store.counts();
    assert!(misses >= 1, "replayed ticket was not looked up");
}

/// Control: the same replay against a store whose `take` does not remove is accepted, so
/// the test above fails for the right reason (single-use `take`), not because the replay
/// is malformed.
#[tokio::test(flavor = "multi_thread")]
async fn replayed_0rtt_flight_accepted_if_take_is_not_single_use() {
    let (delivered, _) = replay_after_second_restart(true).await;
    let delivered = delivered.expect("control replay was not delivered");
    assert_eq!(delivered.data, b"zero-rtt hello");
    assert!(delivered.is_0rtt);
}
