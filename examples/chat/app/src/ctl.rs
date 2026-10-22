//! The diagnostic protocols: ctl, echo and notify (design notes 006 and 007).

use std::{
    fmt::Write as _,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use iroh::{
    EndpointId,
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler},
};
use iroh_do_core::{Activity, CLOSE_OK};
use n0_future::time::Instant;
use tracing::{debug, info, warn};

use crate::{
    CALLBACK_KEY, Chat, LogKind, LogRow, MAX_CALLBACK_SECS, NOTIFY_ALPN, parse_number, store,
};

/// Default `n` of `/log [n]`.
const LOG_DEFAULT: usize = 5;
/// Largest `n` of `/log [n]`.
const LOG_MAX: usize = 50;
/// Longest accepted ctl line in bytes, excluding the terminator.
const MAX_LINE: usize = 4096;
/// How long a dial-back waits for the peer to close the notify connection.
const NOTIFY_LINGER: Duration = Duration::from_secs(5);

/// `iroh-do/echo/1`: each bi stream is echoed back.
#[derive(Debug, Clone)]
pub(crate) struct Echo(pub(crate) Arc<Chat>);

impl ProtocolHandler for Echo {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let activity = self.0.ctx.activity(&conn);
        while let Ok((send, recv)) = conn.accept_bi().await {
            let (chat, a, peer) = (self.0.clone(), activity.clone(), conn.remote_id());
            self.0.ctx.spawn(async move {
                let bytes = echo_stream(a, send, recv).await;
                chat.append(peer, LogKind::Echo, "", &format!("{bytes} bytes"));
            });
        }
        Ok(())
    }
}

