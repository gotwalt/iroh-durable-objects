//! The library with test apps: iroh's echo example handler, unchanged, and a small app that
//! schedules work and dials peers. Peers run stock iroh 1.4.0.

use std::time::Duration;

use iroh::{
    Endpoint, EndpointAddr, RelayMap, RelayMode,
    endpoint::{Connection, ConnectionError, presets},
    protocol::{AcceptError, ProtocolHandler},
};
use iroh_do_core::{App, ConnectError, Ctx, Protocols};
use iroh_do_harness::{DoObject, Harness, State, Timings};
use tokio::time::{Instant, sleep, timeout};

/// The ALPN of iroh 1.4.0 `examples/echo.rs`.
const ECHO: &[u8] = b"iroh-example/echo/0";

/// Serves iroh's echo example handler, as `examples/echo` does.
struct EchoApp;

impl App for EchoApp {
    const NAME: &'static str = "echo";

    fn new(_ctx: &Ctx) -> Self {
        EchoApp
    }

    fn protocols(&self, _ctx: &Ctx, p: &mut Protocols) {
        p.accept(ECHO, Echo);
    }
}

// From here to `peer`, iroh 1.4.0 `examples/echo.rs`.

#[derive(Debug, Clone)]
struct Echo;

impl ProtocolHandler for Echo {
    /// The `accept` method is called for each incoming connection for our ALPN.
    ///
    /// The returned future runs on a newly spawned tokio task, so it can run as long as
    /// the connection lasts.
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // We can get the remote's endpoint id from the connection.
        let endpoint_id = connection.remote_id();
        println!("accepted connection from {endpoint_id}");

        // Our protocol is a simple request-response protocol, so we expect the
        // connecting peer to open a single bi-directional stream.
        let (mut send, mut recv) = connection.accept_bi().await?;

        // Echo any bytes received back directly.
        // This will keep copying until the sender signals the end of data on the stream.
        let bytes_sent = tokio::io::copy(&mut recv, &mut send).await?;
        println!("Copied over {bytes_sent} byte(s)");

        // By calling `finish` on the send stream we signal that we will not send anything
        // further, which makes the receive stream on the other end terminate.
        send.finish()?;

        // Wait until the remote closes the connection, which it does once it
        // received the response.
        connection.closed().await;

        Ok(())
    }
}

async fn peer(alpns: Vec<Vec<u8>>) -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Custom(RelayMap::empty()))
        .alpns(alpns)
        .bind()
        .await
        .expect("bind peer")
}

fn addr(h: &Harness, obj: &DoObject) -> EndpointAddr {
    EndpointAddr::new(obj.id()).with_relay_url(h.relay_url(&obj.id()).parse().expect("relay url"))
}

async fn echo(conn: &Connection, msg: &[u8]) -> Vec<u8> {
    let (mut s, mut r) = conn.open_bi().await.expect("open_bi");
    s.write_all(msg).await.expect("write");
    s.finish().expect("finish");
    r.read_to_end(1 << 20).await.expect("read")
}

/// Waits until the object is Warm with no node tasks left, as a Durable Object must be to
/// hibernate.
async fn warm_and_quiet(obj: &DoObject, max: Duration) {
    let t0 = Instant::now();
    while obj.state() != State::Warm {
        assert!(t0.elapsed() < max, "not Warm: {:?}", obj.state());
        sleep(Duration::from_millis(20)).await;
    }
    if let Some(node) = obj.node() {
        let s = node.stats();
        assert_eq!(s.tasks, 0, "{s:?}");
        assert_eq!(s.teardown_leftover_tasks, Some(0), "{s:?}");
    }
}

/// The host's bind callbacks run outside the node's lock: they can read the node's state,
/// which is Active when the endpoint is bound and no longer Active once it is closed.
#[tokio::test(flavor = "multi_thread")]
async fn bind_callbacks_may_reenter_the_node() {
    let h = Harness::start().await.unwrap();
    let obj = h.mint::<EchoApp>(Timings::default());
    let ep = peer(vec![]).await;
    let conn = timeout(Duration::from_secs(10), ep.connect(addr(&h, &obj), ECHO))
        .await
        .expect("connect timed out (a host callback deadlocked?)")
        .expect("connect");
    assert_eq!(echo(&conn, b"x").await, b"x");
    conn.close(0u32.into(), b"bye!");
    warm_and_quiet(&obj, Duration::from_secs(10)).await;
    assert_eq!(
        obj.bind_callbacks(),
        vec![("built", State::Active), ("closed", State::Warm)]
    );
    ep.close().await;
}

