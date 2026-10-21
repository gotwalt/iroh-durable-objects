//! Malformed, out-of-order and hostile client input.

mod common;

use std::{num::NonZeroU16, time::Duration};

use common::*;
use iroh_base::{PublicKey, SecretKey};
use iroh_relay_ingress::*;

fn other() -> SecretKey {
    SecretKey::from_bytes(&[9u8; 32])
}

fn local() -> PublicKey {
    SecretKey::from_bytes(&[1u8; 32]).public()
}

fn fresh(proto: &str) -> IngressSession {
    IngressSession::on_upgrade(
        &req(Some(proto)),
        &IngressConfig::new(local()),
        &mut FixedRng(7),
        T0,
    )
    .unwrap()
    .1
}

#[test]
fn upgrade_version_negotiation() {
    let cfg = IngressConfig::new(local());
    let v = |p: Option<&str>| {
        IngressSession::on_upgrade(&req(p), &cfg, &mut rand::rng(), T0).map(|(r, _)| r.version)
    };
    assert_eq!(
        v(Some("iroh-relay-v2, iroh-relay-v1")).unwrap(),
        ProtocolVersion::V2
    );
    assert_eq!(
        v(Some("iroh-relay-v1 ,iroh-relay-v2")).unwrap(),
        ProtocolVersion::V2
    );
    assert_eq!(v(Some("iroh-relay-v1")).unwrap(), ProtocolVersion::V1);
    assert_eq!(
        v(Some("foo, iroh-relay-v1, bar")).unwrap(),
        ProtocolVersion::V1
    );
    for bad in [
        None,
        Some(""),
        Some("iroh-relay-v3"),
        Some("IROH-RELAY-V2"),
        Some("iroh-relay-v2x"),
    ] {
        let err = v(bad).unwrap_err();
        assert_eq!(err.http_status(), 400, "{bad:?}");
    }
}

#[test]
fn upgrade_path_and_method() {
    let cfg = IngressConfig::new(local());
    let mut r = req(Some("iroh-relay-v2"));
    r.path = "/relay?token=abc";
    assert!(IngressSession::on_upgrade(&r, &cfg, &mut rand::rng(), T0).is_ok());
    r.path = "/";
    assert_eq!(
        IngressSession::on_upgrade(&r, &cfg, &mut rand::rng(), T0)
            .unwrap_err()
            .http_status(),
        404
    );
    r.path = "/relay/x";
    assert_eq!(
        IngressSession::on_upgrade(&r, &cfg, &mut rand::rng(), T0)
            .unwrap_err()
            .http_status(),
        404
    );
    r.path = "/relay";
    r.method = "POST";
    assert_eq!(
        IngressSession::on_upgrade(&r, &cfg, &mut rand::rng(), T0)
            .unwrap_err()
            .http_status(),
        405
    );
}

#[test]
fn challenges_are_random() {
    let cfg = IngressConfig::new(local());
    let a = IngressSession::on_upgrade(&req(Some("iroh-relay-v2")), &cfg, &mut rand::rng(), T0)
        .unwrap()
        .0;
    let b = IngressSession::on_upgrade(&req(Some("iroh-relay-v2")), &cfg, &mut rand::rng(), T0)
        .unwrap()
        .0;
    assert_eq!(a.challenge.as_bytes().len(), 17);
    assert_ne!(a.challenge, b.challenge);
}

#[test]
fn bad_signature() {
    let mut s = fresh("iroh-relay-v2");
    // Signed with the wrong key for the claimed public key.
    let mut auth = client_auth(&other(), &[7; 16]);
    auth[1..33].copy_from_slice(local().as_bytes());
    let c = s.on_message(&auth, T0).unwrap_err();
    assert!(matches!(c.reason, CloseReason::SignatureInvalid { .. }));
    assert_eq!(c.frame.unwrap().as_bytes()[0], 0x03);
    assert!(s.is_closed());
    // Dead afterwards.
    let c = s
        .on_message(&client_auth(&other(), &[7; 16]), T0)
        .unwrap_err();
    assert!(matches!(c.reason, CloseReason::AlreadyClosed { .. }));

    // Flipped signature bit.
    let mut s = fresh("iroh-relay-v2");
    let mut auth = client_auth(&other(), &[7; 16]);
    auth[40] ^= 1;
    assert!(matches!(
        s.on_message(&auth, T0).unwrap_err().reason,
        CloseReason::SignatureInvalid { .. }
    ));
}

