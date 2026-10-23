//! The chat room, `iroh-do/room/1` (design note 008).
//!
//! Requests arrive on bidirectional streams that members open, one request per stream.
//! Events are delivered on unidirectional streams that the app opens, on a connection to the
//! member that is already open in either direction or else on a new one dialed over the
//! member's relay socket.
//!
//! A request that stores an event writes it, and the member's new state with it, in one
//! transaction. The reply is `storage error` only if that transaction failed, so that a member
//! that sends the request again does not store it twice. Once the event is stored the reply
//! is `ok`, even if queueing it for the other members fails. Any other failure, such as a read
//! that fails, leaves it unknown whether the request took effect: the room then resets the
//! request stream instead of answering (`PROTOCOL.md`, "Not guaranteed").

use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    fmt::Write as _,
    sync::Arc,
    time::Duration,
};

use iroh::{
    EndpointId,
    endpoint::{Connection, RecvStream, SendStream, VarInt},
    protocol::{AcceptError, ProtocolHandler},
};
use iroh_do_core::{Activity, Sql, SqlError};
use n0_future::time::Instant;
use tracing::{debug, error, info, warn};

use crate::{
    Chat, DELIVERY_BATCH, LogKind, LogRow, Member, ROOM_ALPN, json_escape_into, parse_number, store,
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
/// Most event lines queued for one member.
///
/// Lines queue while the member's previous batch is being written, which a member that stops
/// reading holds up until the connection's `t_silent` closes it. Past the limit the oldest
/// line is dropped: the first line delivered then has a `prev` the member has not seen, and
/// one `/since` (at most [`SINCE_MAX`] events) fetches what it missed.
const OUTBOX_MAX: usize = SINCE_MAX;
/// Application error code of a request stream the room resets because handling the request
/// failed in a way that leaves its effect unknown.
const REQUEST_FAILED: VarInt = VarInt::from_u32(1);

/// In-memory room state: open connections, pending deliveries and request times.
#[derive(Default)]
pub(crate) struct Room {
    /// Open room connections by peer, whichever side dialed. The newest one wins.
    conns: HashMap<EndpointId, (Connection, Option<Arc<Activity>>)>,
    /// Event lines waiting for delivery, by recipient. An entry is present while that
    /// recipient's delivery task runs.
    outbox: HashMap<EndpointId, Outbox>,
    /// Numbers delivery tasks, so that a task touches only its own outbox.
    next_delivery: u64,
    /// Recent request times by peer, for the rate limit.
    requests: HashMap<EndpointId, VecDeque<Instant>>,
    /// When peers with no request inside the window were last forgotten.
    swept_at: Option<Instant>,
}

/// Event lines waiting for one recipient.
struct Outbox {
    /// The delivery task that owns this outbox.
    task: u64,
    /// At most [`OUTBOX_MAX`] lines.
    lines: VecDeque<String>,
    /// Lines dropped from the front since the last batch was taken.
    dropped: u64,
}

impl Outbox {
    /// Queues `line`, dropping the oldest line if the outbox is full.
    fn push(&mut self, line: String) {
        self.lines.push_back(line);
        if self.lines.len() > OUTBOX_MAX {
            self.lines.pop_front();
            self.dropped += 1;
        }
    }
}

/// Handling a request failed in a way that leaves it unknown whether the request took effect,
/// so the request is not answered. The cause has been logged.
#[derive(Debug)]
struct Unanswered;

type Handled = Result<Vec<String>, Unanswered>;

/// `iroh-do/room/1`.
#[derive(Debug, Clone)]
pub(crate) struct RoomProtocol(pub(crate) Arc<Chat>);

impl ProtocolHandler for RoomProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        serve_conn(self.0.clone(), conn).await;
        Ok(())
    }
}