/// iroh's echo handler answers stock peers, the endpoint is dropped once the peer closes, and
/// the object hibernates and serves again with a new endpoint.
#[tokio::test(flavor = "multi_thread")]
async fn echo_example_round_trips_across_hibernation() {
    let h = Harness::start().await.unwrap();
    let obj = h.mint::<EchoApp>(Timings::default());
    let ep = peer(vec![]).await;
    for msg in [&b"hello"[..], &[7u8; 200_000][..]] {
        let conn = ep.connect(addr(&h, &obj), ECHO).await.expect("connect");
        assert_eq!(echo(&conn, msg).await, msg);
        conn.close(0u32.into(), b"bye!");
    }
    warm_and_quiet(&obj, Duration::from_secs(10)).await;
    let st = obj.node().unwrap().status();
    assert_eq!(
        (st["app"].as_str(), st["endpoint_builds"].as_u64()),
        (Some("echo"), Some(1))
    );

    obj.hibernate().expect("hibernatable");
    let conn = ep.connect(addr(&h, &obj), ECHO).await.expect("connect");
    assert_eq!(echo(&conn, b"after").await, b"after");
    conn.close(0u32.into(), b"bye!");
    let st = obj.node().unwrap().status();
    assert_eq!(
        (st["wakes"].as_u64(), st["endpoint_builds"].as_u64()),
        (Some(2), Some(2))
    );
    warm_and_quiet(&obj, Duration::from_secs(10)).await;
    ep.close().await;
}

/// A handler that reports nothing is still closed when idle: iroh's echo handler waits for
/// the peer to close, and a peer that does not is closed after `t_idle` (up to a quarter
/// later, the sampling step), with reason `idle`.
#[tokio::test(flavor = "multi_thread")]
async fn idle_close_without_handler_reports() {
    let h = Harness::start().await.unwrap();
    let t_idle = Duration::from_secs(2);
    let obj = h.mint::<EchoApp>(Timings {
        t_idle,
        ..Timings::default()
    });
    let ep = peer(vec![]).await;
    let conn = ep.connect(addr(&h, &obj), ECHO).await.expect("connect");
    assert_eq!(echo(&conn, b"x").await, b"x");
    let t0 = Instant::now();
    let reason = timeout(Duration::from_secs(10), conn.closed())
        .await
        .expect("closed in time");
    let took = t0.elapsed();
    match &reason {
        ConnectionError::ApplicationClosed(c) => {
            assert_eq!(c.error_code, 0u32.into());
            assert_eq!(&c.reason[..], b"idle");
        }
        other => panic!("unexpected close: {other}"),
    }
    assert!(
        took >= t_idle - Duration::from_millis(200)
            && took < t_idle * 5 / 4 + Duration::from_secs(1),
        "closed after {took:?}"
    );
    assert_eq!(obj.node().unwrap().stats().idle_closes, 1);
    warm_and_quiet(&obj, Duration::from_secs(10)).await;
    ep.close().await;
}

/// ALPN a peer dials to ask for a call back: it writes the delay in milliseconds on a bi
/// stream and reads `ok` back.
const ASK: &[u8] = b"iroh-do-test/ask/0";
/// ALPN the test app dials the peer back with; it writes `back` on a uni stream.
const BACK: &[u8] = b"iroh-do-test/back/0";

/// Schedules a call back for every request, and dials the peer when it is due.
struct CallBack;

#[derive(Debug, Clone)]
struct Ask(Ctx);

impl ProtocolHandler for Ask {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let (mut send, mut recv) = conn.accept_bi().await?;
        let req = recv.read_to_end(64).await.map_err(AcceptError::from_err)?;
        let ms: u64 = String::from_utf8_lossy(&req).trim().parse().unwrap_or(0);
        self.0.schedule(
            &format!("back:{}", conn.remote_id().to_z32()),
            Duration::from_millis(ms),
            "",
        );
        send.write_all(b"ok").await.map_err(AcceptError::from_err)?;
        send.finish()?;
        conn.closed().await;
        Ok(())
    }
}

impl App for CallBack {
    const NAME: &'static str = "callback";

    fn new(_ctx: &Ctx) -> Self {
        CallBack
    }

    fn protocols(&self, ctx: &Ctx, p: &mut Protocols) {
        p.accept(ASK, Ask(ctx.clone()));
    }

