//! The chat room, `iroh-do/room/1`, with stock iroh 1.4.0 members whose home relay is the
//! room, as the chat client configures it (design note 008).
//!
//! Each member is an endpoint with `RelayMode::Custom` holding only the room's relay URL, so
//! it connects to the room's ingress eagerly, keeps that socket open and probes it like any
//! home relay (`/ping` over HTTP, QUIC address discovery on the default port, which fails
//! here). A member opens a new connection for each request and closes it after the reply, so
//! deliveries reach it on connections the object dials unless one is still open.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use iroh::{
    Endpoint, EndpointAddr, RelayMap, RelayMode, RelayUrl,
    endpoint::{Connection, presets},
};
use iroh_do_core::{CTL_ALPN, ROOM_ALPN};
use iroh_do_harness::{DoObject, Harness, State, Timings};
use serde_json::Value;
use tokio::{
    sync::mpsc,
    time::{Instant, sleep, timeout},
};

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "iroh_do_core=info,iroh_do_harness=info".into()),
        )
        .with_test_writer()
        .try_init();
}

/// A room member running stock iroh with the room as its only (home) relay.
struct Member {
    ep: Endpoint,
    room: EndpointAddr,
    events: mpsc::UnboundedReceiver<Value>,
    tx: mpsc::UnboundedSender<Value>,
    /// Connections the room dialed to this member.
    incoming: Arc<Mutex<u32>>,
}

impl Member {
    async fn join_room(h: &Harness, obj: &DoObject) -> Self {
        Self::with_key(h, obj, iroh::SecretKey::generate()).await
    }

    async fn with_key(h: &Harness, obj: &DoObject, key: iroh::SecretKey) -> Self {
        let url: RelayUrl = h.relay_url(&obj.id()).parse().expect("relay url");
        let ep = Endpoint::builder(presets::Minimal)
            .secret_key(key)
            .relay_mode(RelayMode::Custom(RelayMap::from(url.clone())))
            .alpns(vec![ROOM_ALPN.to_vec()])
            .bind()
            .await
            .expect("bind member");
        // The client connects to its home relay before it has anything to send.
        timeout(Duration::from_secs(15), ep.online())
            .await
            .expect("home relay connected");
        let (tx, events) = mpsc::unbounded_channel();
        let incoming = Arc::new(Mutex::new(0));
        let (ep2, tx2, inc2) = (ep.clone(), tx.clone(), incoming.clone());
        tokio::spawn(async move {
            while let Some(inc) = ep2.accept().await {
                let Ok(accepting) = inc.accept() else {
                    continue;
                };
                let Ok(conn) = accepting.await else { continue };
                assert_eq!(conn.alpn(), ROOM_ALPN);
                *inc2.lock().unwrap() += 1;
                tokio::spawn(read_deliveries(conn, tx2.clone()));
            }
        });
        Self {
            room: EndpointAddr::new(obj.id()).with_relay_url(url),
            ep,
            events,
            tx,
            incoming,
        }
    }

    /// Sends one request on a new connection, returns the reply lines, and closes the
    /// connection.
    async fn request(&self, line: &str) -> Vec<Value> {
        let conn = self
            .ep
            .connect(self.room.clone(), ROOM_ALPN)
            .await
            .expect("connect");
        let reply = self.request_on(&conn, line).await;
        conn.close(0u32.into(), b"done");
        reply
    }

    async fn request_on(&self, conn: &Connection, line: &str) -> Vec<Value> {
        tokio::spawn(read_deliveries(conn.clone(), self.tx.clone()));
        let (mut send, mut recv) = conn.open_bi().await.expect("open_bi");
        send.write_all(format!("{line}\n").as_bytes())
            .await
            .expect("write");
        send.finish().expect("finish");
        let bytes = timeout(Duration::from_secs(10), recv.read_to_end(1 << 20))
            .await
            .expect("reply in time")
            .expect("read reply");
        String::from_utf8(bytes)
            .expect("utf8")
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad line {l:?}: {e}")))
            .collect()
    }