/// Serves requests on a room connection, accepted or dialed, until it closes.
async fn serve_conn(chat: Arc<Chat>, conn: Connection) {
    let peer = conn.remote_id();
    let activity = chat.ctx.activity(&conn);
    chat.room()
        .conns
        .insert(peer, (conn.clone(), activity.clone()));
    while let Ok((send, recv)) = conn.accept_bi().await {
        let (c, a) = (chat.clone(), activity.clone());
        chat.ctx
            .spawn(async move { request_stream(c, peer, a, send, recv).await });
    }
    let mut room = chat.room();
    if let Entry::Occupied(e) = room.conns.entry(peer)
        && e.get().0.stable_id() == conn.stable_id()
    {
        e.remove();
    }
}

/// Reads one request, handles it and writes the reply, or resets the stream if the request
/// cannot be answered.
async fn request_stream(
    chat: Arc<Chat>,
    peer: EndpointId,
    activity: Option<Arc<Activity>>,
    mut send: SendStream,
    mut recv: RecvStream,
) {
    let _open = activity.as_ref().map(|a| a.stream());
    // Room for a `\r\n` terminator.
    let replies = match recv.read_to_end(MAX_REQUEST + 2).await {
        Ok(bytes) => match parse_request(&bytes) {
            Ok(line) => handle(&chat, peer, &line),
            Err(e) => Some(vec![err(e)]),
        },
        Err(e) => {
            debug!(peer = %peer.fmt_short(), "room request: {e}");
            Some(vec![err("request too long or unreadable")])
        }
    };
    if let Some(a) = &activity {
        a.touch();
    }
    let Some(replies) = replies else {
        let _ = send.reset(REQUEST_FAILED);
        return;
    };
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

/// Handles one request, logs it as a `room_request` event and returns the reply lines, or
/// `None` if the request is not answered.
///
/// The event records the request kind and the member's nick, never the text.
fn handle(chat: &Arc<Chat>, peer: EndpointId, line: &str) -> Option<Vec<String>> {
    let res = handle_request(chat, peer, line);
    let cmd = line.split(' ').next().unwrap_or_default();
    let kind = match cmd {
        "/join" | "/say" | "/me" | "/nick" | "/who" | "/history" | "/since" | "/quit" => &cmd[1..],
        _ => "unknown",
    };
    let Ok(replies) = res else {
        error!(event = "room_request_failed", peer = %peer.to_z32(), kind);
        return None;
    };
    let error = replies
        .first()
        .and_then(|r| r.strip_prefix("{\"t\":\"err\""))
        .is_some();
    let nick = chat
        .sql("member", |s| store::member(s, &peer))
        .flatten()
        .map(|m| m.nick)
        .unwrap_or_default();
    info!(event = "room_request", peer = %peer.to_z32(), kind, nick, ok = !error);
    Some(replies)
}

/// Runs a read for a request; a failure is logged and leaves the request unanswered.
fn read<T>(
    chat: &Chat,
    what: &'static str,
    f: impl FnOnce(&dyn Sql) -> Result<T, SqlError>,
) -> Result<T, Unanswered> {
    match chat.ctx.with_sql(f) {
        Some(Ok(v)) => Ok(v),
        Some(Err(e)) => {
            warn!(event = "chat_sql_failed", what, error = %e);
            Err(Unanswered)
        }
        None => {
            warn!(
                event = "chat_sql_failed",
                what,
                error = "the object has no database"
            );
            Err(Unanswered)
        }
    }
}

/// Handles one request and returns the reply lines.
///
/// Holding the room lock serializes requests, so checks such as nick uniqueness and the
/// writes that depend on them are atomic on multi-threaded hosts too.
fn handle_request(chat: &Arc<Chat>, peer: EndpointId, line: &str) -> Handled {
    let mut room = chat.room();
    if !allow(&mut room, peer, Instant::now()) {
        drop(room);
        chat.update_stats(|s| s.rate_limited += 1);
        return Ok(vec![err(&format!(
            "rate limit: at most {RATE_LIMIT} requests per {}s",
            RATE_WINDOW.as_secs()
        ))]);
    }
    let me = read(chat, "member", |s| store::member(s, &peer))?.filter(|m| m.joined);
    let (cmd, arg) = match line.split_once(' ') {
        Some((c, a)) => (c, a.trim_ascii()),
        None => (line, ""),
    };
    if cmd == "/join" {
        return join(chat, &mut room, peer, me, arg);
    }
    let Some(me) = me else {
        return Ok(vec![err("not joined: send /join [nick] first")]);
    };
    match cmd {
        "/say" | "/me" => {
            if arg.is_empty() {
                return Ok(vec![err(&format!("usage: {cmd} <text>"))]);
            }
            let kind = if cmd == "/say" {
                LogKind::Msg
            } else {
                LogKind::Act
            };
            Ok(store_and_publish(
                chat, &mut room, None, peer, kind, &me.nick, arg,
            ))
        }
        "/nick" => match check_nick(chat, peer, arg)? {
            Err(e) => Ok(vec![err(&e)]),
            Ok(()) if arg == me.nick => Ok(vec![err("nick unchanged")]),
            Ok(()) => {
                let old = me.nick.clone();
                let renamed = Member {
                    nick: arg.to_string(),
                    ..me
                };
                Ok(store_and_publish(
                    chat,
                    &mut room,
                    Some(&renamed),
                    peer,
                    LogKind::Nick,
                    arg,
                    &old,
                ))
            }
        },
        "/who" => Ok(vec![who(chat)?]),
        "/history" => {
            let n = if arg.is_empty() {
                HISTORY_DEFAULT
            } else {
                match parse_number(arg) {
                    Some(n) => usize::try_from(n).map_or(HISTORY_MAX, |n| n.min(HISTORY_MAX)),
                    None => {
                        return Ok(vec![err(&format!(
                            "usage: /history [n], n <= {HISTORY_MAX}"
                        ))]);
                    }
                }
            };
            Ok(history(&read(chat, "events_recent", |s| {
                store::events_recent(s, n)
            })?))
        }
        "/since" => match parse_number(arg) {
            Some(seq) => Ok(history(&read(chat, "events_after", |s| {
                store::events_after(s, seq, SINCE_MAX)
            })?)),
            None => Ok(vec![err("usage: /since <seq>")]),
        },
        "/quit" => {
            room.outbox.remove(&peer);
            let left = Member {
                joined: false,
                ..me.clone()
            };
            Ok(store_and_publish(
                chat,
                &mut room,
                Some(&left),
                peer,
                LogKind::Part,
                &me.nick,
                arg,
            ))
        }
        _ => Ok(vec![err("unknown request")]),
    }
}

/// Records a request from `peer` at `now`; returns whether it is within the rate limit.
///
/// Peers with no request inside the window are forgotten, at most once per window, so that
/// the limiter holds the peers of about the last two windows rather than every peer seen.
fn allow(room: &mut Room, peer: EndpointId, now: Instant) -> bool {
    if room
        .swept_at
        .is_none_or(|t| now.duration_since(t) >= RATE_WINDOW)
    {
        room.swept_at = Some(now);
        room.requests.retain(|_, times| {
            times
                .back()
                .is_some_and(|t| now.duration_since(*t) < RATE_WINDOW)
        });
    }
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
    chat: &Arc<Chat>,
    room: &mut Room,
    peer: EndpointId,
    me: Option<Member>,
    arg: &str,
) -> Handled {
    let you = match me {
        // Already joined: `/join <nick>` with another nick renames.
        Some(me) if arg.is_empty() || arg == me.nick => me,
        Some(me) => {
            if let Err(e) = check_nick(chat, peer, arg)? {
                return Ok(vec![err(&e)]);
            }
            let old = me.nick.clone();
            let me = Member {
                nick: arg.to_string(),
                ..me
            };
            let stored = store_and_publish(chat, room, Some(&me), peer, LogKind::Nick, arg, &old);
            if is_err(&stored) {
                return Ok(stored);
            }
            me
        }
        None => {
            let nick = if arg.is_empty() {
                default_nick(chat, peer)?
            } else {
                if let Err(e) = check_nick(chat, peer, arg)? {
                    return Ok(vec![err(&e)]);
                }
                arg.to_string()
            };
            let me = Member {
                peer,
                nick,
                joined: true,
            };
            let stored =
                store_and_publish(chat, room, Some(&me), peer, LogKind::Join, &me.nick, "");
            if is_err(&stored) {
                return Ok(stored);
            }
            me
        }
    };
    let (recent, room_seq) = read(chat, "join_reply", |s| {
        Ok((
            store::events_recent(s, HISTORY_DEFAULT)?,
            store::room_seq(s)?,
        ))
    })?;
    let mut replies = history(&recent);
    let mut ready = String::new();
    let _ = write!(
        ready,
        "{{\"t\":\"ready\",\"room\":\"{}\",\"you\":{{\"peer\":\"{}\",\"nick\":\"",
        chat.ctx.id().to_z32(),
        peer.to_z32()
    );
    json_escape_into(&mut ready, &you.nick);
    let _ = write!(ready, "\"}},\"last_seq\":{room_seq}}}");
    replies.push(ready);
    Ok(replies)
}

fn is_err(replies: &[String]) -> bool {
    replies
        .first()
        .is_some_and(|r| r.starts_with("{\"t\":\"err\""))
}

/// The nick a member gets when it joins without one: a previous nick if it is still free,
/// else a prefix of its EndpointId.
fn default_nick(chat: &Chat, peer: EndpointId) -> Result<String, Unanswered> {
    let previous = read(chat, "member", |s| store::member(s, &peer))?;
    let joined = members(chat)?;
    let free = |nick: &str| check_nick_among(&joined, peer, nick).is_ok();
    if let Some(m) = previous
        && free(&m.nick)
    {
        return Ok(m.nick);
    }
    let z = peer.to_z32();
    let base = &z[..8];
    Ok(std::iter::once(base.to_string())
        .chain((2..).map(|i| format!("{base}-{i}")))
        .find(|n| free(n))
        .unwrap_or_else(|| z[..NICK_MAX].to_string()))
}

/// Checks that `nick` is valid and not used by another joined member.
fn check_nick(chat: &Chat, peer: EndpointId, nick: &str) -> Result<Result<(), String>, Unanswered> {
    Ok(check_nick_among(&members(chat)?, peer, nick))
}

fn check_nick_among(joined: &[Member], peer: EndpointId, nick: &str) -> Result<(), String> {
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
    let taken = joined
        .iter()
        .any(|m| m.peer != peer && m.nick.eq_ignore_ascii_case(nick));
    if taken {
        return Err(format!("nick {nick} is taken"));
    }
    Ok(())
}

/// Stores an event, with `member`'s new state if given, queues it for every other joined,
/// present member, and returns the `ok` reply; `storage error` if nothing was stored.
fn store_and_publish(
    chat: &Arc<Chat>,
    room: &mut Room,
    member: Option<&Member>,
    peer: EndpointId,
    kind: LogKind,
    nick: &str,
    text: &str,
) -> Vec<String> {
    let row = match chat.append_with(member, peer, kind, nick, text) {
        Ok(row) => row,
        Err(e) => {
            error!(
                event = "room_request_failed",
                peer = %peer.to_z32(),
                kind = kind.as_str(),
                error = %e,
            );
            return vec![err("storage error")];
        }
    };
    let line = event_json(&row, false);
    // The event is stored: from here on the reply is `ok`. Members that miss the event because
    // queueing failed see the gap through `prev`.
    let recipients = match chat.ctx.with_sql(store::members) {
        Some(Ok(members)) => queue(chat, room, peer, &members, &line),
        Some(Err(e)) => {
            error!(event = "room_queue_failed", peer = %peer.to_z32(), seq = row.seq, error = %e);
            0
        }
        None => 0,
    };
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

/// Queues `line` for the joined, present `members` other than `author`; returns how many.
fn queue(
    chat: &Arc<Chat>,
    room: &mut Room,
    author: EndpointId,
    members: &[Member],
    line: &str,
) -> usize {
    let mut recipients = 0;
    for m in members {
        if m.peer == author || !chat.ctx.has_socket(&m.peer) {
            continue;
        }
        recipients += 1;
        match room.outbox.entry(m.peer) {
            Entry::Occupied(mut e) => e.get_mut().push(line.to_string()),
            Entry::Vacant(e) => {
                room.next_delivery += 1;
                let task = room.next_delivery;
                e.insert(Outbox {
                    task,
                    lines: VecDeque::from([line.to_string()]),
                    dropped: 0,
                });
                spawn_delivery(chat, m.peer, task);
            }
        }
    }
    recipients
}

/// Removes a delivery task's outbox when the task ends, however it ends: while the outbox is
/// registered, new events are only appended to it, so an outbox left behind would stop that
/// member's deliveries.
struct DeliveryGuard {
    chat: Arc<Chat>,
    peer: EndpointId,
    task: u64,
}

impl Drop for DeliveryGuard {
    fn drop(&mut self) {
        let mut room = self.chat.room();
        if let Entry::Occupied(e) = room.outbox.entry(self.peer)
            && e.get().task == self.task
        {
            e.remove();
        }
    }
}

/// Starts delivery task `task` for `peer`.
///
/// The endpoint stays bound while an app task runs, so it stays up through the batch window
/// and the dial.
fn spawn_delivery(chat: &Arc<Chat>, peer: EndpointId, task: u64) {
    let n = chat.clone();
    chat.ctx.spawn(async move {
        let _guard = DeliveryGuard {
            chat: n.clone(),
            peer,
            task,
        };
        n0_future::time::sleep(DELIVERY_BATCH).await;
        loop {
            let (lines, dropped) = {
                let mut room = n.room();
                match room.outbox.entry(peer) {
                    Entry::Occupied(mut e) if e.get().task == task => {
                        let o = e.get_mut();
                        if o.lines.is_empty() {
                            // Under the same lock as the check, so that no line is queued
                            // to an outbox that is about to go.
                            e.remove();
                            break;
                        }
                        (
                            o.lines.drain(..).collect::<Vec<_>>(),
                            std::mem::take(&mut o.dropped),
                        )
                    }
                    // Cancelled by `/quit`, or replaced by a later task's outbox.
                    _ => break,
                }
            };
            let started = Instant::now();
            let (ok, via) = deliver(&n, peer, &lines).await;
            let ms = started.elapsed().as_millis() as u64;
            let (peer_z, events) = (peer.to_z32(), lines.len());
            if ok {
                info!(
                    event = "room_delivery",
                    peer = peer_z,
                    events,
                    dropped,
                    via,
                    ok,
                    ms
                );
            } else {
                warn!(
                    event = "room_delivery",
                    peer = peer_z,
                    events,
                    dropped,
                    via,
                    ok,
                    ms
                );
            }
            n.update_stats(|s| {
                if ok {
                    s.deliveries_ok += 1;
                } else {
                    s.deliveries_failed += 1;
                }
            });
        }
    });
}

/// Writes `lines` to `peer` on one uni stream; returns whether the stream was finished, and
/// the connection used: `open` (reused), `dial` (new) or `none` (no path).
async fn deliver(chat: &Arc<Chat>, peer: EndpointId, lines: &[String]) -> (bool, &'static str) {
    let mut payload = String::new();
    for l in lines {
        payload.push_str(l);
        payload.push('\n');
    }
    let open = chat.room().conns.get(&peer).cloned();
    if let Some((conn, activity)) = open
        && conn.close_reason().is_none()
    {
        if write_uni(&conn, &activity, &payload).await {
            chat.update_stats(|s| s.delivery_reuses += 1);
            return (true, "open");
        }
        debug!(peer = %peer.fmt_short(), "delivery on open connection failed, dialing");
    }
    if !chat.ctx.has_socket(&peer) {
        debug!(peer = %peer.fmt_short(), "delivery: no socket for peer");
        return (false, "none");
    }
    let conn = match chat.ctx.connect(peer, ROOM_ALPN).await {
        Ok(c) => c,
        Err(e) => {
            warn!(peer = %peer.fmt_short(), "room: dial failed: {e:#}");
            return (false, "dial");
        }
    };
    chat.update_stats(|s| s.delivery_dials += 1);
    let activity = chat.ctx.activity(&conn);
    // The member may send requests on this connection too.
    let (c, cn) = (chat.clone(), conn.clone());
    chat.ctx.spawn(async move { serve_conn(c, cn).await });
    (write_uni(&conn, &activity, &payload).await, "dial")
}

async fn write_uni(conn: &Connection, activity: &Option<Arc<Activity>>, payload: &str) -> bool {
    let _open = activity.as_ref().map(|a| a.stream());
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

fn members(chat: &Chat) -> Result<Vec<Member>, Unanswered> {
    read(chat, "members", store::members)
}

fn history(rows: &[LogRow]) -> Vec<String> {
    rows.iter().map(|r| event_json(r, true)).collect()
}

fn who(chat: &Chat) -> Result<String, Unanswered> {
    let mut out = String::from("{\"t\":\"who\",\"members\":[");
    for (i, m) in members(chat)?.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{{\"peer\":\"{}\",\"nick\":\"", m.peer.to_z32());
        json_escape_into(&mut out, &m.nick);
        let _ = write!(out, "\",\"present\":{}}}", chat.ctx.has_socket(&m.peer));
    }
    out.push_str("]}");
    Ok(out)
}

fn err(text: &str) -> String {
    let mut out = String::from("{\"t\":\"err\",\"text\":\"");
    json_escape_into(&mut out, text);
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
        json_escape_into(out, value);
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

    /// An outbox holds the newest `OUTBOX_MAX` lines and counts those it dropped.
    #[test]
    fn outbox_drops_oldest() {
        let mut o = Outbox {
            task: 1,
            lines: VecDeque::new(),
            dropped: 0,
        };
        for i in 0..OUTBOX_MAX + 3 {
            o.push(i.to_string());
        }
        assert_eq!(o.lines.len(), OUTBOX_MAX);
        assert_eq!(o.dropped, 3);
        assert_eq!(o.lines.front().map(String::as_str), Some("3"));
    }

    /// The window admits `RATE_LIMIT` requests.
    #[test]
    fn rate_window() {
        let mut room = Room::default();
        let peer = iroh::SecretKey::from_bytes(&[1; 32]).public();
        let now = Instant::now();
        for _ in 0..RATE_LIMIT {
            assert!(allow(&mut room, peer, now));
        }
        assert!(!allow(&mut room, peer, now));
        assert!(allow(&mut room, peer, now + RATE_WINDOW));
    }

    /// A peer whose window has emptied is forgotten once another window has passed, so the
    /// limiter does not keep every peer it has seen.
    #[test]
    fn rate_limiter_forgets_quiet_peers() {
        let mut room = Room::default();
        let peers: Vec<EndpointId> = (1..=5u8)
            .map(|i| iroh::SecretKey::from_bytes(&[i; 32]).public())
            .collect();
        let t0 = Instant::now();
        for p in &peers {
            assert!(allow(&mut room, *p, t0));
        }
        assert_eq!(room.requests.len(), 5);
        let later = t0 + RATE_WINDOW + Duration::from_millis(1);
        assert!(allow(&mut room, peers[0], later));
        assert_eq!(room.requests.len(), 1, "quiet peers are forgotten");
        // At most one sweep per window: a peer seen since stays until the next one.
        assert!(allow(&mut room, peers[1], later + Duration::from_secs(1)));
        assert_eq!(room.requests.len(), 2);
    }
}
