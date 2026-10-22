//! The chat room app: a small chat room per endpoint (`iroh-do/room/1`, design note 008),
//! and the diagnostic protocols that exercise the lifecycle (`iroh-do/ctl/1`,
//! `iroh-do/echo/1`, `iroh-do/notify/1`, design notes 006 and 007).
//!
//! # The room
//!
//! Members do not hold connections, so that the object can hibernate between messages. A
//! member opens a connection to send a request, and the app opens one to deliver events,
//! each reusing an open connection to the other side if there is one. Both kinds close when
//! idle like any other connection, and the endpoint is dropped after the linger.
//!
//! A member is present while it has an authenticated relay socket to the object: that socket
//! is the only path the app can dial. The app delivers each new event to every joined,
//! present member except its author, batching events produced within [`DELIVERY_BATCH`] on
//! one unidirectional stream. A failed delivery is not retried; members catch up with
//! `/since`.
//!
//! # Idle close
//!
//! Every handler reports its open streams through [`iroh_do_core::Activity`], so a
//! connection with a stream open is closed only after `t_silent`, and one without after
//! `t_idle`. The lifecycle demo relies on this: its ctl session finishes its stream and
//! expects the close after `t_idle`.
#![deny(missing_docs)]

mod ctl;
mod room;
pub mod store;

use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use iroh_do_core::{App, Ctx, EndpointId, Protocols};
use tracing::warn;

/// ALPN of the echo protocol: each bi stream is echoed back.
pub const ECHO_ALPN: &[u8] = b"iroh-do/echo/1";
/// ALPN of the chat room: requests on member-opened bi streams, deliveries on app-opened
/// uni streams.
pub const ROOM_ALPN: &[u8] = b"iroh-do/room/1";
/// ALPN of the diagnostic protocol: one reply line per request line, plus `/status`,
/// `/count`, `/log` and `/callback`.
pub const CTL_ALPN: &[u8] = b"iroh-do/ctl/1";
/// ALPN the app dials a peer with for a dial-back.
pub const NOTIFY_ALPN: &[u8] = b"iroh-do/notify/1";
/// Largest delay, in seconds, a peer may request with `/callback`.
///
/// A dial-back needs the peer's relay socket, and iroh's relay client closes a socket to a
/// relay other than its home relay 60 s after its last send.
pub const MAX_CALLBACK_SECS: u64 = 50;
/// How long the app collects new room events for one member before opening a delivery
/// stream.
pub const DELIVERY_BATCH: Duration = Duration::from_millis(50);
/// Longest record text stored in the log, in bytes.
pub const MAX_LOG_TEXT: usize = 1024;
/// Prefix of the schedule keys of dial-backs; the rest is the peer's id in z-base-32.
const CALLBACK_KEY: &str = "callback:";
/// How long after its newest record expires a quiet log is pruned.
const PRUNE_SLACK: Duration = Duration::from_secs(60);

/// Kind of a message-log record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    /// A room message (`/say`).
    Msg,
    /// A room action (`/me`).
    Act,
    /// A member joined the room.
    Join,
    /// A member left the room (`/quit`).
    Part,
    /// A member changed its nick.
    Nick,
    /// A request line on [`CTL_ALPN`].
    Ctl,
    /// An echo stream.
    Echo,
    /// A dial-back attempt.
    Callback,
}

impl LogKind {
    /// Returns the name stored in a record's `kind` field.
    pub fn as_str(self) -> &'static str {
        match self {
            LogKind::Msg => "msg",
            LogKind::Act => "act",
            LogKind::Join => "join",
            LogKind::Part => "part",
            LogKind::Nick => "nick",
            LogKind::Ctl => "ctl",
            LogKind::Echo => "echo",
            LogKind::Callback => "callback",
        }
    }

    /// Returns whether records of this kind are room events.
    pub fn is_room_event(self) -> bool {
        matches!(
            self,
            LogKind::Msg | LogKind::Act | LogKind::Join | LogKind::Part | LogKind::Nick
        )
    }
}

/// A member of the room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The member's EndpointId.
    pub peer: EndpointId,
    /// Its nick: 1–24 characters of `[A-Za-z0-9_.-]`, unique among joined members ignoring
    /// ASCII case.
    pub nick: String,
    /// Whether it is joined. A member that sent `/quit` keeps its record, not joined.
    pub joined: bool,
}

/// One message-log record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRow {
    /// Sequence number.
    pub seq: u64,
    /// For a room event, the `seq` of the room event before it (0 if none); 0 for other
    /// records.
    ///
    /// Room events share `seq` with the diagnostic records, so consecutive room events need
    /// not have consecutive numbers. `prev` lets a member that last saw room event `L` tell a
    /// missed event (`prev > L`) from a diagnostic record in between.
    pub prev: u64,
    /// Insert time in milliseconds since the UNIX epoch.
    pub ts_ms: u64,
    /// The peer's EndpointId, z-base-32.
    pub peer: String,
    /// One of [`LogKind::as_str`].
    pub kind: String,
    /// The author's nick for room events (the new nick for `nick`), empty otherwise.
    pub nick: String,
    /// The message for `msg` and `act`, the reason for `part`, the old nick for `nick`, the
    /// request line for `ctl`, `"<n> bytes"` for `echo`, and `"delivered"` or
    /// `"failed: <reason>"` for `callback`.
    pub text: String,
}