    async fn one(&self, line: &str) -> Value {
        let mut r = self.request(line).await;
        assert_eq!(r.len(), 1, "{line}: {r:?}");
        r.remove(0)
    }

    async fn next_event(&mut self, within: Duration) -> Value {
        timeout(within, self.events.recv())
            .await
            .expect("delivery in time")
            .expect("channel open")
    }

    async fn no_event(&mut self, within: Duration) {
        if let Ok(Some(ev)) = timeout(within, self.events.recv()).await {
            panic!("unexpected delivery {ev}");
        }
    }

    fn incoming(&self) -> u32 {
        *self.incoming.lock().unwrap()
    }
}

/// Reads every uni stream of `conn` as JSON lines.
async fn read_deliveries(conn: Connection, tx: mpsc::UnboundedSender<Value>) {
    while let Ok(mut uni) = conn.accept_uni().await {
        let Ok(bytes) = uni.read_to_end(1 << 20).await else {
            continue;
        };
        for l in String::from_utf8_lossy(&bytes).lines() {
            let _ = tx.send(serde_json::from_str(l).expect("delivery json"));
        }
    }
}

fn ok_seq(v: &Value) -> u64 {
    assert_eq!(v["t"], "ok", "{v}");
    v["seq"].as_u64().unwrap()
}

/// Joins as `nick`; returns the `ready` line after checking the reply ends with it.
async fn join(m: &Member, nick: &str) -> Value {
    let r = m.request(&format!("/join {nick}")).await;
    let ready = r.last().unwrap().clone();
    assert_eq!(ready["t"], "ready", "{r:?}");
    assert_eq!(ready["you"]["nick"], nick);
    assert_eq!(ready["you"]["peer"], m.ep.id().to_z32());
    assert!(r[..r.len() - 1].iter().all(|e| e["hist"] == true), "{r:?}");
    ready
}

async fn wait_state(obj: &DoObject, want: State, max: Duration) {
    let t0 = Instant::now();
    while obj.state() != want {
        assert!(
            t0.elapsed() < max,
            "{want:?} not reached: {:?}",
            obj.state()
        );
        sleep(Duration::from_millis(20)).await;
    }
}

async fn assert_warm_and_quiet(obj: &DoObject) {
    wait_state(obj, State::Warm, Duration::from_secs(20)).await;
    let s = obj.node().unwrap().stats();
    assert_eq!(s.tasks, 0, "{s:?}");
    assert_eq!(s.teardown_leftover_tasks, Some(0), "{s:?}");
}

fn no_dst_violations(obj: &DoObject) {
    let v = obj.dst_violations();
    assert!(v.iter().all(|&n| n == 0), "destination violations: {v:?}");
}

async fn ctl_status(obj: &DoObject, h: &Harness) -> Value {
    serde_json::from_str(&ctl_line(obj, h, "/status").await).unwrap()
}

/// Sends one line on `iroh-do/ctl/1` from a fresh endpoint and returns the reply line.
async fn ctl_line(obj: &DoObject, h: &Harness, request: &str) -> String {
    let ep = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Custom(RelayMap::empty()))
        .bind()
        .await
        .unwrap();
    let addr = EndpointAddr::new(obj.id()).with_relay_url(h.relay_url(&obj.id()).parse().unwrap());
    let conn = ep.connect(addr, CTL_ALPN).await.unwrap();
    let (mut s, mut r) = conn.open_bi().await.unwrap();
    s.write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    s.finish().unwrap();
    let mut line = Vec::new();
    let mut buf = [0u8; 1024];
    while !line.contains(&b'\n') {
        let n = r.read(&mut buf).await.unwrap().unwrap();
        line.extend_from_slice(&buf[..n]);
    }
    conn.close(0u32.into(), b"bye");
    ep.close().await;
    let end = line.iter().position(|&b| b == b'\n').unwrap();
    String::from_utf8(line[..end].to_vec()).unwrap()
}

