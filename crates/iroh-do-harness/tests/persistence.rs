//! Host-contract tests: destination-violation counts kept in socket attachments.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use iroh::SecretKey;
use iroh_do_core::{App, Ctx, Protocols};
use iroh_do_harness::{Harness, Timings};
use iroh_relay_ingress::{DEFAULT_DST_VIOLATION_LIMIT, DOMAIN_SEP_CHALLENGE};
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};

/// An app with no protocols: these tests never reach the endpoint.
struct Silent;

impl App for Silent {
    const NAME: &'static str = "silent";

    fn new(_ctx: &Ctx) -> Self {
        Silent
    }

    fn protocols(&self, _ctx: &Ctx, _p: &mut Protocols) {}
}

/// The destination-violation count survives hibernation in the socket attachment, so spreading
/// violations across hibernations does not evade the limit.
#[tokio::test(flavor = "multi_thread")]
async fn dst_violations_survive_hibernation() {
    let h = Harness::start().await.unwrap();
    let obj = h.mint::<Silent>(Timings::default());
    let mut req = format!("ws://127.0.0.1:{}/relay", h.port())
        .into_client_request()
        .unwrap();
    let host = format!("{}.localhost:{}", obj.id().to_z32(), h.port());
    req.headers_mut()
        .insert("host", HeaderValue::from_str(&host).unwrap());
    req.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("iroh-relay-v2"),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();

    // Handshake: ServerChallenge (00 ‖ 16 B) → ClientAuth (01 ‖ pk ‖ 40 ‖ sig) → confirm.
    let challenge = next_binary(&mut ws).await;
    assert_eq!((challenge.len(), challenge[0]), (17, 0x00));
    let client = SecretKey::from_bytes(&[7u8; 32]);
    let msg = blake3::derive_key(DOMAIN_SEP_CHALLENGE, &challenge[1..]);
    let mut auth = vec![0x01];
    auth.extend_from_slice(client.public().as_bytes());
    auth.push(0x40);
    auth.extend_from_slice(&client.sign(&msg).to_bytes());
    ws.send(Message::binary(auth)).await.unwrap();
    next_binary(&mut ws).await; // ServerConfirmsAuth

    // Datagram frame (04 ‖ dst ‖ ecn ‖ payload) for a destination that is not the object.
    let foreign = SecretKey::from_bytes(&[9u8; 32]).public();
    let mut frame = vec![0x04];
    frame.extend_from_slice(foreign.as_bytes());
    frame.extend_from_slice(&[0, b'x']);

    let limit = DEFAULT_DST_VIOLATION_LIMIT as usize;
    for half in [limit / 2, limit - limit / 2] {
        for _ in 0..half {
            ws.send(Message::binary(frame.clone())).await.unwrap();
        }
        sleep(Duration::from_millis(200)).await;
        obj.hibernate().expect("Warm: no endpoint was ever needed");
    }
    // `limit` violations so far, split across two hibernations: still open.
    assert!(
        timeout(Duration::from_millis(300), ws.next())
            .await
            .is_err(),
        "closed before the limit"
    );
    // One more exceeds the limit only if the count survived both hibernations.
    ws.send(Message::binary(frame)).await.unwrap();
    let closed = timeout(Duration::from_secs(5), async {
        while let Some(Ok(m)) = ws.next().await {
            if let Message::Close(c) = m {
                return c;
            }
        }
        None
    })
    .await
    .expect("closed at the limit");
    println!("closed after limit+1 violations across hibernation: {closed:?}");
}

async fn next_binary<S>(ws: &mut S) -> Vec<u8>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(Message::Binary(b)))) => return b.to_vec(),
            Ok(Some(Ok(_))) => continue,
            other => panic!("expected a binary message, got {other:?}"),
        }
    }
}
