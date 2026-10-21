//! Property tests for frame decoding and encoding.

mod common;

use std::num::NonZeroU16;

use common::*;
use iroh_base::SecretKey;
use iroh_relay_ingress::*;
use proptest::prelude::*;

fn ecn() -> impl Strategy<Value = Option<Ecn>> {
    prop_oneof![
        Just(None),
        Just(Some(Ecn::Ect0)),
        Just(Some(Ecn::Ect1)),
        Just(Some(Ecn::Ce))
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// Arbitrary bytes never panic, before or after authentication.
    #[test]
    fn arbitrary_bytes_never_panic(msg in prop::collection::vec(any::<u8>(), 0..300), authed_first in any::<bool>()) {
        let local = SecretKey::from_bytes(&[1u8; 32]).public();
        let mut s = if authed_first {
            authed(local, &key42(), None)
        } else {
            IngressSession::on_upgrade(&req(Some("iroh-relay-v2")), &IngressConfig::new(local), &mut FixedRng(1), T0).unwrap().1
        };
        let _ = s.on_message(&msg, T0).map(|o| o.count());
        let _ = s.on_message(&msg, T0).map(|o| o.count());
    }

    /// Arbitrary frames with a valid type prefix and plausible lengths.
    #[test]
    fn typed_frames_never_panic(t in 0u8..20, body in prop::collection::vec(any::<u8>(), 0..120)) {
        let local = SecretKey::from_bytes(&[1u8; 32]).public();
        let mut s = authed(local, &key42(), None);
        let mut m = vec![t];
        m.extend_from_slice(&body);
        let _ = s.on_message(&m, T0).map(|o| o.count());
    }

    /// Client datagram frames round-trip through the decoder with ECN and
    /// segment size preserved.
    #[test]
    fn client_datagrams_roundtrip(
        ecn in ecn(),
        seg in prop::option::of(1u16..=1500),
        contents in prop::collection::vec(any::<u8>(), 1..4000),
    ) {
        let local = SecretKey::from_bytes(&[1u8; 32]).public();
        let peer = key42();
        let mut s = authed(local, &peer, None);
        let seg = seg.and_then(NonZeroU16::new);
        let f = client_datagram(&local, ecn, seg, &contents);
        let outs: Vec<_> = s.on_message(&f, T0).unwrap().collect();
        prop_assert_eq!(outs.len(), 1);
        match &outs[0] {
            Output::Datagrams { from, batch } => {
                prop_assert_eq!(*from, peer.public());
                prop_assert_eq!(batch.ecn, ecn);
                prop_assert_eq!(batch.segment_size, seg);
                prop_assert_eq!(batch.contents, &contents[..]);
                let joined: Vec<u8> = batch.segments().flatten().copied().collect();
                prop_assert_eq!(joined, contents.clone());
                if let Some(seg) = seg {
                    let segs: Vec<_> = batch.segments().collect();
                    prop_assert_eq!(segs.len(), contents.len().div_ceil(usize::from(seg.get())));
                    prop_assert!(segs[..segs.len() - 1].iter().all(|x| x.len() == usize::from(seg.get())));
                }
            }
            o => prop_assert!(false, "{:?}", o),
        }
    }

    /// Server datagram frames decode (reference decoder) to the same batch.
    #[test]
    fn server_datagrams_roundtrip(
        ecn in ecn(),
        seg in prop::option::of(1u16..=1500),
        contents in prop::collection::vec(any::<u8>(), 1..4000),
    ) {
        let src = key42().public();
        let seg = seg.and_then(NonZeroU16::new);
        let batch = DatagramBatch { ecn, segment_size: seg, contents: &contents };
        let f = encode_datagrams(&src, &batch).unwrap();
        prop_assert_eq!(
            decode_server(f.as_bytes()),
            Some(ServerMsg::Datagrams { src: *src.as_bytes(), ecn, segment_size: seg, contents: contents.clone() })
        );
    }

    /// Ping payloads are echoed exactly.
    #[test]
    fn ping_echo(data in any::<[u8; 8]>()) {
        let local = SecretKey::from_bytes(&[1u8; 32]).public();
        let mut s = authed(local, &key42(), None);
        let mut m = vec![0x09];
        m.extend_from_slice(&data);
        let outs: Vec<_> = s.on_message(&m, T0).unwrap().collect();
        match &outs[..] {
            [Output::Send(f)] => prop_assert_eq!(decode_server(f.as_bytes()), Some(ServerMsg::Pong(data))),
            o => prop_assert!(false, "{:?}", o),
        }
    }

    /// Version negotiation never panics and only returns offered versions.
    #[test]
    fn negotiate_total(offer in ".{0,80}") {
        if let Some(v) = ProtocolVersion::negotiate(&offer) {
            prop_assert!(offer.contains(v.as_str()));
        }
    }
}
