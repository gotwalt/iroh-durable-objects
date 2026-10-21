//! The chat room, `iroh-do/room/1` (design note 008).
//!
//! Requests arrive on bidirectional streams that members open, one request per stream.
//! Events are delivered on unidirectional streams that the node opens, on a connection to the
//! member that is already open in either direction or else on a new one dialed over the
//! member's relay socket.

use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    fmt::Write as _,
    sync::Arc,
    time::Duration,
};

use iroh::{
    EndpointId,
    endpoint::{Connection, RecvStream, SendStream},
};
use iroh_ingress_transport::peer_endpoint_addr;
use n0_future::time::Instant;
use tracing::{debug, info, warn};

use crate::{
    DELIVERY_BATCH, LogKind, LogRow, MAX_LOG_TEXT, Member, ROOM_ALPN, Shared,
    protocols::{self, Activity},
};

/// Longest request line in bytes, excluding the terminator.
const MAX_REQUEST: usize = 1024;
/// Events in the `/join` reply, and the default `n` of `/history`.
const HISTORY_DEFAULT: usize = 50;
/// Largest `n` of `/history`.
const HISTORY_MAX: usize = 200;
/// Most events in one `/since` reply.
const SINCE_MAX: usize = 500;
/// Requests a member may make per [`RATE_WINDOW`].
const RATE_LIMIT: usize = 10;
/// Window of the request rate limit.
///
/// The limiter is in memory. That is enough: the object hibernates only after `t_idle` +
/// `t_linger` without connections, which is longer than the window, so hibernation cannot
/// reset a window that is still running. Abrupt eviction can.
const RATE_WINDOW: Duration = Duration::from_secs(10);
/// Longest nick in characters.
const NICK_MAX: usize = 24;

/// In-memory room state: open connections, pending deliveries and request times.
#[derive(Default)]
pub(crate) struct Room {
    /// Open room connections by peer, whichever side dialed. The newest one wins.
    conns: HashMap<EndpointId, (Connection, Arc<Activity>)>,
    /// Event lines waiting for delivery, by recipient. A key is present while that
    /// recipient's delivery task runs.
    outbox: HashMap<EndpointId, Vec<String>>,
    /// Recent request times by peer, for the rate limit.
    requests: HashMap<EndpointId, VecDeque<Instant>>,
}

impl Room {
    /// Forgets every connection; called when the endpoint is closed.
    pub(crate) fn clear_connections(&mut self) {
        self.conns.clear();
    }
}

/// Returns the number of joined members and how many of them are present.
pub(crate) fn member_counts(node: &Shared) -> (usize, usize) {
    let members = node.host().members();
    let present = members.iter().filter(|m| node.has_socket(&m.peer)).count();
    (members.len(), present)
}

/// Serves requests on a room connection until it closes.
pub(crate) async fn serve_conn(node: Arc<Shared>, conn: Connection, activity: Arc<Activity>) {
    let peer = conn.remote_id();
    register(&node, &conn, &activity);
    while let Ok((send, recv)) = conn.accept_bi().await {
        let (n, a) = (node.clone(), activity.clone());
        node.spawn(async move { request_stream(n, peer, a, send, recv).await });
    }
    let mut room = node.room();
    if let Entry::Occupied(e) = room.conns.entry(peer)
        && e.get().0.stable_id() == conn.stable_id()
    {
        e.remove();
    }
}

fn register(node: &Shared, conn: &Connection, activity: &Arc<Activity>) {
    node.room()
        .conns
        .insert(conn.remote_id(), (conn.clone(), activity.clone()));
}

/// Reads one request, handles it and writes the reply.
async fn request_stream(
    node: Arc<Shared>,
    peer: EndpointId,
    activity: Arc<Activity>,
    mut send: SendStream,
    mut recv: RecvStream,
) {
    let _open = activity.stream();
    // Room for a `\r\n` terminator.
    let replies = match recv.read_to_end(MAX_REQUEST + 2).await {
        Ok(bytes) => match parse_request(&bytes) {
            Ok(line) => handle(&node, peer, &line),
            Err(e) => vec![err(e)],
        },
        Err(e) => {
            debug!(peer = %peer.fmt_short(), "room request: {e}");
            vec![err("request too long or unreadable")]
        }
    };
    activity.touch();
    let mut out = String::new();
    for r in replies {
        out.push_str(&r);
        out.push('\n');
    }
    if send.write_all(out.as_bytes()).await.is_ok() {
        let _ = send.finish();
    }
}

