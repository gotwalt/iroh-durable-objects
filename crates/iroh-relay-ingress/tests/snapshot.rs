//! Snapshot and restore, as across eviction from memory.

mod common;

use common::*;
use iroh_base::SecretKey;
use iroh_relay_ingress::*;

#[test]
fn authenticated_roundtrip_small_and_live() {
    let local = SecretKey::from_bytes(&[1u8; 32]).public();
    let peer = key42();
    let cfg = IngressConfig::new(local);
    let mut s = authed(local, &peer, None);
    // One violation to check it persists.
    s.on_message(&client_datagram(&peer.public(), None, None, b"x"), T0)
        .unwrap()
        .for_each(drop);

    let snap = s.snapshot().unwrap();
    assert_eq!(snap.peer(), Some(peer.public()));
    let json = serde_json::to_vec(&snap).unwrap();
    let pc = postcard::to_allocvec(&snap).unwrap();
    eprintln!(
        "snapshot json={} B postcard={} B: {}",
        json.len(),
        pc.len(),
        String::from_utf8_lossy(&json)
    );
    assert!(json.len() < 2048 && pc.len() < 2048);

    for bytes in [
        serde_json::from_slice::<SessionSnapshot>(&json).unwrap(),
        postcard::from_bytes(&pc).unwrap(),
    ] {
        assert_eq!(bytes, snap);
        let mut r = IngressSession::restore(bytes, &cfg);
        assert!(r.is_authenticated());
        assert_eq!(r.peer(), Some(peer.public()));
        assert_eq!(r.dst_violations(), 1);
        assert_eq!(r.handshake_deadline(), None);
        // Ping and datagrams immediately, no handshake.
        let m = hexd("09 0102030405060708");
        let outs: Vec<_> = r.on_message(&m, T0).unwrap().collect();
        assert!(
            matches!(&outs[..], [Output::Send(f)] if f.as_bytes() == hexd("0a 0102030405060708"))
        );
        let d = client_datagram(&local, Some(Ecn::Ect0), None, b"quic");
        let outs: Vec<_> = r.on_message(&d, T0).unwrap().collect();
        assert!(
            matches!(&outs[..], [Output::Datagrams { from, batch }] if *from == peer.public() && batch.contents == b"quic")
        );
        assert!(
            r.encode_datagrams(&DatagramBatch {
                ecn: None,
                segment_size: None,
                contents: b"back"
            })
            .is_ok()
        );
    }
}

#[test]
fn awaiting_auth_roundtrip() {
    let local = SecretKey::from_bytes(&[1u8; 32]).public();
    let cfg = IngressConfig::new(local);
    let (_, s) = IngressSession::on_upgrade(
        &req(Some("iroh-relay-v1, iroh-relay-v2")),
        &cfg,
        &mut FixedRng(5),
        T0,
    )
    .unwrap();
    let snap = s.snapshot().unwrap();
    assert_eq!(snap.peer(), None);
    let json = serde_json::to_string(&snap).unwrap();
    let mut r = IngressSession::restore(serde_json::from_str(&json).unwrap(), &cfg);
    assert_eq!(r.version(), ProtocolVersion::V2);
    assert_eq!(r.handshake_deadline(), Some(T0 + HANDSHAKE_TIMEOUT));
    let m = client_auth(&key42(), &[5; 16]);
    let outs: Vec<_> = r.on_message(&m, T0).unwrap().collect();
    assert!(matches!(outs.last(), Some(Output::Authenticated { .. })));
}

#[test]
fn closed_has_no_snapshot() {
    let local = SecretKey::from_bytes(&[1u8; 32]).public();
    let mut s = authed(local, &key42(), None);
    let _ = s.on_text_message();
    assert!(s.snapshot().is_none());
}