/// Echoes one stream; returns the number of bytes echoed.
async fn echo_stream(
    activity: Option<Arc<Activity>>,
    mut send: SendStream,
    mut recv: RecvStream,
) -> u64 {
    let _open = activity.as_ref().map(|a| a.stream());
    let mut buf = vec![0u8; 16 * 1024];
    let mut total = 0u64;
    loop {
        match recv.read(&mut buf).await {
            Ok(Some(n)) => {
                touch(&activity);
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

fn touch(activity: &Option<Arc<Activity>>) {
    if let Some(a) = activity {
        a.touch();
    }
}

/// `iroh-do/ctl/1`: one reply line per request line.
#[derive(Debug, Clone)]
pub(crate) struct Ctl(pub(crate) Arc<Chat>);

impl ProtocolHandler for Ctl {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let activity = self.0.ctx.activity(&conn);
        let hello_sent = Arc::new(AtomicBool::new(false));
        while let Ok((send, recv)) = conn.accept_bi().await {
            let (chat, c, a, h) = (
                self.0.clone(),
                conn.clone(),
                activity.clone(),
                hello_sent.clone(),
            );
            self.0
                .ctx
                .spawn(async move { ctl_stream(chat, c, a, h, send, recv).await });
        }
        Ok(())
    }
}

async fn ctl_stream(
    chat: Arc<Chat>,
    conn: Connection,
    activity: Option<Arc<Activity>>,
    hello_sent: Arc<AtomicBool>,
    mut send: SendStream,
    recv: RecvStream,
) {
    let _open = activity.as_ref().map(|a| a.stream());
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
        touch(&activity);
        let mut reply = respond(&chat, &conn, &line);
        reply.push('\n');
        if send.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
        if !hello_sent.swap(true, Ordering::SeqCst) {
            let (c, a, me) = (conn.clone(), activity.clone(), chat.ctx.id());
            chat.ctx.spawn(async move { send_hello(c, a, me).await });
        }
    }
    let _ = send.finish();
}

/// Opens a uni stream to the peer and writes one greeting line.
///
/// Sent once per ctl connection without a request, to show that the object can open
/// streams on a connection the peer dialed.
async fn send_hello(conn: Connection, activity: Option<Arc<Activity>>, me: EndpointId) {
    let _open = activity.as_ref().map(|a| a.stream());
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
fn respond(chat: &Arc<Chat>, conn: &Connection, line: &str) -> String {
    let peer = conn.remote_id();
    let z = peer.to_z32();
    if line == "/status" {
        return chat.ctx.status().to_string();
    }
    if line == "/count" {
        let counts = chat.sql("count", |s| {
            Ok((
                store::log_total(s)?,
                store::log_yours(s, &z)?,
                store::log_peers(s)?,
            ))
        });
        let (total, yours, peers) = counts.unwrap_or_default();
        return format!("{{\"total\":{total},\"yours\":{yours},\"peers\":{peers}}}");
    }
    if let Some(arg) = command_arg(line, "/log") {
        let n = if arg.is_empty() {
            LOG_DEFAULT
        } else {
            match parse_number(arg) {
                Some(n) => usize::try_from(n).map_or(LOG_MAX, |n| n.min(LOG_MAX)),
                None => return format!("error: usage /log [n], n <= {LOG_MAX}"),
            }
        };
        let rows = chat
            .sql("log_recent", |s| store::log_recent(s, n))
            .unwrap_or_default();
        return log_json(&rows);
    }
    if let Some(arg) = command_arg(line, "/callback") {
        let secs = match parse_number(arg) {
            Some(s) if s <= MAX_CALLBACK_SECS => s,
            _ => return format!("error: usage /callback <secs>, secs <= {MAX_CALLBACK_SECS}"),
        };
        chat.ctx
            .schedule(&format!("{CALLBACK_KEY}{z}"), Duration::from_secs(secs), "");
        return format!("ok: callback in {secs}s");
    }
    let seq = chat
        .append(peer, LogKind::Ctl, "", line)
        .map_or(0, |r| r.seq);
    let yours = chat
        .sql("log_yours", |s| store::log_yours(s, &z))
        .unwrap_or(0);
    // `seq` numbers every record ever inserted, so it is also the total.
    format!("echo: {line} (#{seq}, total {seq}, yours {yours})")
}

/// Returns the trimmed argument if `line` is `cmd` or `cmd <arg>`.
pub(crate) fn command_arg<'a>(line: &'a str, cmd: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(cmd)?;
    if rest.is_empty() {
        Some("")
    } else if rest.starts_with(' ') {
        Some(rest.trim_ascii())
    } else {
        None
    }
}

fn log_json(rows: &[LogRow]) -> String {
    let mut out = String::from("[");
    for (i, r) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{{\"seq\":{},\"ts_ms\":{},\"peer\":\"", r.seq, r.ts_ms);
        crate::json_escape_into(&mut out, &r.peer);
        out.push_str("\",\"kind\":\"");
        crate::json_escape_into(&mut out, &r.kind);
        out.push_str("\",\"text\":\"");
        crate::json_escape_into(&mut out, &r.text);
        out.push_str("\"}");
    }
    out.push(']');
    out
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

/// Dials `peer` with the notify ALPN over its relay socket and writes one line on a uni
/// stream. Every attempt is logged, including those that fail before dialing.
///
/// Waits up to [`NOTIFY_LINGER`] for the peer to close the connection, then closes it.
pub(crate) async fn dial_notify(chat: &Arc<Chat>, peer: EndpointId) {
    let started = Instant::now();
    let fail = |chat: &Chat, error: String| {
        warn!(
            event = "dial_back",
            peer = %peer.to_z32(),
            ok = false,
            ms = started.elapsed().as_millis() as u64,
            error,
        );
        chat.update_stats(|s| s.dial_backs_failed += 1);
    };
    let conn = match chat.ctx.connect(peer, NOTIFY_ALPN).await {
        Ok(c) => c,
        Err(e) => {
            fail(chat, format!("connect: {e}"));
            chat.append(peer, LogKind::Callback, "", &format!("failed: {e}"));
            return;
        }
    };
    let records = chat.sql("log_total", store::log_total).unwrap_or(0);
    let line = format!(
        "callback from {} wakes={} records={records}\n",
        chat.ctx.id().to_z32(),
        chat.ctx.wakes(),
    );
    let sent = async {
        let mut s = conn.open_uni().await.map_err(|e| e.to_string())?;
        s.write_all(line.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        s.finish().map_err(|e| e.to_string())
    }
    .await;
    let outcome = match &sent {
        Ok(()) => {
            info!(
                event = "dial_back",
                peer = %peer.to_z32(),
                ok = true,
                ms = started.elapsed().as_millis() as u64,
            );
            chat.update_stats(|s| s.dial_backs_ok += 1);
            "delivered".to_string()
        }
        Err(e) => {
            fail(chat, format!("stream: {e}"));
            format!("failed: {e}")
        }
    };
    chat.append(peer, LogKind::Callback, "", &outcome);
    let wait_close = async {
        conn.closed().await;
    };
    let timeout = n0_future::time::sleep(NOTIFY_LINGER);
    n0_future::future::race(wait_close, timeout).await;
    conn.close(CLOSE_OK, b"done");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command matches only as a whole word, with or without an argument.
    #[test]
    fn command_args() {
        assert_eq!(command_arg("/log", "/log"), Some(""));
        assert_eq!(command_arg("/log 7", "/log"), Some("7"));
        assert_eq!(command_arg("/logx", "/log"), None);
        assert_eq!(command_arg("hello", "/log"), None);
        // Only ASCII white space is trimmed.
        assert_eq!(command_arg("/log \t7 ", "/log"), Some("7"));
        assert_eq!(command_arg("/log \u{a0}7", "/log"), Some("\u{a0}7"));
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
}
