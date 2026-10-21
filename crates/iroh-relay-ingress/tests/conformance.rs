//! Conformance against the stock iroh-relay 1.4.0 client and a stock iroh
//! 1.4.0 `Endpoint`, through a tokio-tungstenite WebSocket server wrapping
//! `IngressSession`.
//!
//! The client's own encoder produces the frames the session decodes, and its
//! own decoder consumes the frames the session encodes. Raw bytes on the
//! server side are also compared with the layouts in `golden.rs`.
#![cfg(not(target_family = "wasm"))]

mod common;

use std::{
    collections::HashMap,
    num::NonZeroU16,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::{SinkExt, StreamExt};
use iroh_base::{EndpointAddr, RelayUrl, SecretKey};
use iroh_relay::protos::relay::{ClientToRelayMsg, Datagrams, RelayToClientMsg};
use iroh_relay_ingress::{
    DatagramBatch, Ecn, EndpointId, IngressConfig, IngressSession, Output, ProtocolVersion, Status,
    UpgradeRequest, encode_datagrams,
};
use tokio::{net::TcpListener, sync::mpsc, time::timeout};
use tokio_tungstenite::tungstenite::{
    Message,
    handshake::server::{ErrorResponse, Request, Response},
    http::{HeaderValue, StatusCode},
    protocol::{CloseFrame, frame::coding::CloseCode},
};

const T: Duration = Duration::from_secs(10);

#[derive(Debug)]
enum Event {
    Upgraded {
        version: ProtocolVersion,
        client_auth_header: bool,
    },
    Raw(Vec<u8>),
    Authenticated(EndpointId),
    Datagram {
        from: EndpointId,
        ecn: Option<Ecn>,
        segment_size: Option<NonZeroU16>,
        contents: Vec<u8>,
    },
    Closed {
        code: u16,
    },
}

type Peers = Arc<Mutex<HashMap<EndpointId, mpsc::UnboundedSender<Vec<u8>>>>>;

struct Harness {
    local: EndpointId,
    url: RelayUrl,
    events: mpsc::UnboundedReceiver<Event>,
    peers: Peers,
}

fn now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

impl Harness {
    async fn start(local: EndpointId) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // `*.localhost` resolves to loopback without DNS.
        let url: RelayUrl = format!("http://{}.localhost:{port}/", local.to_z32())
            .parse()
            .unwrap();
        let (tx, events) = mpsc::unbounded_channel();
        let peers: Peers = Default::default();
        let cfg = IngressConfig::new(local);
        let peers2 = peers.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve(stream, cfg.clone(), tx.clone(), peers2.clone()));
            }
        });
        Self {
            local,
            url,
            events,
            peers,
        }
    }

    async fn next(&mut self) -> Event {
        timeout(T, self.events.recv())
            .await
            .expect("event timeout")
            .expect("harness alive")
    }

    /// Next event that is not `Raw`.
    async fn next_non_raw(&mut self) -> Event {
        loop {
            match self.next().await {
                Event::Raw(_) => continue,
                e => return e,
            }
        }
    }

    fn send_to(&self, peer: &EndpointId, frame: Vec<u8>) {
        self.peers.lock().unwrap()[peer].send(frame).unwrap();
    }
}

