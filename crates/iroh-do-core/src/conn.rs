//! Connection dispatch by ALPN and the idle close.
//!
//! Every connection, accepted or dialed, gets a watcher task. The watcher holds only a weak
//! handle, so dropping every clone of the connection closes it, as under iroh's `Router`. It
//! counts the connection for the linger, and closes it once it has carried no application
//! data for its idle limit.
//!
//! Application data is observed in one of two ways. By default the watcher samples the
//! connection's STREAM frame counts (`frame_tx.stream`, `frame_rx.stream`): iroh's keep-alives
//! are PING and ACK frames and leave them alone (design note 011, phase 0). Sampling every
//! quarter of the limit, it closes a connection between one and one and a quarter limits after
//! its last application data. A handler may instead report activity through [`Activity`].
//! Once it has, the watcher stops sampling and closes exactly one limit after the last
//! report. Reports also add a distinction QUIC statistics cannot make: a connection with a
//! stream open (counted by [`Activity::stream`] guards) is closed only after
//! [`Config::t_silent`](crate::Config::t_silent).

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use iroh::endpoint::{Connection, ConnectionStats, Incoming, VarInt};
use n0_future::time::Instant;
use tracing::{debug, info, warn};

use crate::{Shared, app::Registered};

/// Application error code of the node's own closes.
pub const CLOSE_OK: VarInt = VarInt::from_u32(0);

/// Application activity on one connection, for the idle close.
#[derive(Debug)]
pub struct Activity {
    open_streams: AtomicUsize,
    last: Mutex<Instant>,
    /// Whether the handler has reported activity; from then on, frame counts are ignored.
    reported: AtomicBool,
}

impl Activity {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            open_streams: AtomicUsize::new(0),
            last: Mutex::new(Instant::now()),
            reported: AtomicBool::new(false),
        })
    }

    /// Records application activity now.
    pub fn touch(&self) {
        self.reported.store(true, Ordering::SeqCst);
        self.observed();
    }

    /// Records activity seen in the connection's frame counts.
    fn observed(&self) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    /// Counts an open stream until the guard is dropped, and records activity at both ends.
    pub fn stream(self: &Arc<Self>) -> StreamGuard {
        self.open_streams.fetch_add(1, Ordering::SeqCst);
        self.touch();
        StreamGuard(self.clone())
    }

    /// Returns the time left until the connection is closed for inactivity, and the close
    /// reason: `idle` with no stream open (limit `idle`), `silent` otherwise (limit `silent`).
    fn close_deadline(&self, idle: Duration, silent: Duration) -> (Duration, &'static str) {
        let (limit, reason) = if self.open_streams.load(Ordering::SeqCst) > 0 {
            (silent, "silent")
        } else {
            (idle, "idle")
        };
        let last = *self.last.lock().unwrap_or_else(|e| e.into_inner());
        (limit.saturating_sub(last.elapsed()), reason)
    }
}

/// Counts an open stream for the idle close until dropped.
#[derive(Debug)]
pub struct StreamGuard(Arc<Activity>);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.0.open_streams.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}

fn stream_frames(s: &ConnectionStats) -> u64 {
    s.frame_tx.stream + s.frame_rx.stream
}

impl Shared {
    /// Accepts one incoming connection and hands it to the handler registered for its ALPN.
    pub(crate) async fn handle_incoming(
        self: Arc<Self>,
        incoming: Incoming,
        protocols: Arc<std::collections::HashMap<Vec<u8>, Registered>>,
    ) {
        // Counts the handshake for the linger; `track` takes over once it completes.
        let _busy = self.busy();
        let mut accepting = match incoming.accept() {
            Ok(a) => a,
            Err(e) => {
                debug!("incoming refused: {e}");
                return;
            }
        };
        let alpn = match accepting.alpn().await {
            Ok(a) => a,
            Err(e) => {
                debug!("handshake failed: {e}");
                return;
            }
        };
        let Some(entry) = protocols.get(&alpn) else {
            // The endpoint offers only registered ALPNs, so TLS has already refused others.
            warn!(event = "conn_refused", alpn = %String::from_utf8_lossy(&alpn));
            return;
        };
        let handler = entry.handler.clone();
        let conn = match handler.on_accepting(accepting).await {
            Ok(c) => c,
            Err(e) => {
                debug!("accepting failed: {e}");
                return;
            }
        };
        self.track(&conn, "in");
        drop(_busy);
        if let Err(e) = handler.accept(conn).await {
            debug!(alpn = %String::from_utf8_lossy(&alpn), "handler: {e}");
        }
    }

    /// Returns the activity record of `conn`, if the node tracks it.
    pub(crate) fn activity(&self, conn: &Connection) -> Option<Arc<Activity>> {
        self.activities().get(&conn.stable_id()).cloned()
    }

    /// Counts `conn` until it closes, and closes it when idle. `dir` is `in` or `out`.
    pub(crate) fn track(self: &Arc<Self>, conn: &Connection, dir: &'static str) {
        let id = conn.stable_id();
        let peer = conn.remote_id().to_z32();
        let alpn = String::from_utf8_lossy(conn.alpn()).into_owned();
        let idle = self.idle_limit(conn.alpn());
        let silent = self.config().t_silent;
        let activity = Activity::new();
        self.activities().insert(id, activity.clone());
        self.connection_opened();
        let busy = self.busy();
        let weak = conn.weak_handle();
        let closed = weak.closed();
        let mut frames = stream_frames(&conn.stats());
        info!(event = "conn_open", peer, alpn, dir);
        let this = self.clone();
        self.spawn(async move {
            let started = Instant::now();
            let watch = async {
                loop {
                    let Some(conn) = weak.upgrade() else {
                        return "dropped".to_string();
                    };
                    if !activity.reported.load(Ordering::SeqCst) {
                        let now = stream_frames(&conn.stats());
                        if now != frames {
                            frames = now;
                            activity.observed();
                        }
                    }
                    let (left, reason) = activity.close_deadline(idle, silent);
                    if left.is_zero() {
                        conn.close(CLOSE_OK, reason.as_bytes());
                        this.record_idle_close();
                        return reason.to_string();
                    }
                    drop(conn);
                    // Re-checked at least every shorter limit, because a stream that closes
                    // switches the connection from `silent` to `idle`. Sampled frame counts
                    // make a close up to a quarter of that limit late, so they are sampled four
                    // times as often.
                    let shorter = idle.min(silent);
                    let step = if activity.reported.load(Ordering::SeqCst) {
                        left.min(shorter)
                    } else {
                        left.min(shorter / 4)
                    };
                    n0_future::time::sleep(step).await;
                }
            };
            let closed = async {
                closed
                    .await
                    .map_or_else(|| "dropped".to_string(), |c| c.reason.to_string())
            };
            let reason = n0_future::future::race(watch, closed).await;
            this.activities().remove(&id);
            this.connection_closed();
            drop(busy);
            info!(
                event = "conn_close",
                peer,
                alpn,
                dir,
                reason,
                ms = started.elapsed().as_millis() as u64,
            );
        });
    }
}
