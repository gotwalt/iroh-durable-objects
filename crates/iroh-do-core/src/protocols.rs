//! The diagnostic protocols (ctl, echo, notify), connection dispatch and the per-connection
//! idle close. The room is in `room.rs`.

use std::{
    fmt::Write as _,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use iroh::{
    Endpoint, EndpointId,
    endpoint::{Connection, Incoming, RecvStream, SendStream, VarInt},
};
use iroh_ingress_transport::peer_endpoint_addr;
use n0_future::time::Instant;
use tracing::{debug, info, warn};

use crate::{
    CTL_ALPN, ECHO_ALPN, LogKind, MAX_CALLBACK_SECS, MAX_LOG_TEXT, NOTIFY_ALPN, ROOM_ALPN, Shared,
    room,
};

/// Default `n` of `/log [n]`.
const LOG_DEFAULT: usize = 5;
/// Largest `n` of `/log [n]`.
const LOG_MAX: usize = 50;

/// Application error code of the node's normal closes: idle, silent and end of dial-back.
pub(crate) const CLOSE_OK: VarInt = VarInt::from_u32(0);
/// Longest accepted ctl line in bytes, excluding the terminator.
const MAX_LINE: usize = 4096;
/// How long a dial-back waits for the peer to close the notify connection.
const NOTIFY_LINGER: Duration = Duration::from_secs(5);

/// Application activity on one connection, for the idle close.
pub(crate) struct Activity {
    open_streams: AtomicUsize,
    last: Mutex<Instant>,
}

impl Activity {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            open_streams: AtomicUsize::new(0),
            last: Mutex::new(Instant::now()),
        })
    }

    pub(crate) fn touch(&self) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    pub(crate) fn stream(self: &Arc<Self>) -> StreamGuard {
        self.open_streams.fetch_add(1, Ordering::SeqCst);
        self.touch();
        StreamGuard(self.clone())
    }

    /// Returns the time left until the connection is closed for inactivity, and the close
    /// reason: `idle` when no stream is open (limit `t_idle`), `silent` otherwise (limit
    /// `t_silent`).
    fn close_deadline(&self, t_idle: Duration, t_silent: Duration) -> (Duration, &'static [u8]) {
        let (limit, reason): (_, &'static [u8]) = if self.open_streams.load(Ordering::SeqCst) > 0 {
            (t_silent, b"silent")
        } else {
            (t_idle, b"idle")
        };
        let last = *self.last.lock().unwrap_or_else(|e| e.into_inner());
        (limit.saturating_sub(last.elapsed()), reason)
    }
}

/// Counts an open stream for the idle close until dropped.
pub(crate) struct StreamGuard(Arc<Activity>);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.0.touch();
        self.0.open_streams.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Accepts one incoming connection and serves it until it is closed.
pub(crate) async fn handle_incoming(node: Arc<Shared>, incoming: Incoming) {
    let accepting = match incoming.accept() {
        Ok(a) => a,
        Err(e) => {
            debug!("incoming refused: {e}");
            return;
        }
    };
    let conn = match accepting.await {
        Ok(c) => c,
        Err(e) => {
            debug!("handshake failed: {e}");
            return;
        }
    };
    serve(node, conn, Activity::new(), "in").await;
}

/// Dispatches a connection, accepted (`dir` is `in`) or dialed (`out`), by ALPN and enforces
/// the idle close.
///
/// Returns once the connection is closed.
pub(crate) async fn serve(
    node: Arc<Shared>,
    conn: Connection,
    activity: Arc<Activity>,
    dir: &'static str,
) {
    let peer = conn.remote_id();
    let started = Instant::now();
    let alpn = String::from_utf8_lossy(conn.alpn()).into_owned();
    let (n, c, a) = (node.clone(), conn.clone(), activity.clone());
    match conn.alpn() {
        ROOM_ALPN => node.spawn(async move { room::serve_conn(n, c, a).await }),
        CTL_ALPN => node.spawn(async move { ctl_conn(n, c, a).await }),
        ECHO_ALPN => node.spawn(async move { echo_conn(n, c, a).await }),
        _ => {
            warn!(event = "conn_refused", peer = %peer.to_z32(), alpn, dir);
            conn.close(VarInt::from_u32(1), b"unsupported alpn");
            return;
        }
    }
    info!(event = "conn_open", peer = %peer.to_z32(), alpn, dir);
    let (t_idle, t_silent) = (node.config().t_idle, node.config().t_silent);
    let idle = async {
        loop {
            match activity.close_deadline(t_idle, t_silent) {
                (d, reason) if d.is_zero() => {
                    conn.close(CLOSE_OK, reason);
                    node.record_idle_close();
                    conn.closed().await;
                    return String::from_utf8_lossy(reason).into_owned();
                }
                // Re-checked on wake: a stream opening or closing switches the limit.
                (d, _) => n0_future::time::sleep(d.min(t_idle.min(t_silent))).await,
            }
        }
    };
    let closed = async { conn.closed().await.to_string() };
    let reason = n0_future::future::race(idle, closed).await;
    info!(
        event = "conn_close",
        peer = %peer.to_z32(),
        alpn,
        dir,
        reason,
        ms = started.elapsed().as_millis() as u64,
    );
}