async fn serve(
    stream: tokio::net::TcpStream,
    cfg: IngressConfig,
    tx: mpsc::UnboundedSender<Event>,
    peers: Peers,
) {
    let mut upgraded = None;
    let callback = |req: &Request, mut resp: Response| -> Result<Response, ErrorResponse> {
        let proto = req
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok());
        let ureq = UpgradeRequest {
            method: req.method().as_str(),
            path: req.uri().path(),
            sec_websocket_protocol: proto,
        };
        match IngressSession::on_upgrade(&ureq, &cfg, &mut rand::rng(), now()) {
            Ok((r, session)) => {
                let (name, value) = r.subprotocol_header();
                resp.headers_mut()
                    .insert(name, HeaderValue::from_static(value));
                tx.send(Event::Upgraded {
                    version: r.version,
                    client_auth_header: req.headers().contains_key("x-iroh-relay-client-auth-v1"),
                })
                .ok();
                upgraded = Some((r.challenge, session));
                Ok(resp)
            }
            Err(e) => {
                let mut err = ErrorResponse::new(Some(e.to_string()));
                *err.status_mut() = StatusCode::from_u16(e.http_status()).unwrap();
                Err(err)
            }
        }
    };
    let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
        return;
    };
    let Some((challenge, mut session)) = upgraded else {
        return;
    };
    let (mut sink, mut stream) = ws.split();
    sink.send(Message::binary(challenge.into_vec()))
        .await
        .unwrap();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    loop {
        tokio::select! {
            Some(frame) = out_rx.recv() => {
                if sink.send(Message::binary(frame)).await.is_err() { break; }
            }
            msg = stream.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    _ => break,
                };
                let res = match &msg {
                    Message::Binary(b) => {
                        tx.send(Event::Raw(b.to_vec())).ok();
                        session.on_message(b, now())
                    }
                    Message::Text(_) => Err(session.on_text_message()),
                    Message::Close(_) => break,
                    _ => continue,
                };
                match res {
                    Ok(outs) => {
                        for o in outs {
                            match o {
                                Output::Send(f) => sink.send(Message::binary(f.into_vec())).await.unwrap(),
                                Output::Authenticated { peer } => {
                                    peers.lock().unwrap().insert(peer, out_tx.clone());
                                    tx.send(Event::Authenticated(peer)).ok();
                                }
                                Output::Datagrams { from, batch } => {
                                    tx.send(Event::Datagram {
                                        from,
                                        ecn: batch.ecn,
                                        segment_size: batch.segment_size,
                                        contents: batch.contents.to_vec(),
                                    }).ok();
                                }
                            }
                        }
                    }
                    Err(close) => {
                        if let Some(f) = &close.frame {
                            sink.send(Message::binary(f.as_bytes().to_vec())).await.ok();
                        }
                        sink.send(Message::Close(Some(CloseFrame {
                            code: CloseCode::from(close.code),
                            reason: close.reason_text().into(),
                        }))).await.ok();
                        tx.send(Event::Closed { code: close.code }).ok();
                        break;
                    }
                }
            }
        }
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

async fn stock_client(url: &RelayUrl, secret: SecretKey) -> iroh_relay::client::Client {
    let tls = iroh_relay::tls::CaTlsConfig::default()
        .client_config(iroh_relay::tls::default_provider())
        .unwrap();
    timeout(
        T,
        iroh_relay::client::ClientBuilder::new(
            url.clone(),
            secret,
            iroh_dns::dns::DnsResolver::new(),
        )
        .tls_client_config(tls)
        .connect(),
    )
    .await
    .expect("connect timeout")
    .expect("stock client connects")
}

async fn recv_msg(client: &mut iroh_relay::client::Client) -> RelayToClientMsg {
    timeout(T, client.next())
        .await
        .expect("recv timeout")
        .expect("stream open")
        .expect("stock client decodes our frame")
}