    fn on_alarm(&self, ctx: &Ctx, key: &str, _payload: &str) {
        let Some(peer) = key
            .strip_prefix("back:")
            .and_then(|z| iroh::EndpointId::from_z32(z).ok())
        else {
            return;
        };
        let ctx2 = ctx.clone();
        ctx.spawn(async move {
            let Ok(conn) = ctx2.connect(peer, BACK).await else {
                return;
            };
            if let Ok(mut s) = conn.open_uni().await {
                let _ = s.write_all(b"back").await;
                let _ = s.finish();
            }
            conn.closed().await;
        });
    }
}

/// Work scheduled through `Ctx` runs from the alarm on an object that hibernated in between,
/// and `Ctx::connect` binds an endpoint and dials the peer over its relay socket.
#[tokio::test(flavor = "multi_thread")]
async fn scheduled_dial_after_hibernation() {
    let h = Harness::start().await.unwrap();
    let obj = h.mint::<CallBack>(Timings::default());
    let ep = peer(vec![BACK.to_vec()]).await;
    let conn = ep.connect(addr(&h, &obj), ASK).await.expect("connect");
    let (mut s, mut r) = conn.open_bi().await.unwrap();
    s.write_all(b"4000").await.unwrap();
    s.finish().unwrap();
    assert_eq!(r.read_to_end(8).await.unwrap(), b"ok");
    conn.close(0u32.into(), b"bye");
    warm_and_quiet(&obj, Duration::from_secs(4)).await;
    obj.hibernate().expect("hibernatable");

    let incoming = timeout(Duration::from_secs(10), ep.accept())
        .await
        .expect("called back in time")
        .expect("endpoint open");
    let back = incoming.accept().unwrap().await.expect("handshake");
    assert_eq!((back.alpn(), back.remote_id()), (BACK, obj.id()));
    let mut uni = back.accept_uni().await.unwrap();
    assert_eq!(uni.read_to_end(8).await.unwrap(), b"back");
    back.close(0u32.into(), b"thanks");
    assert_eq!(obj.wakes(), 2, "the alarm ran on a new node");
    warm_and_quiet(&obj, Duration::from_secs(10)).await;
    ep.close().await;
}

/// `Ctx::connect` to a peer without a relay socket fails at once and binds no endpoint.
#[tokio::test(flavor = "multi_thread")]
async fn connect_without_socket_fails_without_binding() {
    let h = Harness::start().await.unwrap();
    let obj = h.mint::<CallBack>(Timings::default());
    // The node exists once a socket has authenticated; any peer's will do.
    let ep = peer(vec![]).await;
    let conn = ep.connect(addr(&h, &obj), ASK).await.expect("connect");
    conn.close(0u32.into(), b"bye");
    warm_and_quiet(&obj, Duration::from_secs(10)).await;
    let ctx = obj.node().unwrap().ctx();
    let stranger = iroh::SecretKey::generate().public();
    match ctx.connect(stranger, BACK).await {
        Err(ConnectError::NoSocket) => {}
        other => panic!("expected NoSocket, got {:?}", other.map(|c| c.remote_id())),
    }
    assert_eq!(obj.state(), State::Warm);
    assert_eq!(obj.node().unwrap().stats().endpoint_builds, 1);
    ep.close().await;
}

/// A task spawned with `Ctx::spawn` makes the object Active even with no endpoint bound, so a
/// host does not hibernate it under the task; it returns to Warm when the task ends.
#[tokio::test(flavor = "multi_thread")]
async fn app_task_keeps_object_active() {
    let h = Harness::start().await.unwrap();
    let obj = h.mint::<CallBack>(Timings::default());
    let ep = peer(vec![]).await;
    let conn = ep.connect(addr(&h, &obj), ASK).await.expect("connect");
    conn.close(0u32.into(), b"bye");
    warm_and_quiet(&obj, Duration::from_secs(10)).await;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    obj.node().unwrap().ctx().spawn(async move {
        let _ = rx.await;
    });
    assert_eq!(obj.state(), State::Active);
    assert!(obj.hibernate().is_err(), "hibernated under a running task");
    tx.send(()).unwrap();
    warm_and_quiet(&obj, Duration::from_secs(5)).await;
    assert_eq!(
        obj.node().unwrap().stats().endpoint_builds,
        1,
        "no endpoint was bound"
    );
    ep.close().await;
}