/// Three members join and talk. Each message reaches the other two, on connections the room
/// dials over their home-relay sockets or reuses, and not its author. The object returns to
/// Warm with no tasks afterwards, and no member's socket carries traffic for another
/// destination.
#[tokio::test(flavor = "multi_thread")]
async fn members_exchange_messages() {
    init_tracing();
    let h = Harness::start().await.unwrap();
    let obj = h.mint(Timings::default());
    let mut ana = Member::join_room(&h, &obj).await;
    let mut bo = Member::join_room(&h, &obj).await;
    let mut cy = Member::join_room(&h, &obj).await;
    assert_eq!(obj.sockets(), 3, "eager home-relay sockets");
    assert_eq!(obj.state(), State::Warm);

    join(&ana, "ana").await;
    // A member's own events are not delivered to it.
    ana.no_event(Duration::from_millis(300)).await;
    join(&bo, "bo").await;
    let ev = ana.next_event(Duration::from_secs(5)).await;
    assert_eq!(
        (ev["t"].as_str(), ev["nick"].as_str()),
        (Some("join"), Some("bo")),
        "{ev}"
    );
    assert!(ev.get("hist").is_none());
    join(&cy, "cy").await;
    for m in [&mut ana, &mut bo] {
        let ev = m.next_event(Duration::from_secs(5)).await;
        assert_eq!(ev["nick"], "cy");
    }

    let t0 = Instant::now();
    let seq = ok_seq(&ana.one("/say hello room").await);
    for m in [&mut bo, &mut cy] {
        let ev = m.next_event(Duration::from_secs(5)).await;
        assert_eq!(ev["t"], "msg");
        assert_eq!(ev["seq"], seq);
        assert_eq!(ev["nick"], "ana");
        assert_eq!(ev["peer"], ana.ep.id().to_z32());
        assert_eq!(ev["text"], "hello room");
        assert!(ev["ts"].as_u64().unwrap() > 1_600_000_000_000);
    }
    println!("say → delivered to 2 members in {:?}", t0.elapsed());
    ok_seq(&bo.one("/me waves").await);
    let ev = ana.next_event(Duration::from_secs(5)).await;
    assert_eq!(
        (ev["t"].as_str(), ev["text"].as_str()),
        (Some("act"), Some("waves"))
    );
    assert_eq!(cy.next_event(Duration::from_secs(5)).await["t"], "act");
    ana.no_event(Duration::from_millis(300)).await;

    let who = bo.one("/who").await;
    assert_eq!(who["t"], "who");
    let nicks: Vec<_> = who["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["nick"].as_str().unwrap().to_string(),
                m["present"].clone(),
            )
        })
        .collect();
    assert_eq!(
        nicks,
        [
            ("ana".to_string(), Value::Bool(true)),
            ("bo".to_string(), Value::Bool(true)),
            ("cy".to_string(), Value::Bool(true))
        ]
    );
    let st = ctl_status(&obj, &h).await;
    assert_eq!(
        (st["members"].as_u64(), st["present"].as_u64()),
        (Some(3), Some(3)),
        "{st}"
    );

    let s = obj.node().unwrap().stats();
    println!(
        "stats after exchange: {s:?}; incoming dials ana={} bo={} cy={}",
        ana.incoming(),
        bo.incoming(),
        cy.incoming()
    );
    assert!(s.delivery_dials >= 2, "{s:?}");
    assert!(ana.incoming() + bo.incoming() + cy.incoming() >= 2);
    assert_eq!(s.deliveries_failed, 0, "{s:?}");
    assert_warm_and_quiet(&obj).await;
    assert_eq!(obj.sockets(), 3, "members stay present while Warm");
    no_dst_violations(&obj);
    for m in [ana, bo, cy] {
        m.ep.close().await;
    }
}