/// Returns the request line without its terminator.
fn parse_request(bytes: &[u8]) -> Result<String, &'static str> {
    let line = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.len() > MAX_REQUEST {
        return Err("request too long");
    }
    if line.contains(&b'\n') {
        return Err("one request per stream");
    }
    String::from_utf8(line.to_vec()).map_err(|_| "request is not utf-8")
}

/// Handles one request, logs it as a `room_request` event and returns the reply lines.
///
/// The event records the request kind and the member's nick, never the text.
fn handle(node: &Arc<Shared>, peer: EndpointId, line: &str) -> Vec<String> {
    let replies = handle_request(node, peer, line);
    let cmd = line.split(' ').next().unwrap_or_default();
    let kind = match cmd {
        "/join" | "/say" | "/me" | "/nick" | "/who" | "/history" | "/since" | "/quit" => &cmd[1..],
        _ => "unknown",
    };
    let error = replies
        .first()
        .and_then(|r| r.strip_prefix("{\"t\":\"err\""))
        .is_some();
    let nick = node.host().member(peer).map(|m| m.nick).unwrap_or_default();
    info!(event = "room_request", peer = %peer.to_z32(), kind, nick, ok = !error);
    replies
}

/// Handles one request and returns the reply lines.
///
/// Holding the room lock serializes requests, so checks such as nick uniqueness and the
/// writes that depend on them are atomic on multi-threaded hosts too.
fn handle_request(node: &Arc<Shared>, peer: EndpointId, line: &str) -> Vec<String> {
    let mut room = node.room();
    if !allow(&mut room, peer) {
        drop(room);
        node.update_stats(|s| s.rate_limited += 1);
        return vec![err(&format!(
            "rate limit: at most {RATE_LIMIT} requests per {}s",
            RATE_WINDOW.as_secs()
        ))];
    }
    let host = node.host();
    let me = host.member(peer).filter(|m| m.joined);
    let (cmd, arg) = match line.split_once(' ') {
        Some((c, a)) => (c, a.trim()),
        None => (line, ""),
    };
    if cmd == "/join" {
        return join(node, &mut room, peer, me, arg);
    }
    let Some(me) = me else {
        return vec![err("not joined: send /join [nick] first")];
    };
    match cmd {
        "/say" | "/me" => {
            if arg.is_empty() {
                return vec![err(&format!("usage: {cmd} <text>"))];
            }
            let kind = if cmd == "/say" {
                LogKind::Msg
            } else {
                LogKind::Act
            };
            store_and_publish(node, &mut room, peer, kind, &me.nick, arg)
        }
        "/nick" => match check_nick(node, peer, arg) {
            Err(e) => vec![err(&e)],
            Ok(()) if arg == me.nick => vec![err("nick unchanged")],
            Ok(()) => {
                let old = me.nick.clone();
                host.set_member(&Member {
                    nick: arg.to_string(),
                    ..me
                });
                store_and_publish(node, &mut room, peer, LogKind::Nick, arg, &old)
            }
        },
        "/who" => vec![who(node)],
        "/history" => {
            let n = if arg.is_empty() {
                HISTORY_DEFAULT
            } else {
                match arg.parse::<usize>() {
                    Ok(n) => n.min(HISTORY_MAX),
                    Err(_) => {
                        return vec![err(&format!("usage: /history [n], n <= {HISTORY_MAX}"))];
                    }
                }
            };
            history(&host.events_recent(n))
        }
        "/since" => match arg.parse::<u64>() {
            Ok(seq) => history(&host.events_after(seq, SINCE_MAX)),
            Err(_) => vec![err("usage: /since <seq>")],
        },
        "/quit" => {
            host.set_member(&Member {
                joined: false,
                ..me.clone()
            });
            room.outbox.remove(&peer);
            store_and_publish(node, &mut room, peer, LogKind::Part, &me.nick, arg)
        }
        _ => vec![err("unknown request")],
    }
}

