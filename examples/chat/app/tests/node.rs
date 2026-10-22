//! End-to-end tests: a stock iroh 1.4.0 peer with default features against an object hosted
//! by the harness.
//!
//! The peer dials `EndpointAddr { id: E, relay: http://<E z32>.localhost:<port>/ }`. Its relay
//! map is empty, so it has no home relay and the tests need only loopback. The object's relay
//! URL is therefore a relay other than the peer's home relay, as in production.

use std::time::Duration;

use iroh::{
    Endpoint, EndpointAddr, RelayMap, RelayMode,
    endpoint::{Connection, ConnectionError, RecvStream, SendStream, ZeroRttStatus, presets},
};
use iroh_do_chat_app::{CTL_ALPN, ChatApp, ChatStats, ECHO_ALPN, NOTIFY_ALPN, store};
use iroh_do_harness::{DoObject, Harness, State, Timings};
use tokio::time::{Instant, sleep, timeout};

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "iroh_do_core=info,iroh_do_harness=info".into()),
        )
        .with_test_writer()
        .try_init();
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

async fn wait_state(obj: &DoObject, want: State, max: Duration) -> Duration {
    let t0 = Instant::now();
    loop {
        if obj.state() == want {
            return t0.elapsed();
        }
        assert!(
            t0.elapsed() < max,
            "state {:?} not reached in {max:?}: {:?}",
            want,
            obj.state()
        );
        sleep(Duration::from_millis(20)).await;
    }
}

/// Asserts the object is Warm with no node tasks (hibernatable).
async fn assert_warm_and_quiet(obj: &DoObject, max: Duration) -> Duration {
    let took = wait_state(obj, State::Warm, max).await;
    if let Some(node) = obj.node() {
        let s = node.stats();
        assert_eq!(s.tasks, 0, "tasks after Active→Warm: {s:?}");
        assert_eq!(s.teardown_leftover_tasks, Some(0), "{s:?}");
    }
    took
}

async fn echo_once(conn: &Connection, msg: &[u8]) -> Vec<u8> {
    let (mut s, mut r) = conn.open_bi().await.expect("open_bi");
    s.write_all(msg).await.expect("write");
    s.finish().expect("finish");
    r.read_to_end(1 << 20).await.expect("read")
}

struct Ctl {
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
    buf: Vec<u8>,
}

impl Ctl {
    async fn open(ep: &Endpoint, addr: EndpointAddr) -> Self {
        let conn = ep.connect(addr, CTL_ALPN).await.expect("connect ctl");
        let (send, recv) = conn.open_bi().await.expect("open control");
        Self {
            conn,
            send,
            recv,
            buf: Vec::new(),
        }
    }

    async fn line(&mut self, req: &str) -> String {
        self.send
            .write_all(format!("{req}\n").as_bytes())
            .await
            .expect("write");
        loop {
            if let Some(p) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=p).collect();
                return String::from_utf8(line[..line.len() - 1].to_vec()).expect("utf8");
            }
            let mut chunk = [0u8; 1024];
            let n = self
                .recv
                .read(&mut chunk)
                .await
                .expect("read")
                .expect("eof");
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    async fn status(&mut self) -> serde_json::Value {
        let s = self.line("/status").await;
        serde_json::from_str(&s).unwrap_or_else(|e| panic!("bad /status {s:?}: {e}"))
    }

    fn close(self) {
        self.conn.close(0u32.into(), b"bye");
    }
}

/// Echo returns small and 200 kB payloads intact, and the object returns to Warm with no
/// tasks once the peer closes.
#[tokio::test(flavor = "multi_thread")]
async fn echo_round_trip() {
    init_tracing();
    let h = Harness::start().await.unwrap();
    let obj = h.mint::<ChatApp>(Timings::default());
    let ep = peer(vec![]).await;
    let t0 = Instant::now();
    let conn = ep
        .connect(addr(&h, &obj), ECHO_ALPN)
        .await
        .expect("connect");
    let connect = t0.elapsed();
    assert_eq!(echo_once(&conn, b"hello ingress").await, b"hello ingress");
    let big: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
    assert_eq!(echo_once(&conn, &big).await, big);
    println!("echo: connect {connect:?}, total {:?}", t0.elapsed());
    assert_eq!(obj.state(), State::Active);
    conn.close(0u32.into(), b"bye");
    let took = assert_warm_and_quiet(&obj, Duration::from_secs(10)).await;
    println!("echo: Active→Warm {took:?} after close");
    ep.close().await;
}