#[test]
fn invalid_public_key() {
    // Find 32 bytes that are not a valid compressed Edwards point.
    let bad = (0u8..=255)
        .map(|i| {
            let mut k = [i; 32];
            k[31] &= 0x7f;
            k
        })
        .find(|k| PublicKey::from_bytes(k).is_err())
        .expect("some non-point");
    let mut s = fresh("iroh-relay-v2");
    let mut auth = client_auth(&other(), &[7; 16]);
    auth[1..33].copy_from_slice(&bad);
    let c = s.on_message(&auth, T0).unwrap_err();
    assert!(matches!(c.reason, CloseReason::InvalidPublicKey { .. }));
    assert!(c.frame.is_none());
}

#[test]
fn malformed_client_auth() {
    let good = client_auth(&other(), &[7; 16]);
    let mut short = good.clone();
    short.pop();
    let mut long = good.clone();
    long.push(0);
    let mut badlen = good.clone();
    badlen[33] = 0x3f;
    for m in [short, long, badlen, vec![0x01]] {
        let c = fresh("iroh-relay-v2").on_message(&m, T0).unwrap_err();
        assert!(matches!(c.reason, CloseReason::InvalidFrame { .. }), "{c}");
        assert_eq!(c.code, close_code::PROTOCOL_ERROR);
    }
}

#[test]
fn frames_before_auth() {
    let l = local();
    let frames: Vec<Vec<u8>> = vec![
        hexd("09 0102030405060708"),
        hexd("0a 0102030405060708"),
        client_datagram(&l, None, None, b"x"),
        client_datagram(&l, None, NonZeroU16::new(1), b"xy"),
        hexd("00 11111111111111111111111111111111"),
        hexd("02"),
        hexd("0d 00"),
    ];
    for f in frames {
        let c = fresh("iroh-relay-v2").on_message(&f, T0).unwrap_err();
        assert!(
            matches!(
                c.reason,
                CloseReason::UnexpectedFrame {
                    authenticated: false,
                    ..
                }
            ),
            "{c}"
        );
    }
}

#[test]
fn unexpected_after_auth() {
    for f in [
        client_auth(&other(), &[7; 16]),
        hexd("00 11111111111111111111111111111111"),
        hexd("06")
            .into_iter()
            .chain(local().as_bytes().iter().copied())
            .chain([0, 1])
            .collect(),
        hexd("08")
            .into_iter()
            .chain(local().as_bytes().iter().copied())
            .collect(),
        hexd("0c 0000000a 00000014"),
        hexd("0d 00"),
        hexd("0b 6869"),
    ] {
        let mut s = authed(local(), &other(), None);
        let c = s.on_message(&f, T0).unwrap_err();
        assert!(
            matches!(
                c.reason,
                CloseReason::UnexpectedFrame {
                    authenticated: true,
                    ..
                }
            ),
            "{c}"
        );
    }
    // Unknown frame types, incl. multi-byte varints.
    for f in [
        hexd("0e"),
        hexd("3f"),
        hexd("4040"),
        hexd("c00000000000000e"),
        hexd("ffffffffffffffff"),
    ] {
        let c = authed(local(), &other(), None)
            .on_message(&f, T0)
            .unwrap_err();
        assert!(
            matches!(c.reason, CloseReason::UnknownFrameType { .. }),
            "{c}"
        );
    }
    // Truncated varint / empty message.
    for f in [vec![], hexd("40"), hexd("80 00")] {
        let c = authed(local(), &other(), None)
            .on_message(&f, T0)
            .unwrap_err();
        assert!(matches!(c.reason, CloseReason::EmptyMessage { .. }), "{c}");
    }
}