async fn echo_conn(node: Arc<Shared>, conn: Connection, activity: Arc<Activity>) {
    while let Ok((send, recv)) = conn.accept_bi().await {
        let (n, a, peer) = (node.clone(), activity.clone(), conn.remote_id());
        node.spawn(async move {
            let bytes = echo_stream(a, send, recv).await;
            n.host()
                .log_event(peer, LogKind::Echo, &format!("{bytes} bytes"));
        });
    }
}

/// Echoes one stream; returns the number of bytes echoed.
async fn echo_stream(activity: Arc<Activity>, mut send: SendStream, mut recv: RecvStream) -> u64 {
    let _open = activity.stream();
    let mut buf = vec![0u8; 16 * 1024];
    let mut total = 0u64;
    loop {
        match recv.read(&mut buf).await {
            Ok(Some(n)) => {
                activity.touch();
                if send.write_all(&buf[..n]).await.is_err() {
                    return total;
                }
                total += n as u64;
            }
            Ok(None) => break,
            Err(e) => {
                debug!("echo read: {e}");
                return total;
            }
        }
    }
    let _ = send.finish();
    total
}

async fn ctl_conn(node: Arc<Shared>, conn: Connection, activity: Arc<Activity>) {
    let hello_sent = Arc::new(AtomicBool::new(false));
    while let Ok((send, recv)) = conn.accept_bi().await {
        let (n, c, a, h) = (
            node.clone(),
            conn.clone(),
            activity.clone(),
            hello_sent.clone(),
        );
        node.spawn(async move { ctl_stream(n, c, a, h, send, recv).await });
    }
}

async fn ctl_stream(
    node: Arc<Shared>,
    conn: Connection,
    activity: Arc<Activity>,
    hello_sent: Arc<AtomicBool>,
    mut send: SendStream,
    recv: RecvStream,
) {
    let _open = activity.stream();
    let mut lines = LineReader::new(recv);
    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => break,
            Err(e) => {
                debug!("ctl read: {e}");
                let _ = send.write_all(format!("error: {e}\n").as_bytes()).await;
                break;
            }
        };
        activity.touch();
        let mut reply = respond(&node, &conn, &line);
        reply.push('\n');
        if send.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
        if !hello_sent.swap(true, Ordering::SeqCst) {
            let (c, a, me) = (conn.clone(), activity.clone(), node.id());
            node.spawn(async move { send_hello(c, a, me).await });
        }
    }
    let _ = send.finish();
}

/// Opens a uni stream to the peer and writes one greeting line.
///
/// Sent once per ctl connection without a request, to show that the object can open
/// streams on a connection the peer dialed.
async fn send_hello(conn: Connection, activity: Arc<Activity>, me: EndpointId) {
    let _open = activity.stream();
    let line = format!(
        "hello from {}: you are {}\n",
        me.to_z32(),
        conn.remote_id().to_z32()
    );
    let mut s = match conn.open_uni().await {
        Ok(s) => s,
        Err(e) => {
            debug!("hello open_uni: {e}");
            return;
        }
    };
    if let Err(e) = s.write_all(line.as_bytes()).await {
        debug!("hello write: {e}");
        return;
    }
    let _ = s.finish();
}

/// Returns the reply line, without `\n`, to one ctl request line.
fn respond(node: &Arc<Shared>, conn: &Connection, line: &str) -> String {
    let peer = conn.remote_id();
    if line == "/status" {
        return status_json(node);
    }
    if line == "/count" {
        let c = node.host().count(peer);
        return format!(
            "{{\"total\":{},\"yours\":{},\"peers\":{}}}",
            c.total, c.yours, c.peers
        );
    }
    if let Some(arg) = command_arg(line, "/log") {
        let n = if arg.is_empty() {
            LOG_DEFAULT
        } else {
            match arg.parse::<usize>() {
                Ok(n) => n.min(LOG_MAX),
                Err(_) => return format!("error: usage /log [n], n <= {LOG_MAX}"),
            }
        };
        return log_json(&node.host().recent(n));
    }
    if let Some(arg) = command_arg(line, "/callback") {
        let secs = match arg.parse::<u64>() {
            Ok(s) if s <= MAX_CALLBACK_SECS => s,
            _ => return format!("error: usage /callback <secs>, secs <= {MAX_CALLBACK_SECS}"),
        };
        node.host()
            .schedule_callback(peer, Duration::from_secs(secs));
        return format!("ok: callback in {secs}s");
    }
    let stats = node
        .host()
        .log_event(peer, LogKind::Ctl, truncate(line, MAX_LOG_TEXT));
    format!(
        "echo: {line} (#{}, total {}, yours {})",
        stats.seq, stats.total, stats.yours
    )
}