/// ctl replies, the `/status` fields and the unprompted uni stream match the protocol.
#[tokio::test(flavor = "multi_thread")]
async fn ctl_status_and_unprompted_uni() {
    init_tracing();
    let h = Harness::start().await.unwrap();
    let obj = h.mint::<ChatApp>(Timings::default());
    let ep = peer(vec![]).await;
    let mut ctl = Ctl::open(&ep, addr(&h, &obj)).await;
    assert_eq!(
        ctl.line("hi there").await,
        "echo: hi there (#1, total 1, yours 1)"
    );
    // The object opens a uni stream after the first request line.
    let mut uni = timeout(Duration::from_secs(5), ctl.conn.accept_uni())
        .await
        .expect("uni in time")
        .expect("accept_uni");
    let hello = String::from_utf8(uni.read_to_end(1024).await.unwrap()).unwrap();
    assert_eq!(
        hello,
        format!(
            "hello from {}: you are {}\n",
            obj.id().to_z32(),
            ep.id().to_z32()
        )
    );
    let st = ctl.status().await;
    println!("/status: {st}");
    assert_eq!(st["id"], obj.id().to_z32());
    assert_eq!(st["state"], "Active");
    assert_eq!(st["wakes"], 1);
    assert_eq!(st["endpoint_builds"], 1);
    assert_eq!(st["connections"], 1);
    assert_eq!(st["bundle"], iroh_do_harness::BUNDLE[..12]);
    assert!(st["since_wake_ms"].as_u64().is_some());
    let keys: Vec<_> = st.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys.len(), 11, "{keys:?}");
    assert_eq!(st["app"], "chat");
    assert_eq!(
        (st["members"].clone(), st["present"].clone()),
        (0.into(), 0.into())
    );
    assert_eq!(st["records"], 1);
    assert!(ctl.line("/callback 99").await.starts_with("error:"));
    assert_eq!(
        ctl.line("/nope").await,
        "echo: /nope (#2, total 2, yours 2)"
    );
    ctl.close();
    assert_warm_and_quiet(&obj, Duration::from_secs(10)).await;
    ep.close().await;
}

/// Each of three sessions binds a new endpoint and tears it down, with no constructor run.
#[tokio::test(flavor = "multi_thread")]
async fn three_sessions_rebuild_endpoint() {
    init_tracing();
    let h = Harness::start().await.unwrap();
    let obj = h.mint::<ChatApp>(Timings::default());
    let ep = peer(vec![]).await;
    for i in 1..=3u64 {
        let t0 = Instant::now();
        let mut ctl = Ctl::open(&ep, addr(&h, &obj)).await;
        let st = ctl.status().await;
        println!("session {i}: first reply {:?}, status {st}", t0.elapsed());
        assert_eq!(st["endpoint_builds"], i);
        assert_eq!(st["wakes"], 1);
        ctl.close();
        let took = assert_warm_and_quiet(&obj, Duration::from_secs(10)).await;
        println!("session {i}: Active→Warm {took:?} after close");
        let s = obj.node().unwrap().stats();
        assert_eq!(s.endpoint_builds, i);
        assert_eq!(s.endpoint_closes, i);
    }
    ep.close().await;
}