#[tokio::test(flavor = "multi_thread")]
async fn stock_relay_client_handshake_and_datagrams() {
    init_tracing();
    let local_secret = SecretKey::from_bytes(&[1u8; 32]);
    let local = local_secret.public();
    let mut h = Harness::start(local).await;
    let client_secret = common::key42();
    let peer = client_secret.public();

    let mut client = stock_client(&h.url, client_secret).await;

    // v2 is negotiated, and there is no exporter header over plain http.
    match h.next().await {
        Event::Upgraded {
            version,
            client_auth_header,
        } => {
            assert_eq!(version, ProtocolVersion::V2);
            assert!(!client_auth_header);
        }
        e => panic!("{e:?}"),
    }
    // ClientAuth bytes from the stock client: 98 bytes, 01 ‖ pk ‖ 40 ‖ sig.
    match h.next().await {
        Event::Raw(b) => {
            assert_eq!(b.len(), 98);
            assert_eq!(b[0], 0x01);
            assert_eq!(&b[1..33], peer.as_bytes());
            assert_eq!(b[33], 0x40);
        }
        e => panic!("{e:?}"),
    }
    match h.next().await {
        Event::Authenticated(p) => assert_eq!(p, peer),
        e => panic!("{e:?}"),
    }

    // Client-to-server batch from upstream's encoder, compared byte for byte.
    let contents = b"abcdefg";
    client
        .send(ClientToRelayMsg::Datagrams {
            dst_endpoint_id: local,
            datagrams: Datagrams {
                ecn: Some(noq_ecn(Ecn::Ce)),
                segment_size: NonZeroU16::new(3),
                contents: contents.to_vec().into(),
            },
        })
        .await
        .unwrap();
    let raw = loop {
        // The stock client may ping first; skip pings.
        match h.next().await {
            Event::Raw(b) if b[0] == 0x09 => continue,
            Event::Raw(b) => break b,
            Event::Datagram { .. } => panic!("datagram before raw"),
            _ => continue,
        }
    };
    let mut exp = vec![0x05];
    exp.extend_from_slice(local.as_bytes());
    exp.extend_from_slice(&[0x03, 0x00, 0x03]);
    exp.extend_from_slice(contents);
    assert_eq!(raw, exp, "upstream ClientToRelayDatagramBatch layout");
    match h.next_non_raw().await {
        Event::Datagram {
            from,
            ecn,
            segment_size,
            contents: c,
        } => {
            assert_eq!(from, peer);
            assert_eq!(ecn, Some(Ecn::Ce));
            assert_eq!(segment_size, NonZeroU16::new(3));
            assert_eq!(c, contents);
        }
        e => panic!("{e:?}"),
    }

    // Single datagram, no ECN.
    client
        .send(ClientToRelayMsg::Datagrams {
            dst_endpoint_id: local,
            datagrams: Datagrams::from(b"x"),
        })
        .await
        .unwrap();
    match h.next_non_raw().await {
        Event::Datagram {
            ecn,
            segment_size,
            contents,
            ..
        } => {
            assert_eq!(
                (ecn, segment_size, contents.as_slice()),
                (None, None, &b"x"[..])
            );
        }
        e => panic!("{e:?}"),
    }

    // Wrong destination: dropped, nothing delivered; the next good one is.
    client
        .send(ClientToRelayMsg::Datagrams {
            dst_endpoint_id: SecretKey::from_bytes(&[9u8; 32]).public(),
            datagrams: Datagrams::from(b"forward me"),
        })
        .await
        .unwrap();
    client
        .send(ClientToRelayMsg::Datagrams {
            dst_endpoint_id: local,
            datagrams: Datagrams::from(b"after"),
        })
        .await
        .unwrap();
    match h.next_non_raw().await {
        Event::Datagram { contents, .. } => assert_eq!(contents, b"after"),
        e => panic!("{e:?}"),
    }

    // Ping → Pong, decoded by the stock client.
    client
        .send(ClientToRelayMsg::Ping([1, 2, 3, 4, 5, 6, 7, 8]))
        .await
        .unwrap();
    loop {
        match recv_msg(&mut client).await {
            RelayToClientMsg::Pong([1, 2, 3, 4, 5, 6, 7, 8]) => break,
            RelayToClientMsg::Pong(_) => continue,
            m => panic!("{m:?}"),
        }
    }

    // Server → client datagrams (batch + single), decoded by upstream.
    for (seg, ecn) in [(NonZeroU16::new(3), Some(Ecn::Ect0)), (None, None)] {
        let batch = DatagramBatch {
            ecn,
            segment_size: seg,
            contents: b"hello, peer",
        };
        h.send_to(
            &peer,
            encode_datagrams(&h.local, &batch).unwrap().into_vec(),
        );
        match recv_msg(&mut client).await {
            RelayToClientMsg::Datagrams {
                remote_endpoint_id,
                datagrams,
            } => {
                assert_eq!(remote_endpoint_id, local);
                assert_eq!(datagrams.ecn, ecn.map(noq_ecn));
                assert_eq!(datagrams.segment_size, seg);
                assert_eq!(&datagrams.contents[..], b"hello, peer");
            }
            m => panic!("{m:?}"),
        }
    }

    // Version-correct status (v2 → Status).
    let s = IngressSession::restore_for_test(local, peer);
    h.send_to(
        &peer,
        s.status_frame(Status::SameEndpointIdConnected)
            .unwrap()
            .into_vec(),
    );
    match recv_msg(&mut client).await {
        RelayToClientMsg::Status(st) => assert_eq!(
            st,
            iroh_relay::protos::relay::Status::SameEndpointIdConnected
        ),
        m => panic!("{m:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_non_relay_path() {
    let local = SecretKey::from_bytes(&[1u8; 32]).public();
    let h = Harness::start(local).await;
    // The stock client always dials /relay, so use a raw WebSocket client.
    let port = h.url.port().unwrap();
    let mut req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        format!("ws://127.0.0.1:{port}/other"),
    )
    .unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("iroh-relay-v2"),
    );
    let err = tokio_tungstenite::connect_async(req).await.unwrap_err();
    assert!(err.to_string().contains("404"), "{err}");

    let mut req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        format!("ws://127.0.0.1:{port}/relay"),
    )
    .unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("iroh-relay-v9"),
    );
    let err = tokio_tungstenite::connect_async(req).await.unwrap_err();
    assert!(err.to_string().contains("400"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_client_misbehaviour_closes() {
    let local = SecretKey::from_bytes(&[1u8; 32]).public();
    let mut h = Harness::start(local).await;
    let port = h.url.port().unwrap();
    let mut req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        format!("ws://127.0.0.1:{port}/relay"),
    )
    .unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("iroh-relay-v1"),
    );
    let (mut ws, resp) = tokio_tungstenite::connect_async(req).await.unwrap();
    assert_eq!(resp.headers()["sec-websocket-protocol"], "iroh-relay-v1");
    let challenge = match ws.next().await.unwrap().unwrap() {
        Message::Binary(b) => b,
        m => panic!("{m:?}"),
    };
    assert_eq!(challenge.len(), 17);
    // Bad signature: ServerDeniesAuth then close 1008.
    let mut auth = common::client_auth(&common::key42(), &[0u8; 16]);
    auth[50] ^= 0xff;
    ws.send(Message::binary(auth)).await.unwrap();
    match ws.next().await.unwrap().unwrap() {
        Message::Binary(b) => assert_eq!(&b[..2], &[0x03, 17]),
        m => panic!("{m:?}"),
    }
    match ws.next().await.unwrap().unwrap() {
        Message::Close(Some(cf)) => assert_eq!(u16::from(cf.code), 1008),
        m => panic!("{m:?}"),
    }
    loop {
        if let Event::Closed { code } = h.next().await {
            assert_eq!(code, 1008);
            break;
        }
    }
}