#[test]
fn non_minimal_varint_type_is_accepted() {
    // QUIC varints may be non-minimal; upstream VarInt::decode accepts them.
    let mut s = authed(local(), &other(), None);
    let m = hexd("4009 0102030405060708");
    let outs: Vec<_> = s.on_message(&m, T0).unwrap().collect();
    assert!(matches!(&outs[..], [Output::Send(f)] if f.as_bytes() == hexd("0a 0102030405060708")));
}

#[test]
fn bad_lengths_after_auth() {
    let l = local();
    let mut short_dgram = client_datagram(&l, None, None, b"");
    short_dgram.pop(); // no ECN byte
    let mut short_batch = client_datagram(&l, None, NonZeroU16::new(1), b"");
    short_batch.pop(); // only one byte of segment size
    for f in [
        hexd("09 01020304050607"),
        hexd("09 010203040506070809"),
        hexd("0a 01"),
        short_dgram,
        short_batch,
        hexd("04 0102"),
    ] {
        let c = authed(l, &other(), None).on_message(&f, T0).unwrap_err();
        assert!(matches!(c.reason, CloseReason::InvalidFrame { .. }), "{c}");
    }
}

#[test]
fn oversize() {
    let l = local();
    let mut s = authed(l, &other(), None);
    // Exactly MAX_PACKET_SIZE after the type: accepted.
    let ok = client_datagram(&l, None, None, &vec![0u8; MAX_DATAGRAM_CONTENTS]);
    assert_eq!(ok.len(), 1 + MAX_PACKET_SIZE);
    assert_eq!(s.on_message(&ok, T0).unwrap().count(), 1);
    // One more: closed.
    let big = client_datagram(&l, None, None, &vec![0u8; MAX_DATAGRAM_CONTENTS + 1]);
    let c = s.on_message(&big, T0).unwrap_err();
    assert!(matches!(
        c.reason,
        CloseReason::FrameTooLarge { len: 65537, .. }
    ));
    // Oversize before auth too.
    let c = fresh("iroh-relay-v2")
        .on_message(&vec![0x01; 1 << 20], T0)
        .unwrap_err();
    assert!(matches!(c.reason, CloseReason::FrameTooLarge { .. }));
}

#[test]
fn wrong_destination_dropped_counted_then_closed() {
    let l = local();
    let cfg = IngressConfig {
        dst_violation_limit: 3,
        ..IngressConfig::new(l)
    };
    let mut s = authed(l, &other(), Some(cfg));
    let foreign = other().public();
    let f = client_datagram(&foreign, Some(Ecn::Ce), None, b"forward me");
    for i in 1..=3 {
        assert!(s.on_message(&f, T0).unwrap().is_empty(), "never forwarded");
        assert_eq!(s.dst_violations(), i);
    }
    // Legit datagrams still flow in between.
    assert_eq!(
        s.on_message(&client_datagram(&l, None, None, b"ok"), T0)
            .unwrap()
            .count(),
        1
    );
    let c = s.on_message(&f, T0).unwrap_err();
    assert!(matches!(
        c.reason,
        CloseReason::DestinationViolations { count: 4, .. }
    ));
    assert_eq!(c.code, close_code::POLICY_VIOLATION);
}

#[test]
fn flood() {
    let l = local();
    let mut s = authed(l, &other(), None);
    for i in 0..10_000u64 {
        let mut f = vec![0x09];
        f.extend_from_slice(&i.to_be_bytes());
        let outs: Vec<_> = s.on_message(&f, T0).unwrap().collect();
        match &outs[..] {
            [Output::Send(p)] => assert_eq!(&p.as_bytes()[1..], &i.to_be_bytes()),
            o => panic!("{o:?}"),
        }
    }
    for _ in 0..10_000 {
        assert_eq!(
            s.on_message(&client_datagram(&l, None, None, b"d"), T0)
                .unwrap()
                .count(),
            1
        );
    }
    // Default limit bounds a wrong-destination flood.
    let foreign = client_datagram(&other().public(), None, None, b"x");
    let mut n = 0;
    while s.on_message(&foreign, T0).is_ok() {
        n += 1;
        assert!(n <= DEFAULT_DST_VIOLATION_LIMIT);
    }
    assert_eq!(n, DEFAULT_DST_VIOLATION_LIMIT);
}