/// An idle connection is closed with reason `idle` after `t_idle`, and the object returns to
/// Warm.
#[tokio::test(flavor = "multi_thread")]
async fn app_idle_close_then_warm() {
    init_tracing();
    let h = Harness::start().await.unwrap();
    let obj = h.mint::<ChatApp>(Timings::default()); // t_idle = 10 s
    let ep = peer(vec![]).await;
    let conn = ep.connect(addr(&h, &obj), ECHO_ALPN).await.unwrap();
    echo_once(&conn, b"x").await;
    let t0 = Instant::now();
    let reason = timeout(Duration::from_secs(20), conn.closed())
        .await
        .expect("idle close");
    let idle = t0.elapsed();
    println!("idle close after {idle:?}: {reason}");
    match reason {
        ConnectionError::ApplicationClosed(c) => {
            assert_eq!(c.error_code, 0u32.into());
            assert_eq!(&c.reason[..], b"idle");
        }
        other => panic!("unexpected close {other:?}"),
    }
    assert!(
        idle >= Duration::from_secs(9) && idle < Duration::from_secs(13),
        "{idle:?}"
    );
    let took = assert_warm_and_quiet(&obj, Duration::from_secs(10)).await;
    println!("idle: Active→Warm {took:?} after idle close");
    assert_eq!(obj.node().unwrap().stats().idle_closes, 1);
    ep.close().await;
}

/// A connection whose open stream carries no bytes is closed with reason `silent` after
/// `t_silent`.
#[tokio::test(flavor = "multi_thread")]
async fn silent_open_stream_is_closed() {
    init_tracing();
    let h = Harness::start().await.unwrap();
    let obj = h.mint::<ChatApp>(Timings {
        t_silent: Duration::from_secs(3),
        ..Timings::default()
    });
    let ep = peer(vec![]).await;
    // ctl holds its control stream open; after one line the peer goes silent.
    let mut ctl = Ctl::open(&ep, addr(&h, &obj)).await;
    ctl.line("then silence").await;
    let t0 = Instant::now();
    let reason = timeout(Duration::from_secs(15), ctl.conn.closed())
        .await
        .expect("silent close");
    let silent = t0.elapsed();
    println!("silent close after {silent:?}: {reason}");
    match reason {
        ConnectionError::ApplicationClosed(c) => assert_eq!(&c.reason[..], b"silent"),
        other => panic!("unexpected close {other:?}"),
    }
    assert!(
        silent >= Duration::from_secs(2) && silent < Duration::from_secs(6),
        "{silent:?}"
    );
    let took = assert_warm_and_quiet(&obj, Duration::from_secs(10)).await;
    println!("silent: Active→Warm {took:?} after close");
    ep.close().await;
}

/// After hibernation the socket survives, the constructor runs again and the endpoint build
/// count carries over.
#[tokio::test(flavor = "multi_thread")]
async fn hibernation_between_sessions() {
    init_tracing();
    let h = Harness::start().await.unwrap();
    let obj = h.mint::<ChatApp>(Timings::default());
    let ep = peer(vec![]).await;
    let mut ctl = Ctl::open(&ep, addr(&h, &obj)).await;
    assert_eq!(ctl.status().await["wakes"], 1);
    ctl.close();
    assert_warm_and_quiet(&obj, Duration::from_secs(10)).await;
    obj.hibernate().expect("hibernatable");
    assert!(obj.node().is_none());
    assert_eq!(obj.sockets(), 1, "socket survives hibernation");
    let t0 = Instant::now();
    let mut ctl = Ctl::open(&ep, addr(&h, &obj)).await;
    let st = ctl.status().await;
    println!(
        "after hibernation: first reply {:?}, status {st}",
        t0.elapsed()
    );
    assert!(st["wakes"].as_u64().unwrap() >= 2);
    assert_eq!(st["endpoint_builds"], 2, "persisted across hibernation");
    assert_eq!(
        obj.node().unwrap().stats().endpoint_builds,
        1,
        "new node, first build"
    );
    assert_eq!(
        ctl.line("still here").await,
        "echo: still here (#1, total 1, yours 1)"
    );
    ctl.close();
    assert_warm_and_quiet(&obj, Duration::from_secs(10)).await;
    ep.close().await;
}