/// A stock `iroh::Endpoint` dials the local endpoint, whose relay URL points at
/// the harness. The dialer's home relay is unreachable, so the harness is a
/// relay it is not homed on. The QUIC Initial must arrive as `Datagrams` from
/// the dialer.
#[tokio::test(flavor = "multi_thread")]
async fn stock_endpoint_initial_arrives() {
    init_tracing();
    let local = SecretKey::from_bytes(&[1u8; 32]).public();
    let mut h = Harness::start(local).await;

    let bogus_home: RelayUrl = "http://127.0.0.1:9/".parse().unwrap();
    let ep = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(iroh::RelayMode::Custom(bogus_home.into()))
        .bind()
        .await
        .unwrap();
    let client_id = ep.id();
    let addr = EndpointAddr::new(local).with_relay_url(h.url.clone());
    let ep2 = ep.clone();
    let dial = tokio::spawn(async move { ep2.connect(addr, b"iroh-do/echo/1").await });

    let mut authed = false;
    let mut initials = 0;
    while initials == 0 {
        match h.next_non_raw().await {
            Event::Upgraded { version, .. } => assert_eq!(version, ProtocolVersion::V2),
            Event::Authenticated(p) => {
                assert_eq!(p, client_id);
                authed = true;
            }
            Event::Datagram {
                from,
                segment_size,
                contents,
                ..
            } => {
                assert!(authed);
                assert_eq!(from, client_id);
                // A QUIC long-header packet (form bit + fixed bit), padded to ≥ 1200.
                let first = DatagramBatch {
                    ecn: None,
                    segment_size,
                    contents: &contents,
                }
                .segments()
                .next()
                .unwrap()
                .to_vec();
                assert_eq!(first[0] & 0xc0, 0xc0, "long header");
                assert!(first.len() >= 1200, "Initial padded: {}", first.len());
                initials += 1;
            }
            Event::Closed { code } => panic!("closed {code}"),
            Event::Raw(_) => unreachable!(),
        }
    }
    dial.abort();
    ep.close().await;
}

fn noq_ecn(e: Ecn) -> noq_proto::EcnCodepoint {
    noq_proto::EcnCodepoint::from_bits(e as u8).unwrap()
}

/// Test-only helper: an authenticated V2 session for encoding status frames.
trait RestoreForTest {
    fn restore_for_test(local: EndpointId, peer: EndpointId) -> IngressSession;
}

impl RestoreForTest for IngressSession {
    fn restore_for_test(local: EndpointId, peer: EndpointId) -> IngressSession {
        let snap: iroh_relay_ingress::SessionSnapshot = serde_json::from_value(serde_json::json!({
            "v": "v2", "s": {"auth": {"p": peer.to_string()}}, "dv": 0
        }))
        .unwrap();
        assert_eq!(snap.peer(), Some(peer));
        IngressSession::restore(snap, &IngressConfig::new(local))
    }
}