/// Records a request from `peer`; returns whether it is within the rate limit.
fn allow(room: &mut Room, peer: EndpointId) -> bool {
    let now = Instant::now();
    let times = room.requests.entry(peer).or_default();
    while times
        .front()
        .is_some_and(|t| now.duration_since(*t) >= RATE_WINDOW)
    {
        times.pop_front();
    }
    if times.len() >= RATE_LIMIT {
        return false;
    }
    times.push_back(now);
    true
}

fn join(
    node: &Arc<Shared>,
    room: &mut Room,
    peer: EndpointId,
    me: Option<Member>,
    arg: &str,
) -> Vec<String> {
    let host = node.host();
    let mut replies = Vec::new();
    let you = match me {
        // Already joined: `/join <nick>` with another nick renames.
        Some(me) if arg.is_empty() || arg == me.nick => me,
        Some(me) => {
            if let Err(e) = check_nick(node, peer, arg) {
                return vec![err(&e)];
            }
            let old = me.nick.clone();
            let me = Member {
                nick: arg.to_string(),
                ..me
            };
            host.set_member(&me);
            store_and_publish(node, room, peer, LogKind::Nick, arg, &old);
            me
        }
        None => {
            let nick = if arg.is_empty() {
                default_nick(node, peer)
            } else {
                if let Err(e) = check_nick(node, peer, arg) {
                    return vec![err(&e)];
                }
                arg.to_string()
            };
            let me = Member {
                peer,
                nick,
                joined: true,
            };
            host.set_member(&me);
            let stored = store_and_publish(node, room, peer, LogKind::Join, &me.nick, "");
            if stored
                .first()
                .is_some_and(|r| r.starts_with("{\"t\":\"err\""))
            {
                return stored;
            }
            me
        }
    };
    replies.extend(history(&host.events_recent(HISTORY_DEFAULT)));
    let mut ready = String::new();
    let _ = write!(
        ready,
        "{{\"t\":\"ready\",\"room\":\"{}\",\"you\":{{\"peer\":\"{}\",\"nick\":\"",
        node.id().to_z32(),
        peer.to_z32()
    );
    protocols::json_escape_into(&mut ready, &you.nick);
    let _ = write!(ready, "\"}},\"last_seq\":{}}}", host.room_seq());
    replies.push(ready);
    replies
}

/// The nick a member gets when it joins without one: a previous nick if it is still free,
/// else a prefix of its EndpointId.
fn default_nick(node: &Shared, peer: EndpointId) -> String {
    if let Some(m) = node.host().member(peer)
        && check_nick(node, peer, &m.nick).is_ok()
    {
        return m.nick;
    }
    let z = peer.to_z32();
    let base = &z[..8];
    std::iter::once(base.to_string())
        .chain((2..).map(|i| format!("{base}-{i}")))
        .find(|n| check_nick(node, peer, n).is_ok())
        .unwrap_or_else(|| z[..NICK_MAX].to_string())
}

/// Checks that `nick` is valid and not used by another joined member.
fn check_nick(node: &Shared, peer: EndpointId, nick: &str) -> Result<(), String> {
    let valid = !nick.is_empty()
        && nick.len() <= NICK_MAX
        && nick
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if !valid {
        return Err(format!(
            "invalid nick: 1-{NICK_MAX} characters of A-Z a-z 0-9 _ . -"
        ));
    }
    let taken = node
        .host()
        .members()
        .iter()
        .any(|m| m.peer != peer && m.nick.eq_ignore_ascii_case(nick));
    if taken {
        return Err(format!("nick {nick} is taken"));
    }
    Ok(())
}

