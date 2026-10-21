//! Byte-level golden tests.
//!
//! Datagram vectors are upstream's snapshot tests in iroh-relay 1.4.0,
//! `src/protos/relay.rs`. Handshake vectors use key `[42; 32]` and challenge
//! `[0x11; 16]`. `conformance.rs` checks the same frames against the stock
//! client.

mod common;

use std::num::NonZeroU16;

use common::*;
use iroh_relay_ingress::*;

const PK42: &str = "197f6b23e16c8532c6abc838facd5ea789be0c76b2920334039bfa8b3d368d61";
const HELLO: &[u8] = b"Hello World!";

#[test]
fn key42_matches_doc() {
    let pk = key42().public();
    assert_eq!(hex::encode(pk.as_bytes()), PK42);
    assert_eq!(
        pk.to_z32(),
        "df9sse9bp1nuftim3yhxiuk6w6r5hddsskjygpydux7esxjstioo"
    );
}

#[test]
fn handshake_vectors() {
    let pk = key42().public();
    let cfg = IngressConfig::new(pk);
    let (resp, mut s) = IngressSession::on_upgrade(
        &req(Some("iroh-relay-v2, iroh-relay-v1")),
        &cfg,
        &mut FixedRng(0x11),
        T0,
    )
    .unwrap();
    // ServerChallenge = 00 ‖ challenge[16]
    assert_eq!(
        resp.challenge.as_bytes(),
        hexd("00 11111111111111111111111111111111")
    );
    assert_eq!(resp.version, ProtocolVersion::V2);
    assert_eq!(
        resp.subprotocol_header(),
        ("Sec-WebSocket-Protocol", "iroh-relay-v2")
    );

    // derive_key and signature
    let msg = blake3::derive_key(DOMAIN_SEP_CHALLENGE, &[0x11; 16]);
    assert_eq!(
        hex::encode(msg),
        "d9f7b5a31b89580cdc27cb4f323ae7dcda22f834b9f35451cb9217c9e0be8601"
    );
    let auth = client_auth(&key42(), &[0x11; 16]);
    let expected = hexd(
        "01 197f6b23e16c8532c6abc838facd5ea789be0c76b2920334039bfa8b3d368d61
         40 8962427ee2d717ba0a250113d637b7c1d84baaec8aab6353bb93de2e3950a14d
            82b6d4ea97e1f88530e8b9b7addee9427cdd45686b8132d0b2d285d974058a0e",
    );
    assert_eq!(auth, expected);
    assert_eq!(auth.len(), CLIENT_AUTH_LEN);

    let outs: Vec<_> = s.on_message(&expected, T0).unwrap().collect();
    assert_eq!(outs.len(), 2);
    match &outs[0] {
        Output::Send(f) => assert_eq!(f.as_bytes(), [0x02]), // ServerConfirmsAuth
        o => panic!("{o:?}"),
    }
    match &outs[1] {
        Output::Authenticated { peer } => assert_eq!(*peer, pk),
        o => panic!("{o:?}"),
    }
}

#[test]
fn server_denies_auth_vector() {
    let pk = key42().public();
    let cfg = IngressConfig::new(pk);
    let (_, mut s) =
        IngressSession::on_upgrade(&req(Some("iroh-relay-v2")), &cfg, &mut FixedRng(0x11), T0)
            .unwrap();
    // Signed over a different challenge.
    let auth = client_auth(&key42(), &[0x12; 16]);
    let close = s.on_message(&auth, T0).unwrap_err();
    // 03 ‖ varint(17) ‖ "signature invalid"
    let mut exp = vec![0x03, 17];
    exp.extend_from_slice(b"signature invalid");
    assert_eq!(close.frame.unwrap().as_bytes(), exp);
    assert_eq!(close.code, close_code::POLICY_VIOLATION);
}

#[test]
fn relay_to_client_datagram_vectors() {
    let pk = key42().public();
    let batch = DatagramBatch {
        ecn: Some(Ecn::Ce),
        segment_size: NonZeroU16::new(6),
        contents: HELLO,
    };
    assert_eq!(
        encode_datagrams(&pk, &batch).unwrap().as_bytes(),
        hexd(&format!("07 {PK42} 03 0006 48656c6c6f20576f726c6421"))
    );
    let single = DatagramBatch {
        segment_size: None,
        ..batch
    };
    assert_eq!(
        encode_datagrams(&pk, &single).unwrap().as_bytes(),
        hexd(&format!("06 {PK42} 03 48656c6c6f20576f726c6421"))
    );
    // Session form is identical (src = local id).
    let s = authed(pk, &key42(), None);
    assert_eq!(
        s.encode_datagrams(&batch).unwrap(),
        encode_datagrams(&pk, &batch).unwrap()
    );
}