/// Returns the trimmed argument if `line` is `cmd` or `cmd <arg>`.
pub(crate) fn command_arg<'a>(line: &'a str, cmd: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(cmd)?;
    if rest.is_empty() {
        Some("")
    } else if rest.starts_with(' ') {
        Some(rest.trim())
    } else {
        None
    }
}

/// Returns the longest prefix of `s` that is at most `max` bytes and ends on a char boundary.
pub(crate) fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn log_json(rows: &[crate::LogRow]) -> String {
    let mut out = String::from("[");
    for (i, r) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{{\"seq\":{},\"ts_ms\":{},\"peer\":\"", r.seq, r.ts_ms);
        json_escape_into(&mut out, &r.peer);
        out.push_str("\",\"kind\":\"");
        json_escape_into(&mut out, &r.kind);
        out.push_str("\",\"text\":\"");
        json_escape_into(&mut out, &r.text);
        out.push_str("\"}");
    }
    out.push(']');
    out
}

pub(crate) fn status_json(node: &Arc<Shared>) -> String {
    let stats = node.stats();
    let bundle: String = node.config().bundle.chars().take(12).collect();
    let mut out = String::new();
    let _ = write!(
        out,
        "{{\"id\":\"{}\",\"state\":\"{}\",\"wakes\":{},\"endpoint_builds\":{},\"connections\":{},\"bundle\":\"",
        node.id().to_z32(),
        node.state().as_str(),
        node.config().wakes,
        node.config().endpoint_builds + stats.endpoint_builds,
        stats.connections,
    );
    json_escape_into(&mut out, &bundle);
    let (members, present) = room::member_counts(node);
    let _ = write!(
        out,
        "\",\"since_wake_ms\":{},\"records\":{},\"members\":{members},\"present\":{present}}}",
        node.since_wake().as_millis(),
        node.host().total()
    );
    out
}

pub(crate) fn json_escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