/// Message-log records and totals survive hibernation, and commands are not logged.
#[tokio::test(flavor = "multi_thread")]
async fn message_log_persists_across_hibernation() {
    init_tracing();
    let h = Harness::start().await.unwrap();
    let obj = h.mint::<ChatApp>(Timings::default());
    let ep = peer(vec![]).await;
    let mut ctl = Ctl::open(&ep, addr(&h, &obj)).await;
    assert_eq!(ctl.line("one").await, "echo: one (#1, total 1, yours 1)");
    assert_eq!(ctl.line("two").await, "echo: two (#2, total 2, yours 2)");
    // Commands are not logged.
    assert_eq!(
        ctl.line("/count").await,
        r#"{"total":2,"yours":2,"peers":1}"#
    );
    let st = ctl.status().await;
    assert_eq!(st["records"], 2);
    ctl.close();
    assert_warm_and_quiet(&obj, Duration::from_secs(10)).await;
    obj.hibernate().expect("hibernatable");
    assert_eq!(obj.with_sql(|s| store::log_total(s).unwrap()), 2);

    let mut ctl = Ctl::open(&ep, addr(&h, &obj)).await;
    let st = ctl.status().await;
    assert!(st["wakes"].as_u64().unwrap() >= 2);
    assert_eq!(st["records"], 2, "records survive hibernation");
    assert_eq!(
        ctl.line("three").await,
        "echo: three (#3, total 3, yours 3)"
    );
    let log: serde_json::Value = serde_json::from_str(&ctl.line("/log 2").await).unwrap();
    let texts: Vec<_> = log
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["text"].clone())
        .collect();
    assert_eq!(texts, ["two", "three"]);
    assert_eq!(log[0]["seq"], 2);
    assert_eq!(log[0]["kind"], "ctl");
    assert_eq!(log[0]["peer"], ep.id().to_z32());
    let all: serde_json::Value = serde_json::from_str(&ctl.line("/log").await).unwrap();
    assert_eq!(
        all.as_array().unwrap().len(),
        3,
        "/log before hibernation still listed"
    );
    assert!(ctl.line("/log x").await.starts_with("error:"));
    ctl.close();
    assert_warm_and_quiet(&obj, Duration::from_secs(10)).await;
    ep.close().await;
}

/// A scheduled dial-back reaches the peer over its relay socket after the object went Warm
/// and hibernated.
#[tokio::test(flavor = "multi_thread")]
async fn dial_back_after_warm_and_hibernation() {
    init_tracing();
    let h = Harness::start().await.unwrap();
    let obj = h.mint::<ChatApp>(Timings::default());
    let ep = peer(vec![NOTIFY_ALPN.to_vec()]).await;
    let mut ctl = Ctl::open(&ep, addr(&h, &obj)).await;
    let delay = 5u64;
    assert_eq!(
        ctl.line(&format!("/callback {delay}")).await,
        format!("ok: callback in {delay}s")
    );
    let scheduled = Instant::now();
    ctl.close();
    assert_warm_and_quiet(&obj, Duration::from_secs(4)).await;
    obj.hibernate().expect("hibernatable");
    let warm_at = scheduled.elapsed();
    let incoming = timeout(Duration::from_secs(delay + 10), ep.accept())
        .await
        .expect("dial-back in time")
        .expect("endpoint open");
    let conn = incoming.accept().expect("accept").await.expect("handshake");
    let connected = scheduled.elapsed();
    assert_eq!(conn.alpn(), NOTIFY_ALPN);
    assert_eq!(conn.remote_id(), obj.id());
    let mut uni = conn.accept_uni().await.expect("uni");
    let line = String::from_utf8(uni.read_to_end(1024).await.unwrap()).unwrap();
    let delivered = scheduled.elapsed();
    let wakes = obj.wakes();
    let prefix = format!("callback from {} wakes={wakes} records=", obj.id().to_z32());
    assert!(line.starts_with(&prefix), "{line:?}");
    assert!(
        line[prefix.len()..].trim_end().parse::<u64>().is_ok(),
        "{line:?}"
    );
    conn.close(0u32.into(), b"thanks");
    println!(
        "dial-back: Warm+hibernated at {warm_at:?}, alarm at {delay}s, connected {connected:?}, \
         delivered {delivered:?} (wakes={wakes})"
    );
    assert_warm_and_quiet(&obj, Duration::from_secs(10)).await;
    let s = chat_stats(&obj);
    assert_eq!((s.dial_backs_ok, s.dial_backs_failed), (1, 0));
    ep.close().await;
}