/// A message sent while the object is hibernated wakes it, and the delivery reaches the other
/// member on a connection the new node dials.
#[tokio::test(flavor = "multi_thread")]
async fn delivery_after_hibernation() {
    init_tracing();
    let h = Harness::start().await.unwrap();
    let obj = h.mint(Timings::default());
    let ana = Member::join_room(&h, &obj).await;
    let mut bo = Member::join_room(&h, &obj).await;
    join(&ana, "ana").await;
    join(&bo, "bo").await;
    assert_warm_and_quiet(&obj).await;
    obj.hibernate().expect("hibernatable");
    assert!(obj.node().is_none());
    let wakes = obj.wakes();

    let t0 = Instant::now();
    let seq = ok_seq(&ana.one("/say after hibernation").await);
    let ev = bo.next_event(Duration::from_secs(10)).await;
    println!("hibernated object: say → delivery in {:?}", t0.elapsed());
    assert_eq!(
        (ev["seq"].as_u64(), ev["text"].as_str()),
        (Some(seq), Some("after hibernation"))
    );
    assert_eq!(obj.wakes(), wakes + 1);
    assert_eq!(obj.node().unwrap().stats().delivery_dials, 1);
    assert_warm_and_quiet(&obj).await;
    ana.ep.close().await;
    bo.ep.close().await;
}

/// A member whose client exits is not present and gets no deliveries. When it comes back
/// with the same key it is still joined, and `/since` returns what it missed.
#[tokio::test(flavor = "multi_thread")]
async fn absent_member_catches_up_with_since() {
    init_tracing();
    let h = Harness::start().await.unwrap();
    let obj = h.mint(Timings::default());
    let ana = Member::join_room(&h, &obj).await;
    let bo_key = iroh::SecretKey::generate();
    let bo = Member::with_key(&h, &obj, bo_key.clone()).await;
    join(&ana, "ana").await;
    let last = join(&bo, "bo").await["last_seq"].as_u64().unwrap();
    bo.ep.close().await;
    drop(bo);
    let t0 = Instant::now();
    while obj.sockets() != 1 {
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "bo's socket not closed"
        );
        sleep(Duration::from_millis(20)).await;
    }
    let who = ana.one("/who").await;
    let bo_entry = who["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["nick"] == "bo")
        .cloned()
        .unwrap();
    assert_eq!(bo_entry["present"], false);
    let st = ctl_status(&obj, &h).await;
    assert_eq!(
        (st["members"].as_u64(), st["present"].as_u64()),
        (Some(2), Some(1)),
        "{st}"
    );

    let before = obj.node().unwrap().stats();
    let mut seqs = Vec::new();
    for i in 0..3 {
        seqs.push(ok_seq(&ana.one(&format!("/say missed {i}")).await));
    }
    sleep(Duration::from_millis(200)).await;
    let s = obj.node().unwrap().stats();
    let attempts =
        |s: &iroh_do_harness::Stats| s.deliveries_ok + s.deliveries_failed + s.delivery_dials;
    assert_eq!(
        attempts(&s),
        attempts(&before),
        "no delivery to an absent member: {s:?}"
    );

    let bo = Member::with_key(&h, &obj, bo_key).await;
    let r = bo.request("/join").await;
    assert_eq!(
        r.last().unwrap()["you"]["nick"],
        "bo",
        "still joined, nick kept"
    );
    let caught: Vec<_> = bo.request(&format!("/since {last}")).await;
    let got: Vec<_> = caught.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(got, seqs);
    assert!(caught.iter().all(|e| e["hist"] == true && e["t"] == "msg"));
    // No new join event: bo never left.
    let hist = bo.request("/history 2").await;
    assert_eq!(hist.len(), 2);
    assert_eq!(hist[1]["text"], "missed 2");
    ana.ep.close().await;
    bo.ep.close().await;
}

