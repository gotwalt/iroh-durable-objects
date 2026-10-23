//! The socket table: one session per socket, one socket per peer, restore and sweep.

mod common;

use std::time::Duration;

use common::*;
use iroh_base::SecretKey;
use iroh_relay_ingress::{
    CLOSE_SUPERSEDED, Effect, HANDSHAKE_TIMEOUT, IngressConfig, SessionSnapshot, Sockets,
    close_code,
};

const V2: Option<&str> = Some("iroh-relay-v2, iroh-relay-v1");

fn table() -> (Sockets<u32>, SecretKey) {
    let local = key42();
    (Sockets::new(IngressConfig::new(local.public())), local)
}

/// Upgrades socket `key` and authenticates it as `peer`; returns the snapshot it persisted.
fn connect(t: &mut Sockets<u32>, key: u32, peer: &SecretKey, now: Duration) -> Vec<Effect<u32>> {
    let up = t.upgrade(key, &req(V2), &mut FixedRng(7), now).unwrap();
    assert!(matches!(up.effects[0], Effect::Persist(k, _) if k == key));
    assert!(matches!(up.effects[1], Effect::Deadline(d) if d == now + HANDSHAKE_TIMEOUT));
    t.message(&key, &client_auth(peer, &[7u8; 16]), now)
}

fn snapshot(effects: &[Effect<u32>], key: u32) -> Option<SessionSnapshot> {
    effects.iter().rev().find_map(|e| match e {
        Effect::Persist(k, s) if *k == key => Some(s.clone()),
        _ => None,
    })
}

/// Authentication persists the session, confirms it to the peer, and routes the peer to the
/// socket; a datagram for this endpoint reaches the host.
#[test]
fn authenticate_and_receive() {
    let (mut t, local) = table();
    let peer = SecretKey::from_bytes(&[1; 32]);
    let fx = connect(&mut t, 1, &peer, T0);
    assert!(
        matches!(&fx[0], Effect::Send(1, f) if f.as_bytes()[0] == 2),
        "{fx:?}"
    );
    assert!(matches!(&fx[1], Effect::Persist(1, _)), "{fx:?}");
    assert!(
        matches!(&fx[2], Effect::PeerUp(p, 1) if *p == peer.public()),
        "{fx:?}"
    );
    assert_eq!(fx.len(), 3);
    assert_eq!(t.socket_of(&peer.public()), Some(&1));
    assert_eq!(t.peer_of(&1), Some(peer.public()));

    let fx = t.message(&1, &client_datagram(&local.public(), None, None, b"hi"), T0);
    assert!(
        matches!(&fx[..], [Effect::Datagrams { from, contents, .. }] if *from == peer.public() && contents == b"hi"),
        "{fx:?}"
    );
}

/// A second socket of the same peer supersedes the first: the old one is told
/// `SameEndpointIdConnected` and closed with 4000, and the peer stays up on the new socket.
#[test]
fn second_socket_supersedes_first() {
    let (mut t, _) = table();
    let peer = SecretKey::from_bytes(&[1; 32]);
    connect(&mut t, 1, &peer, T0);
    let fx = connect(&mut t, 2, &peer, T0);
    let status = fx.iter().position(|e| {
        matches!(e, Effect::Send(1, f) if decode_server(f.as_bytes()) == Some(ServerMsg::Status(1)))
    });
    let close = fx
        .iter()
        .position(|e| matches!(e, Effect::Close(1, CLOSE_SUPERSEDED, "superseded")));
    let up = fx
        .iter()
        .position(|e| matches!(e, Effect::PeerUp(p, 2) if *p == peer.public()));
    assert!(
        status.is_some() && close.is_some() && up.is_some(),
        "{fx:?}"
    );
    assert!(status < close && close < up, "{fx:?}");
    assert!(
        !fx.iter().any(|e| matches!(e, Effect::PeerDown(_))),
        "{fx:?}"
    );
    assert_eq!(t.len(), 1);
    assert_eq!(t.socket_of(&peer.public()), Some(&2));
    // The runtime later reports the old socket closed: nothing changes.
    assert!(t.closed(&1).is_empty());
    assert_eq!(t.socket_of(&peer.public()), Some(&2));
}

/// The peer goes down when its socket closes, whoever closes it.
#[test]
fn peer_down_on_close() {
    let (mut t, _) = table();
    let peer = SecretKey::from_bytes(&[1; 32]);
    connect(&mut t, 1, &peer, T0);
    let fx = t.closed(&1);
    assert!(
        matches!(&fx[..], [Effect::PeerDown(p)] if *p == peer.public()),
        "{fx:?}"
    );
    assert!(t.is_empty() && t.peers().next().is_none());

    // Closed by the session: a text message.
    connect(&mut t, 2, &peer, T0);
    let fx = t.text(&2);
    assert!(
        matches!(
            &fx[..],
            [
                Effect::Close(2, close_code::UNSUPPORTED_DATA, _),
                Effect::PeerDown(_)
            ]
        ),
        "{fx:?}"
    );
    // A message for a socket the table does not know closes it.
    let fx = t.message(&9, &[0x0c], T0);
    assert!(
        matches!(&fx[..], [Effect::Close(9, close_code::PROTOCOL_ERROR, _)]),
        "{fx:?}"
    );
}