/// Stores an event, queues it for every other joined, present member, and returns the `ok`
/// reply.
fn store_and_publish(
    node: &Arc<Shared>,
    room: &mut Room,
    peer: EndpointId,
    kind: LogKind,
    nick: &str,
    text: &str,
) -> Vec<String> {
    let text = protocols::truncate(text, MAX_LOG_TEXT);
    let Some(row) = node.host().append(peer, kind, nick, text) else {
        return vec![err("storage error")];
    };
    let line = event_json(&row, false);
    let mut recipients = 0usize;
    for m in node.host().members() {
        if m.peer == peer || !node.has_socket(&m.peer) {
            continue;
        }
        recipients += 1;
        let queue = room.outbox.entry(m.peer);
        let start = matches!(queue, Entry::Vacant(_));
        queue.or_default().push(line.clone());
        if start {
            spawn_delivery(node, m.peer);
        }
    }
    info!(
        event = "room_event",
        peer = %peer.to_z32(),
        kind = kind.as_str(),
        seq = row.seq,
        recipients,
    );
    vec![format!(
        "{{\"t\":\"ok\",\"seq\":{},\"prev\":{}}}",
        row.seq, row.prev
    )]
}

/// Starts the delivery task for `peer`.
///
/// The task counts as a connection from the start, so the endpoint stays up through the batch
/// window and the dial.
fn spawn_delivery(node: &Arc<Shared>, peer: EndpointId) {
    node.connection_started();
    let n = node.clone();
    node.spawn(async move {
        n0_future::time::sleep(DELIVERY_BATCH).await;
        loop {
            let lines = {
                let mut room = n.room();
                match room.outbox.get_mut(&peer) {
                    Some(q) if !q.is_empty() => std::mem::take(q),
                    _ => {
                        room.outbox.remove(&peer);
                        break;
                    }
                }
            };
            let started = Instant::now();
            let (ok, via) = deliver(&n, peer, &lines).await;
            let ms = started.elapsed().as_millis() as u64;
            let (peer_z, events) = (peer.to_z32(), lines.len());
            if ok {
                info!(event = "room_delivery", peer = peer_z, events, via, ok, ms);
            } else {
                warn!(event = "room_delivery", peer = peer_z, events, via, ok, ms);
            }
            n.update_stats(|s| {
                if ok {
                    s.deliveries_ok += 1;
                } else {
                    s.deliveries_failed += 1;
                }
            });
        }
        n.connection_ended();
    });
}

/// Writes `lines` to `peer` on one uni stream; returns whether the stream was finished, and
/// the connection used: `open` (reused), `dial` (new) or `none` (no path).
async fn deliver(node: &Arc<Shared>, peer: EndpointId, lines: &[String]) -> (bool, &'static str) {
    let mut payload = String::new();
    for l in lines {
        payload.push_str(l);
        payload.push('\n');
    }
    let open = node.room().conns.get(&peer).cloned();
    if let Some((conn, activity)) = open
        && conn.close_reason().is_none()
    {
        if write_uni(&conn, &activity, &payload).await {
            node.update_stats(|s| s.delivery_reuses += 1);
            return (true, "open");
        }
        debug!(peer = %peer.fmt_short(), "delivery on open connection failed, dialing");
    }
    if !node.has_socket(&peer) {
        debug!(peer = %peer.fmt_short(), "delivery: no socket for peer");
        return (false, "none");
    }
    let Some(endpoint) = node.endpoint() else {
        return (false, "none");
    };
    let conn = match endpoint.connect(peer_endpoint_addr(peer), ROOM_ALPN).await {
        Ok(c) => c,
        Err(e) => {
            warn!(peer = %peer.fmt_short(), "room: dial failed: {e:#}");
            return (false, "dial");
        }
    };
    node.update_stats(|s| s.delivery_dials += 1);
    let activity = Activity::new();
    register(node, &conn, &activity);
    node.connection_started();
    let (n, c, a) = (node.clone(), conn.clone(), activity.clone());
    node.spawn(async move {
        protocols::serve(n.clone(), c, a, "out").await;
        n.connection_ended();
    });
    (write_uni(&conn, &activity, &payload).await, "dial")
}