/// Nicks are 1–24 characters of `[A-Za-z0-9_.-]`, unique among joined members ignoring case;
/// renames are delivered; `/quit` frees the nick and stops deliveries; requests before
/// `/join` are refused.
#[tokio::test(flavor = "multi_thread")]
async fn nick_rules() {
    init_tracing();
    let h = Harness::start().await.unwrap();
    let obj = h.mint(Timings::default());
    let mut ana = Member::join_room(&h, &obj).await;
    let mut bo = Member::join_room(&h, &obj).await;

    assert_eq!(ana.one("/say hi").await["t"], "err", "not joined");
    for bad in ["bad nick", "x!", &"a".repeat(25), "ünï"] {
        let r = ana.one(&format!("/join {bad}")).await;
        assert_eq!(r["t"], "err", "{bad}: {r}");
    }
    join(&ana, &"a".repeat(24)).await;
    let r = ana.one("/nick Ana.B_1-x").await;
    ok_seq(&r);
    assert_eq!(
        bo.one("/join ana.b_1-X").await["t"],
        "err",
        "taken, ignoring case"
    );
    // Without a nick, a member gets a prefix of its EndpointId.
    let ready = bo.request("/join").await.pop().unwrap();
    assert_eq!(ready["you"]["nick"], bo.ep.id().to_z32()[..8]);
    let ev = ana.next_event(Duration::from_secs(5)).await;
    assert_eq!(ev["t"], "join");
    assert_eq!(bo.one("/nick ANA.B_1-X").await["t"], "err");
    assert_eq!(bo.one("/nick bo").await["t"], "ok");
    let ev = ana.next_event(Duration::from_secs(5)).await;
    assert_eq!(
        (ev["t"].as_str(), ev["new"].as_str()),
        (Some("nick"), Some("bo")),
        "{ev}"
    );
    assert_eq!(ev["old"], bo.ep.id().to_z32()[..8]);
    assert_eq!(bo.one("/nick bo").await["t"], "err", "unchanged");
    assert_eq!(bo.one("/frobnicate").await["t"], "err");
    assert_eq!(bo.one("/say").await["t"], "err", "empty text");

    ok_seq(&ana.one("/quit gone fishing").await);
    let ev = bo.next_event(Duration::from_secs(5)).await;
    assert_eq!(
        (ev["t"].as_str(), ev["reason"].as_str()),
        (Some("part"), Some("gone fishing"))
    );
    assert_eq!(ana.one("/say still here?").await["t"], "err", "parted");
    ok_seq(&bo.one("/say anyone?").await);
    ana.no_event(Duration::from_millis(500)).await;
    // The parted member's nick is free.
    ok_seq(&bo.one("/nick Ana.B_1-x").await);
    ana.ep.close().await;
    bo.ep.close().await;
}

/// The eleventh request within 10 s is refused.
#[tokio::test(flavor = "multi_thread")]
async fn rate_limit() {
    init_tracing();
    let h = Harness::start().await.unwrap();
    let obj = h.mint(Timings::default());
    let ana = Member::join_room(&h, &obj).await;
    let conn = ana.ep.connect(ana.room.clone(), ROOM_ALPN).await.unwrap();
    let r = ana.request_on(&conn, "/join ana").await;
    assert_eq!(r.last().unwrap()["t"], "ready");
    for i in 1..10 {
        let r = ana.request_on(&conn, &format!("/say {i}")).await;
        assert_eq!(r[0]["t"], "ok", "request {i}");
    }
    let r = ana.request_on(&conn, "/say 10").await;
    assert_eq!(r[0]["t"], "err", "{r:?}");
    assert!(r[0]["text"].as_str().unwrap().starts_with("rate limit"));
    assert_eq!(obj.node().unwrap().stats().rate_limited, 1);
    conn.close(0u32.into(), b"done");
    ana.ep.close().await;
}