/// Sessions come back from their snapshots after a restart; a socket without a snapshot or
/// past its handshake deadline is closed, and of two restored sockets of one peer the later
/// one wins.
#[test]
fn restore_after_restart() {
    let (mut t, _) = table();
    let (a, b) = (
        SecretKey::from_bytes(&[1; 32]),
        SecretKey::from_bytes(&[2; 32]),
    );
    let snap_a = snapshot(&connect(&mut t, 1, &a, T0), 1).unwrap();
    let snap_b = snapshot(&connect(&mut t, 2, &b, T0), 2).unwrap();
    let pending = t.upgrade(3, &req(V2), &mut FixedRng(7), T0).unwrap();
    let snap_pending = snapshot(&pending.effects, 3).unwrap();

    let (mut t, _) = table();
    let now = T0 + Duration::from_secs(5);
    let fx = t.restore(1, Some(snap_a.clone()), now);
    assert!(
        matches!(&fx[..], [Effect::PeerUp(p, 1)] if *p == a.public()),
        "{fx:?}"
    );
    let fx = t.restore(2, Some(snap_b), now);
    assert!(matches!(&fx[..], [Effect::PeerUp(_, 2)]), "{fx:?}");
    assert!(t.restore(3, Some(snap_pending.clone()), now).is_empty());
    let fx = t.restore(4, None, now);
    assert!(
        matches!(&fx[..], [Effect::Close(4, close_code::PROTOCOL_ERROR, _)]),
        "{fx:?}"
    );
    // A second attachment of peer `a`: the host stopped between the steps of a supersede.
    let fx = t.restore(5, Some(snap_a), now);
    assert!(
        matches!(&fx[..], [Effect::Close(1, CLOSE_SUPERSEDED, _), Effect::PeerUp(p, 5)] if *p == a.public()),
        "{fx:?}"
    );
    assert_eq!(t.len(), 3);

    // Restored after the deadline, the pending socket is closed.
    let (mut t, _) = table();
    let late = T0 + HANDSHAKE_TIMEOUT;
    let fx = t.restore(3, Some(snap_pending), late);
    assert!(
        matches!(&fx[..], [Effect::Close(3, close_code::POLICY_VIOLATION, _)]),
        "{fx:?}"
    );
}

/// The sweep closes unauthenticated sockets past their deadline and reports the next
/// deadline; authenticated sockets are left alone.
#[test]
fn sweep_closes_expired_handshakes() {
    let (mut t, _) = table();
    let peer = SecretKey::from_bytes(&[1; 32]);
    connect(&mut t, 1, &peer, T0);
    t.upgrade(2, &req(V2), &mut FixedRng(7), T0).unwrap();
    let later = T0 + Duration::from_secs(10);
    t.upgrade(3, &req(V2), &mut FixedRng(7), later).unwrap();

    let fx = t.sweep(T0 + Duration::from_secs(1));
    assert!(
        matches!(&fx[..], [Effect::Deadline(d)] if *d == T0 + HANDSHAKE_TIMEOUT),
        "{fx:?}"
    );
    let fx = t.sweep(T0 + HANDSHAKE_TIMEOUT);
    assert!(
        matches!(&fx[..], [Effect::Close(2, close_code::POLICY_VIOLATION, "handshake timeout"), Effect::Deadline(d)] if *d == later + HANDSHAKE_TIMEOUT),
        "{fx:?}"
    );
    let fx = t.sweep(later + HANDSHAKE_TIMEOUT);
    assert!(matches!(&fx[..], [Effect::Close(3, ..)]), "{fx:?}");
    assert_eq!(t.len(), 1);
    assert_eq!(t.next_deadline(), None);
}

/// A changed destination-violation count is persisted, so a restart cannot reset it.
#[test]
fn violations_are_persisted() {
    let (mut t, _) = table();
    let peer = SecretKey::from_bytes(&[1; 32]);
    connect(&mut t, 1, &peer, T0);
    let other = SecretKey::from_bytes(&[9; 32]).public();
    let fx = t.message(&1, &client_datagram(&other, None, None, b"x"), T0);
    assert!(matches!(&fx[..], [Effect::Persist(1, _)]), "{fx:?}");
    assert_eq!(t.session(&1).unwrap().dst_violations(), 1);
}