#[test]
fn client_to_relay_datagram_vectors_decode() {
    let pk = key42().public();
    let mut s = authed(pk, &key42(), None);
    // ClientToRelayDatagramBatch(ecn=CE, seg=6, "Hello World!")
    let f = hexd(&format!("05 {PK42} 03 0006 48656c6c6f20576f726c6421"));
    let outs: Vec<_> = s.on_message(&f, T0).unwrap().collect();
    match &outs[..] {
        [Output::Datagrams { from, batch }] => {
            assert_eq!(*from, pk);
            assert_eq!(batch.ecn, Some(Ecn::Ce));
            assert_eq!(batch.segment_size, NonZeroU16::new(6));
            assert_eq!(batch.contents, HELLO);
            assert_eq!(
                batch.segments().collect::<Vec<_>>(),
                vec![&b"Hello "[..], b"World!"]
            );
        }
        o => panic!("{o:?}"),
    }
    // ClientToRelayDatagram(ecn=CE, "Hello World!")
    let f = hexd(&format!("04 {PK42} 03 48656c6c6f20576f726c6421"));
    let outs: Vec<_> = s.on_message(&f, T0).unwrap().collect();
    match &outs[..] {
        [Output::Datagrams { batch, .. }] => {
            assert_eq!(batch.segment_size, None);
            assert_eq!(batch.contents, HELLO);
        }
        o => panic!("{o:?}"),
    }
    // Captured from the stock client: 05 dst 03 0003 "abcdefg".
    let f = hexd(&format!("05 {PK42} 03 0003 61626364656667"));
    let outs: Vec<_> = s.on_message(&f, T0).unwrap().collect();
    match &outs[..] {
        [Output::Datagrams { batch, .. }] => {
            assert_eq!(
                batch.segments().collect::<Vec<_>>(),
                vec![&b"abc"[..], b"def", b"g"]
            );
        }
        o => panic!("{o:?}"),
    }
    // Segment size 0 decodes as none, as upstream does.
    let f = hexd(&format!("05 {PK42} 02 0000 61"));
    let outs: Vec<_> = s.on_message(&f, T0).unwrap().collect();
    match &outs[..] {
        [Output::Datagrams { batch, .. }] => {
            assert_eq!(batch.segment_size, None);
            assert_eq!(batch.ecn, Some(Ecn::Ect0));
        }
        o => panic!("{o:?}"),
    }
}

#[test]
fn ping_pong_vector() {
    let pk = key42().public();
    let mut s = authed(pk, &key42(), None);
    let m = hexd("09 2a2a2a2a2a2a2a2a");
    let outs: Vec<_> = s.on_message(&m, T0).unwrap().collect();
    match &outs[..] {
        [Output::Send(f)] => assert_eq!(f.as_bytes(), hexd("0a 2a2a2a2a2a2a2a2a")),
        o => panic!("{o:?}"),
    }
    // Client Pong is ignored.
    assert!(
        s.on_message(&hexd("0a 2a2a2a2a2a2a2a2a"), T0)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn status_vectors_version_gated() {
    let pk = key42().public();
    let s = authed(pk, &key42(), None);
    assert_eq!(s.version(), ProtocolVersion::V2);
    assert_eq!(
        s.status_frame(Status::SameEndpointIdConnected)
            .unwrap()
            .as_bytes(),
        hexd("0d 01")
    );
    assert_eq!(
        s.status_frame(Status::RateLimited).unwrap().as_bytes(),
        hexd("0d 02")
    );
    assert_eq!(
        s.status_frame(Status::Healthy).unwrap().as_bytes(),
        hexd("0d 00")
    );

    // V1: Health with upstream's Display text, never Status.
    let f = encode_status(ProtocolVersion::V1, Status::SameEndpointIdConnected);
    assert_eq!(f.as_bytes()[0], 0x0b);
    assert_eq!(
        &f.as_bytes()[1..],
        b"Another endpoint connected with the same endpoint id. No more messages will be received."
    );
}