/// A member that has sent nothing for more than 60 s is still reachable: its home-relay
/// socket to the room stays open (a non-home relay socket would have been closed after 60 s),
/// and only probe traffic and relay pings crossed it.
#[tokio::test(flavor = "multi_thread")]
async fn quiet_member_reachable_after_60s() {
    init_tracing();
    let h = Harness::start().await.unwrap();
    let obj = h.mint(Timings::default());
    let ana = Member::join_room(&h, &obj).await;
    let mut bo = Member::join_room(&h, &obj).await;
    join(&ana, "ana").await;
    join(&bo, "bo").await;
    assert_warm_and_quiet(&obj).await;
    let quiet = Duration::from_secs(70);
    println!("waiting {quiet:?} with both members silent");
    let t0 = Instant::now();
    while t0.elapsed() < quiet {
        assert_eq!(obj.sockets(), 2, "socket closed after {:?}", t0.elapsed());
        assert_ne!(
            obj.state(),
            State::Active,
            "silent members activated the object"
        );
        sleep(Duration::from_secs(1)).await;
    }
    let seq = ok_seq(&ana.one("/say still there?").await);
    let ev = bo.next_event(Duration::from_secs(10)).await;
    assert_eq!(ev["seq"], seq);
    no_dst_violations(&obj);
    assert_warm_and_quiet(&obj).await;
    ana.ep.close().await;
    bo.ep.close().await;
}

/// Each room event names the room event before it in `prev`, skipping diagnostic records, so
/// a member detects a missed event exactly: a `ctl` request between two messages leaves the
/// chain unbroken although `seq` skips, and a message sent while the member was absent shows
/// up as `prev` beyond the last event it saw, which `/since` then returns alone.
#[tokio::test(flavor = "multi_thread")]
async fn prev_detects_missed_events_exactly() {
    init_tracing();
    let h = Harness::start().await.unwrap();
    let obj = h.mint(Timings::default());
    let ana = Member::join_room(&h, &obj).await;
    let bo_key = iroh::SecretKey::generate();
    let mut bo = Member::with_key(&h, &obj, bo_key.clone()).await;
    join(&ana, "ana").await;
    join(&bo, "bo").await;

    let one = ok_seq(&ana.one("/say one").await);
    let ev = bo.next_event(Duration::from_secs(10)).await;
    assert_eq!(
        (ev["seq"].as_u64(), ev["text"].as_str()),
        (Some(one), Some("one"))
    );
    let last_seen = one;

    // A diagnostic record lands between the two messages.
    let echo = ctl_line(&obj, &h, "between").await;
    assert!(echo.starts_with("echo: between"), "{echo}");
    let two = ok_seq(&ana.one("/say two").await);
    let ev = bo.next_event(Duration::from_secs(10)).await;
    assert_eq!(ev["seq"].as_u64(), Some(two));
    assert!(
        two > last_seen + 1,
        "seq skipped the ctl record: {two} vs {last_seen}"
    );
    assert_eq!(ev["prev"].as_u64(), Some(last_seen), "no gap: {ev}");
    let last_seen = two;

    // bo leaves the room's reach; ana says something bo does not receive.
    bo.ep.close().await;
    drop(bo);
    let t0 = Instant::now();
    while obj.sockets() != 1 {
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "bo's socket not closed"
        );
        sleep(Duration::from_millis(20)).await;
    }
    let missed = ok_seq(&ana.one("/say three").await);

    // bo returns. The next delivery's `prev` is the missed event, not the last one bo saw.
    bo = Member::with_key(&h, &obj, bo_key).await;
    let four = ok_seq(&ana.one("/say four").await);
    let ev = bo.next_event(Duration::from_secs(10)).await;
    assert_eq!(ev["seq"].as_u64(), Some(four));
    let prev = ev["prev"].as_u64().unwrap();
    assert_eq!(prev, missed);
    assert!(prev > last_seen, "missed event detected");
    let caught = bo.request(&format!("/since {last_seen}")).await;
    let seqs: Vec<_> = caught.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, [missed, four]);
    assert_eq!(caught[0]["prev"].as_u64(), Some(last_seen));
    // `ok` carries `prev` too, so an author detects events it missed before its own.
    let five = bo.one("/say five").await;
    assert_eq!(five["prev"].as_u64(), Some(four));
    ana.ep.close().await;
    bo.ep.close().await;
}