/// Counters of the chat app since the object was constructed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatStats {
    /// Dial-backs whose notify stream was delivered.
    pub dial_backs_ok: u64,
    /// Dial-backs that failed: no socket, bind, connect or stream error.
    pub dial_backs_failed: u64,
    /// Room delivery streams written to members.
    pub deliveries_ok: u64,
    /// Room delivery streams that failed: no socket, connect or stream error.
    pub deliveries_failed: u64,
    /// Connections the app dialed to deliver room events.
    pub delivery_dials: u64,
    /// Delivery streams opened on a connection that was already open.
    pub delivery_reuses: u64,
    /// Room requests refused by the rate limit.
    pub rate_limited: u64,
}

/// State shared by the app and its protocol handlers.
pub(crate) struct Chat {
    pub(crate) ctx: Ctx,
    room: Mutex<room::Room>,
    stats: Mutex<ChatStats>,
}

impl std::fmt::Debug for Chat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chat").finish_non_exhaustive()
    }
}

impl Chat {
    pub(crate) fn room(&self) -> MutexGuard<'_, room::Room> {
        self.room.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn update_stats(&self, f: impl FnOnce(&mut ChatStats)) {
        f(&mut self.stats.lock().unwrap_or_else(|e| e.into_inner()));
    }

    /// Runs `f` on the database; logs an error and returns `None` if it fails or the
    /// database is gone.
    pub(crate) fn sql<T>(
        &self,
        what: &'static str,
        f: impl FnOnce(&dyn iroh_do_core::Sql) -> Result<T, iroh_do_core::SqlError>,
    ) -> Option<T> {
        match self.ctx.with_sql(f)? {
            Ok(v) => Some(v),
            Err(e) => {
                warn!(event = "chat_sql_failed", what, error = %e);
                None
            }
        }
    }

    /// Appends a record to the log, truncating `text` to [`MAX_LOG_TEXT`] bytes; returns the
    /// stored record.
    ///
    /// Inserting prunes the log. A log that stops growing is pruned once more when its newest
    /// record expires, from the alarm this schedules.
    pub(crate) fn append(
        &self,
        peer: EndpointId,
        kind: LogKind,
        nick: &str,
        text: &str,
    ) -> Option<LogRow> {
        let text = truncate(text, MAX_LOG_TEXT);
        let ts = store::now_ms();
        let z = peer.to_z32();
        let (seq, prev) = self.sql("log_insert", |s| {
            store::log_insert(s, ts, &z, kind, nick, text)
        })?;
        self.ctx
            .schedule(store::PRUNE_KEY, store::LOG_MAX_AGE + PRUNE_SLACK, "");
        Some(LogRow {
            seq,
            prev,
            ts_ms: u64::try_from(ts).unwrap_or(0),
            peer: z,
            kind: kind.as_str().to_string(),
            nick: nick.to_string(),
            text: text.to_string(),
        })
    }
}

/// The chat app.
pub struct ChatApp {
    chat: Arc<Chat>,
}

impl ChatApp {
    /// Returns the app's counters.
    pub fn stats(&self) -> ChatStats {
        self.chat
            .stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl App for ChatApp {
    const NAME: &'static str = "chat";

    fn new(ctx: &Ctx) -> Self {
        Self {
            chat: Arc::new(Chat {
                ctx: ctx.clone(),
                room: Mutex::new(room::Room::default()),
                stats: Mutex::new(ChatStats::default()),
            }),
        }
    }

    fn protocols(&self, _ctx: &Ctx, p: &mut Protocols) {
        p.accept(ROOM_ALPN, room::RoomProtocol(self.chat.clone()));
        p.accept(CTL_ALPN, ctl::Ctl(self.chat.clone()));
        p.accept(ECHO_ALPN, ctl::Echo(self.chat.clone()));
    }

    fn schema() -> &'static [&'static str] {
        store::SCHEMA
    }

    fn on_alarm(&self, ctx: &Ctx, key: &str, _payload: &str) {
        if let Some(peer) = key.strip_prefix(CALLBACK_KEY) {
            match EndpointId::from_z32(peer) {
                Ok(peer) => {
                    let chat = self.chat.clone();
                    ctx.spawn(async move { ctl::dial_notify(&chat, peer).await });
                }
                Err(_) => warn!(event = "bad_schedule_key", key),
            }
        } else if key == store::PRUNE_KEY {
            self.chat
                .sql("log_prune", |s| store::log_prune(s, store::now_ms()));
        }
    }

    fn status(&self, ctx: &Ctx) -> serde_json::Map<String, serde_json::Value> {
        let mut out = serde_json::Map::new();
        let records = self.chat.sql("log_total", store::log_total).unwrap_or(0);
        let members = self.chat.sql("members", store::members).unwrap_or_default();
        let present = members.iter().filter(|m| ctx.has_socket(&m.peer)).count();
        out.insert("records".into(), records.into());
        out.insert("members".into(), members.len().into());
        out.insert("present".into(), present.into());
        out
    }

    fn join_hint(id: &str) -> String {
        format!("iroh-do-chat {id}")
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

/// Appends `s` to `out` as the inside of a JSON string.
pub(crate) fn json_escape_into(out: &mut String, s: &str) {
    use std::fmt::Write as _;
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

    /// Quotes, backslashes and control characters are escaped.
    #[test]
    fn escape() {
        let mut s = String::new();
        json_escape_into(&mut s, "a\"b\\c\n");
        assert_eq!(s, "a\\\"b\\\\c\\u000a");
    }
}