/// After eviction, the peer's old connection fails when its idle timeout expires, and a redial
/// works.
///
/// This is a known gap: iroh 1.4.0 creates the stateless-reset key per endpoint, so the new
/// endpoint drops the old connection's packets instead of resetting it within one RTT.
#[tokio::test(flavor = "multi_thread")]
async fn abrupt_restart_documented() {
    init_tracing();
    let h = Harness::start().await.unwrap();
    let timings = Timings {
        max_idle_timeout: Duration::from_secs(10),
        ..Timings::default()
    };
    let obj = h.mint::<ChatApp>(timings);
    let ep = peer(vec![]).await;
    let mut ctl = Ctl::open(&ep, addr(&h, &obj)).await;
    assert_eq!(
        ctl.line("before").await,
        "echo: before (#1, total 1, yours 1)"
    );
    obj.evict();
    let t0 = Instant::now();
    let _ = ctl.send.write_all(b"after restart\n").await;
    let reason = timeout(Duration::from_secs(40), ctl.conn.closed())
        .await
        .expect("old conn ends");
    let failed = t0.elapsed();
    let mut ctl2 = Ctl::open(&ep, addr(&h, &obj)).await;
    let st = ctl2.status().await;
    let recovered = t0.elapsed();
    println!(
        "abrupt restart: old connection failed after {failed:?} ({reason}); \
         redial answered at {recovered:?}; status {st}; node stats {:?}",
        obj.node().map(|n| n.stats())
    );
    assert!(st["wakes"].as_u64().unwrap() >= 2);
    // Bounded by the negotiated idle timeout (10 s here); a stateless reset would take 1 RTT.
    assert!(
        failed < timings.max_idle_timeout + Duration::from_secs(3),
        "{failed:?}"
    );
    ctl2.close();
    ep.close().await;
}

/// Prints whether 0-RTT is accepted on a fresh connection, within one Active period and after
/// an Active → Warm cycle.
///
/// Expected: accepted within one Active period, rejected after Warm, because iroh 1.4.0
/// creates the TLS session store per endpoint. The outcome is printed, not asserted.
#[tokio::test(flavor = "multi_thread")]
async fn zero_rtt_documented() {
    init_tracing();
    let h = Harness::start().await.unwrap();
    let obj = h.mint::<ChatApp>(Timings::default());
    let ep = peer(vec![]).await;
    let a = addr(&h, &obj);

    async fn session(ep: &Endpoint, a: EndpointAddr) -> &'static str {
        let connecting = ep
            .connect_with_opts(a, ECHO_ALPN, Default::default())
            .await
            .unwrap();
        let (res, conn) = match connecting.into_0rtt() {
            Err(c) => {
                let conn = c.await.unwrap();
                ("1-RTT (no ticket)", conn)
            }
            Ok(zc) => {
                let (mut s, mut r) = zc.open_bi().await.unwrap();
                s.write_all(b"0rtt").await.unwrap();
                s.finish().unwrap();
                match zc.handshake_completed().await.unwrap() {
                    ZeroRttStatus::Accepted(c) => {
                        assert_eq!(r.read_to_end(100).await.unwrap(), b"0rtt");
                        ("0-RTT accepted", c)
                    }
                    ZeroRttStatus::Rejected(c) => ("0-RTT rejected", c),
                }
            }
        };
        assert_eq!(echo_once(&conn, b"1rtt").await, b"1rtt");
        conn.close(0u32.into(), b"bye");
        res
    }

    let r1 = session(&ep, a.clone()).await;
    sleep(Duration::from_millis(300)).await; // within t_linger: same endpoint
    let r2 = session(&ep, a.clone()).await;
    assert_warm_and_quiet(&obj, Duration::from_secs(10)).await;
    let r3 = session(&ep, a.clone()).await;
    println!("0-RTT: #1 fresh: {r1}; #2 same Active period: {r2}; #3 after Warm cycle: {r3}");
    ep.close().await;
}

/// The chat app's counters.
#[allow(dead_code)]
fn chat_stats(obj: &iroh_do_harness::DoObject) -> ChatStats {
    obj.with_app(|a: &ChatApp| a.stats())
        .expect("live chat app")
}