#[test]
fn handshake_timeout() {
    let mut s = fresh("iroh-relay-v2");
    assert_eq!(s.handshake_deadline(), Some(T0 + HANDSHAKE_TIMEOUT));
    let late = T0 + HANDSHAKE_TIMEOUT + Duration::from_millis(1);
    let c = s
        .on_message(&client_auth(&other(), &[7; 16]), late)
        .unwrap_err();
    assert!(matches!(c.reason, CloseReason::HandshakeTimeout { .. }));
    // At the deadline exactly: still accepted.
    let mut s = fresh("iroh-relay-v2");
    assert!(
        s.on_message(&client_auth(&other(), &[7; 16]), T0 + HANDSHAKE_TIMEOUT)
            .is_ok()
    );
    assert_eq!(s.handshake_deadline(), None);
}

#[test]
fn text_message_closes() {
    let mut s = authed(local(), &other(), None);
    let c = s.on_text_message();
    assert_eq!(c.code, close_code::UNSUPPORTED_DATA);
    assert!(s.is_closed());
}

#[test]
fn encode_guards() {
    let l = local();
    let s = fresh("iroh-relay-v2");
    let b = DatagramBatch {
        ecn: None,
        segment_size: None,
        contents: b"x",
    };
    assert!(matches!(
        s.encode_datagrams(&b),
        Err(EncodeError::NotAuthenticated { .. })
    ));
    assert!(s.status_frame(Status::Healthy).is_none());

    let s = authed(l, &other(), None);
    let empty = DatagramBatch { contents: b"", ..b };
    assert!(matches!(
        s.encode_datagrams(&empty),
        Err(EncodeError::Empty { .. })
    ));
    let max = vec![0u8; MAX_DATAGRAM_CONTENTS];
    assert_eq!(
        s.encode_datagrams(&DatagramBatch {
            contents: &max,
            ..b
        })
        .unwrap()
        .as_bytes()
        .len(),
        1 + MAX_PACKET_SIZE
    );
    let over = vec![0u8; MAX_DATAGRAM_CONTENTS + 1];
    assert!(matches!(
        s.encode_datagrams(&DatagramBatch {
            contents: &over,
            ..b
        }),
        Err(EncodeError::TooLarge { .. })
    ));
    let maxb = vec![0u8; MAX_BATCH_CONTENTS];
    let seg = NonZeroU16::new(1200);
    assert!(
        s.encode_datagrams(&DatagramBatch {
            contents: &maxb,
            segment_size: seg,
            ..b
        })
        .is_ok()
    );
    let overb = vec![0u8; MAX_BATCH_CONTENTS + 1];
    assert!(
        s.encode_datagrams(&DatagramBatch {
            contents: &overb,
            segment_size: seg,
            ..b
        })
        .is_err()
    );
}

#[test]
fn self_peer_and_v1_session() {
    // V1 session: Health instead of Status.
    let l = local();
    let cfg = IngressConfig::new(l);
    let (_, mut s) =
        IngressSession::on_upgrade(&req(Some("iroh-relay-v1")), &cfg, &mut FixedRng(3), T0)
            .unwrap();
    s.on_message(&client_auth(&other(), &[3; 16]), T0)
        .unwrap()
        .for_each(drop);
    let f = s.status_frame(Status::SameEndpointIdConnected).unwrap();
    assert!(matches!(
        decode_server(f.as_bytes()),
        Some(ServerMsg::Health(_))
    ));
}