async fn write_uni(conn: &Connection, activity: &Arc<Activity>, payload: &str) -> bool {
    let _open = activity.stream();
    let res = async {
        let mut s = conn.open_uni().await.map_err(|e| e.to_string())?;
        s.write_all(payload.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        s.finish().map_err(|e| e.to_string())
    }
    .await;
    if let Err(e) = &res {
        debug!(peer = %conn.remote_id().fmt_short(), "delivery stream: {e}");
    }
    res.is_ok()
}

fn history(rows: &[LogRow]) -> Vec<String> {
    rows.iter().map(|r| event_json(r, true)).collect()
}

fn who(node: &Shared) -> String {
    let mut out = String::from("{\"t\":\"who\",\"members\":[");
    for (i, m) in node.host().members().iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{{\"peer\":\"{}\",\"nick\":\"", m.peer.to_z32());
        protocols::json_escape_into(&mut out, &m.nick);
        let _ = write!(out, "\",\"present\":{}}}", node.has_socket(&m.peer));
    }
    out.push_str("]}");
    out
}

fn err(text: &str) -> String {
    let mut out = String::from("{\"t\":\"err\",\"text\":\"");
    protocols::json_escape_into(&mut out, text);
    out.push_str("\"}");
    out
}

/// Encodes a stored room event as note 008's JSON line.
fn event_json(r: &LogRow, hist: bool) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "{{\"t\":\"{}\",\"seq\":{},\"prev\":{},\"ts\":{},\"peer\":\"{}\"",
        r.kind, r.seq, r.prev, r.ts_ms, r.peer
    );
    let field = |out: &mut String, name: &str, value: &str| {
        let _ = write!(out, ",\"{name}\":\"");
        protocols::json_escape_into(out, value);
        out.push('"');
    };
    match r.kind.as_str() {
        "nick" => {
            field(&mut out, "old", &r.text);
            field(&mut out, "new", &r.nick);
        }
        "join" => field(&mut out, "nick", &r.nick),
        "part" => {
            field(&mut out, "nick", &r.nick);
            field(&mut out, "reason", &r.text);
        }
        _ => {
            field(&mut out, "nick", &r.nick);
            field(&mut out, "text", &r.text);
        }
    }
    if hist {
        out.push_str(",\"hist\":true");
    }
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, nick: &str, text: &str) -> LogRow {
        LogRow {
            seq: 7,
            prev: 5,
            ts_ms: 1000,
            peer: "p".into(),
            kind: kind.into(),
            nick: nick.into(),
            text: text.into(),
        }
    }

    /// Events encode as note 008 shows them.
    #[test]
    fn event_lines() {
        assert_eq!(
            event_json(&row("msg", "ana", "hi \"there\""), false),
            r#"{"t":"msg","seq":7,"prev":5,"ts":1000,"peer":"p","nick":"ana","text":"hi \"there\""}"#
        );
        assert_eq!(
            event_json(&row("join", "bo", ""), true),
            r#"{"t":"join","seq":7,"prev":5,"ts":1000,"peer":"p","nick":"bo","hist":true}"#
        );
        assert_eq!(
            event_json(&row("part", "bo", "bye"), false),
            r#"{"t":"part","seq":7,"prev":5,"ts":1000,"peer":"p","nick":"bo","reason":"bye"}"#
        );
        assert_eq!(
            event_json(&row("nick", "bob", "bo"), false),
            r#"{"t":"nick","seq":7,"prev":5,"ts":1000,"peer":"p","old":"bo","new":"bob"}"#
        );
        assert_eq!(err("x"), r#"{"t":"err","text":"x"}"#);
    }

    /// One line per stream, terminator optional, at most `MAX_REQUEST` bytes.
    #[test]
    fn request_parsing() {
        assert_eq!(parse_request(b"/who\n").unwrap(), "/who");
        assert_eq!(parse_request(b"/who\r\n").unwrap(), "/who");
        assert_eq!(parse_request(b"/who").unwrap(), "/who");
        assert!(parse_request(b"/who\n/who").is_err());
        let long = vec![b'a'; MAX_REQUEST + 1];
        assert!(parse_request(&long).is_err());
        assert!(parse_request(&vec![b'a'; MAX_REQUEST]).is_ok());
        assert!(parse_request(&[0xff]).is_err());
    }

    /// The window admits `RATE_LIMIT` requests.
    #[test]
    fn rate_window() {
        let mut room = Room::default();
        let peer = iroh::SecretKey::from_bytes(&[1; 32]).public();
        for _ in 0..RATE_LIMIT {
            assert!(allow(&mut room, peer));
        }
        assert!(!allow(&mut room, peer));
    }
}