/// Takes the next complete line (without `\n` / `\r\n`) from `buf`; at end of stream a final
/// unterminated line counts as complete. Lines longer than [`MAX_LINE`] bytes are an error,
/// whichever read delivered their terminator.
fn take_line(buf: &mut Vec<u8>, eof: bool) -> Result<Option<String>, String> {
    let end = match buf.iter().position(|&b| b == b'\n') {
        Some(pos) => pos,
        // Room for a `\r` that may precede the terminator still to come.
        None if buf.len() > MAX_LINE + 1 => return Err("line too long".to_string()),
        None if eof && !buf.is_empty() => buf.len(),
        None => return Ok(None),
    };
    let mut line: Vec<u8> = buf.drain(..end).collect();
    if !buf.is_empty() {
        buf.remove(0); // the `\n`
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    if line.len() > MAX_LINE {
        return Err("line too long".to_string());
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|_| "line is not utf-8".to_string())
}

/// Reads `\n`-terminated UTF-8 lines from a stream.
struct LineReader {
    recv: RecvStream,
    buf: Vec<u8>,
    eof: bool,
}

impl LineReader {
    fn new(recv: RecvStream) -> Self {
        Self {
            recv,
            buf: Vec::new(),
            eof: false,
        }
    }

    /// Next line without its terminator (`\n` or `\r\n`); a final unterminated line is
    /// returned at end of stream.
    async fn next_line(&mut self) -> Result<Option<String>, String> {
        let mut chunk = [0u8; 1024];
        loop {
            if let Some(line) = take_line(&mut self.buf, self.eof)? {
                return Ok(Some(line));
            }
            if self.eof {
                return Ok(None);
            }
            match self.recv.read(&mut chunk).await {
                Ok(Some(n)) => self.buf.extend_from_slice(&chunk[..n]),
                Ok(None) => self.eof = true,
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

/// Logs the outcome of one dial-back attempt, `delivered` or `failed: <reason>`.
///
/// Every attempt is logged, including those that fail before dialing.
pub(crate) fn log_callback(node: &Shared, peer: EndpointId, outcome: &str) {
    node.host()
        .log_event(peer, LogKind::Callback, truncate(outcome, MAX_LOG_TEXT));
}

/// Dials `peer` with the notify ALPN over its relay socket and writes one line on a uni
/// stream; returns whether the line was delivered.
///
/// Waits up to [`NOTIFY_LINGER`] for the peer to close the connection, then closes it.
pub(crate) async fn dial_notify(node: &Arc<Shared>, endpoint: &Endpoint, peer: EndpointId) -> bool {
    let started = Instant::now();
    let conn = match endpoint
        .connect(peer_endpoint_addr(peer), NOTIFY_ALPN)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            warn!(
                event = "dial_back",
                peer = %peer.to_z32(),
                ok = false,
                ms = started.elapsed().as_millis() as u64,
                error = format!("connect: {e:#}"),
            );
            log_callback(node, peer, &format!("failed: {e}"));
            return false;
        }
    };
    let alpn = String::from_utf8_lossy(NOTIFY_ALPN);
    info!(event = "conn_open", peer = %peer.to_z32(), alpn = %alpn, dir = "out");
    let line = format!(
        "callback from {} wakes={} records={}\n",
        node.id().to_z32(),
        node.config().wakes,
        node.host().total()
    );
    let sent = async {
        let mut s = conn.open_uni().await.map_err(|e| e.to_string())?;
        s.write_all(line.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        s.finish().map_err(|e| e.to_string())
    }
    .await;
    match &sent {
        Ok(()) => info!(
            event = "dial_back",
            peer = %peer.to_z32(),
            ok = true,
            ms = started.elapsed().as_millis() as u64,
        ),
        Err(e) => warn!(
            event = "dial_back",
            peer = %peer.to_z32(),
            ok = false,
            ms = started.elapsed().as_millis() as u64,
            error = format!("stream: {e}"),
        ),
    }
    let outcome = match &sent {
        Ok(()) => "delivered".to_string(),
        Err(e) => format!("failed: {e}"),
    };
    log_callback(node, peer, &outcome);
    let wait_close = async {
        conn.closed().await;
    };
    let timeout = n0_future::time::sleep(NOTIFY_LINGER);
    n0_future::future::race(wait_close, timeout).await;
    conn.close(CLOSE_OK, b"done");
    info!(
        event = "conn_close",
        peer = %peer.to_z32(),
        alpn = %alpn,
        dir = "out",
        reason = %conn.close_reason().map_or_else(|| "done".to_owned(), |r| r.to_string()),
        ms = started.elapsed().as_millis() as u64,
    );
    sent.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Truncation never splits a UTF-8 character.
    #[test]
    fn truncate_on_char_boundary() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("héllo", 2), "h");
        assert_eq!(truncate("héllo", 3), "hé");
    }

    /// A command matches only as a whole word, with or without an argument.
    #[test]
    fn command_args() {
        assert_eq!(command_arg("/log", "/log"), Some(""));
        assert_eq!(command_arg("/log 7", "/log"), Some("7"));
        assert_eq!(command_arg("/logx", "/log"), None);
        assert_eq!(command_arg("hello", "/log"), None);
    }

    /// [`MAX_LINE`] holds however the line and its terminator are split across reads.
    #[test]
    fn line_limit_applies_to_returned_lines() {
        // The terminator arrives in the same chunk that crosses MAX_LINE.
        let mut buf = vec![b'a'; 4500];
        buf.push(b'\n');
        assert_eq!(take_line(&mut buf, false), Err("line too long".to_string()));

        let mut buf = vec![b'a'; MAX_LINE];
        buf.extend_from_slice(b"\r\nnext");
        assert_eq!(take_line(&mut buf, false).unwrap().unwrap().len(), MAX_LINE);
        assert_eq!(buf, b"next");
        assert_eq!(take_line(&mut buf, false), Ok(None));
        assert_eq!(take_line(&mut buf, true), Ok(Some("next".to_string())));
        assert_eq!(take_line(&mut buf, true), Ok(None));

        // No terminator yet: waits up to MAX_LINE + 1 bytes (a trailing `\r`), then errors.
        let mut buf = vec![b'a'; MAX_LINE + 1];
        assert_eq!(take_line(&mut buf, false), Ok(None));
        buf.push(b'a');
        assert_eq!(take_line(&mut buf, false), Err("line too long".to_string()));

        let mut buf = vec![0xff, b'\n'];
        assert_eq!(
            take_line(&mut buf, false),
            Err("line is not utf-8".to_string())
        );
    }

    /// Quotes, backslashes and control characters are escaped.
    #[test]
    fn escape() {
        let mut s = String::new();
        json_escape_into(&mut s, "a\"b\\c\n");
        assert_eq!(s, "a\\\"b\\\\c\\u000a");
    }
}
