//! Client side of `iroh-do/ctl/1`, the diagnostic protocol (notes 006, 007, 008).

use std::time::{Duration, Instant};

use iroh::endpoint::{Connection, RecvStream, SendStream};
use n0_error::{Result, StdResultExt, anyerr, bail_any};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::mpsc,
};

/// How long to wait for one reply line.
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);

/// The `/status` reply (note 006). Unknown fields are ignored; missing ones default.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Status {
    /// The DO's endpoint id (z32).
    pub id: String,
    /// Lifecycle state as the DO sees it (`Active` while answering).
    pub state: String,
    /// DO constructor runs (persisted): an increase proves eviction/hibernation.
    pub wakes: u64,
    /// Endpoint instantiations: an increase proves Active -> Warm -> Active.
    pub endpoint_builds: u64,
    /// Live QUIC connections.
    pub connections: u64,
    /// Prefix of the deployed bundle's SHA-256.
    pub bundle: String,
    /// Milliseconds since the DO isolate last woke.
    pub since_wake_ms: u64,
    /// Total rows ever written to the DO's SQLite log (note 007); `None` from
    /// DO builds that predate the log.
    pub records: Option<u64>,
}

impl Status {
    /// Parses one `/status` line.
    pub fn parse(line: &str) -> Result<Self> {
        serde_json::from_str(line).std_context("parsing /status JSON")
    }

    /// One-line summary.
    pub fn summary(&self) -> String {
        format!(
            "state={} wakes={} endpoint_builds={} connections={} since_wake={}ms bundle={}",
            self.state,
            self.wakes,
            self.endpoint_builds,
            self.connections,
            self.since_wake_ms,
            self.bundle
        ) + &self
            .records
            .map(|r| format!(" records={r}"))
            .unwrap_or_default()
    }
}

/// The `/count` reply (note 007).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Counts {
    /// Rows ever written (monotonic).
    pub total: u64,
    /// Rows written for the asking peer.
    pub yours: u64,
    /// Distinct peers in the retained rows.
    pub peers: u64,
}

impl Counts {
    /// Parses one `/count` line.
    pub fn parse(line: &str) -> Result<Self> {
        serde_json::from_str(line).std_context("parsing /count JSON")
    }
}

/// One row of the `/log` reply (note 007).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct LogRow {
    /// Monotonic row id.
    pub seq: u64,
    /// Insert time, ms since the Unix epoch (DO clock).
    pub ts_ms: i64,
    /// Peer EndpointId (z32).
    pub peer: String,
    /// `ctl`, `echo`, `callback`, or a room event kind (`msg`, `act`, `join`, `part`, `nick`).
    pub kind: String,
    /// Logged text.
    pub text: String,
}

/// Parses a `/log` reply (JSON array, oldest first).
pub fn parse_log(line: &str) -> Result<Vec<LogRow>> {
    serde_json::from_str(line).std_context("parsing /log JSON")
}

/// Formats log rows as an aligned table: seq, local time, short peer, kind, text.
pub fn format_log(rows: &[LogRow]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if rows.is_empty() {
        out.push_str("   (no rows)\n");
        return out;
    }
    let _ = writeln!(
        out,
        "{:>7}  {:<19}  {:<10}  {:<8}  text",
        "seq", "time (local)", "peer", "kind"
    );
    for r in rows {
        let time = chrono::DateTime::from_timestamp_millis(r.ts_ms)
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_else(|| r.ts_ms.to_string());
        let peer: String = r.peer.chars().take(10).collect();
        let _ = writeln!(
            out,
            "{:>7}  {:<19}  {:<10}  {:<8}  {}",
            r.seq, time, peer, r.kind, r.text
        );
    }
    out
}

/// An open ctl control stream.
pub struct CtlSession {
    /// The underlying connection.
    pub conn: Connection,
    send: SendStream,
    recv: BufReader<RecvStream>,
    unprompted: mpsc::UnboundedReceiver<String>,
    /// When the last application bytes were exchanged.
    pub last_activity: Instant,
}

impl CtlSession {
    /// Opens the control stream and starts accepting the DO's unprompted uni streams.
    pub async fn open(conn: Connection) -> Result<Self> {
        let (send, recv) = conn.open_bi().await.anyerr()?;
        let (tx, unprompted) = mpsc::unbounded_channel();
        let c = conn.clone();
        tokio::spawn(async move {
            while let Ok(mut uni) = c.accept_uni().await {
                match uni.read_to_end(64 * 1024).await {
                    Ok(data) => {
                        let line = String::from_utf8_lossy(&data).trim_end().to_string();
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => continue,
                }
            }
        });
        Ok(Self {
            conn,
            send,
            recv: BufReader::new(recv),
            unprompted,
            last_activity: Instant::now(),
        })
    }

    /// Sends one line and waits for the one-line reply. Returns the reply and its RTT.
    pub async fn request(&mut self, line: &str) -> Result<(String, Duration)> {
        if line.contains('\n') {
            bail_any!("ctl lines must not contain newlines");
        }
        let t0 = Instant::now();
        self.send
            .write_all(format!("{line}\n").as_bytes())
            .await
            .anyerr()?;
        let mut reply = String::new();
        let n = tokio::time::timeout(REPLY_TIMEOUT, self.recv.read_line(&mut reply))
            .await
            .map_err(|_| anyerr!("no reply to {line:?} within {REPLY_TIMEOUT:?}"))?
            .anyerr()?;
        let rtt = t0.elapsed();
        self.last_activity = Instant::now();
        if n == 0 {
            bail_any!("the object finished the ctl stream (no reply to {line:?})");
        }
        Ok((reply.trim_end().to_string(), rtt))
    }

    /// Waits up to `timeout` for the next unprompted uni-stream line.
    pub async fn next_unprompted(&mut self, timeout: Duration) -> Option<String> {
        tokio::time::timeout(timeout, self.unprompted.recv())
            .await
            .ok()
            .flatten()
    }

    /// Returns an unprompted line if one has already arrived.
    pub fn try_unprompted(&mut self) -> Option<String> {
        self.unprompted.try_recv().ok()
    }

    /// Finishes our side of the control stream and drains the DO's side (bounded).
    pub async fn finish(&mut self, timeout: Duration) {
        let _ = self.send.finish();
        let mut rest = String::new();
        let _ = tokio::time::timeout(timeout, async {
            loop {
                rest.clear();
                match self.recv.read_line(&mut rest).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_with_and_without_records() {
        let old =
            Status::parse(r#"{"id":"x","state":"Active","wakes":2,"endpoint_builds":1}"#).unwrap();
        assert_eq!(old.records, None);
        let new = Status::parse(r#"{"state":"Active","wakes":3,"records":42}"#).unwrap();
        assert_eq!(new.records, Some(42));
        assert!(new.summary().ends_with(" records=42"));
    }

    #[test]
    fn count_and_log_parse_and_format() {
        let c = Counts::parse(r#"{"total":7,"yours":3,"peers":2}"#).unwrap();
        assert_eq!((c.total, c.yours, c.peers), (7, 3, 2));
        let rows = parse_log(
            r#"[{"seq":6,"ts_ms":1792600000000,"peer":"ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1u","kind":"ctl","text":"hi"},
                {"seq":7,"ts_ms":1792600001000,"peer":"ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1u","kind":"callback","text":"delivered"}]"#,
        )
        .unwrap();
        let out = format_log(&rows);
        assert!(out.contains("ybndrfg8ej  ctl "));
        assert!(out.contains("callback  delivered"));
        assert!(parse_log("echo: /log 3").is_err());
    }
}
