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
//!
//! When the connection closes, its guards stop counting. A guard held by a task that has not
//! yet noticed the close, or by any value that outlives the connection, must not be counted
//! as an open stream after the connection is gone.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use iroh::endpoint::{Connection, ConnectionError, ConnectionStats, Incoming, VarInt};
use n0_future::time::Instant;
use tracing::{debug, info, warn};

use crate::{Shared, app::Registered};

/// Application error code of the node's own closes.
pub const CLOSE_OK: VarInt = VarInt::from_u32(0);

/// Application activity on one connection, for the idle close.
#[derive(Debug)]
pub struct Activity {
    /// Open streams; `None` once the connection has closed, after which guards count nothing.
    open_streams: Mutex<Option<usize>>,
    /// Open streams of every connection of the node ([`Stats::open_streams`](crate::Stats)).
    node_streams: Arc<AtomicUsize>,
    last: Mutex<Instant>,
    /// Whether the handler has reported activity; from then on, frame counts are ignored.
    reported: AtomicBool,
    /// The reason of the node's own close ([`Activity::close`]), for the `conn_close` event.
    closed_by_node: Mutex<Option<&'static str>>,
}

impl Activity {
    fn new(node_streams: Arc<AtomicUsize>) -> Arc<Self> {
        Arc::new(Self {
            open_streams: Mutex::new(Some(0)),
            node_streams,
            last: Mutex::new(Instant::now()),
            reported: AtomicBool::new(false),
            closed_by_node: Mutex::new(None),
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

    /// Closes `conn` with [`CLOSE_OK`] and `reason`, unless it is already closed, and logs
    /// `reason` in its `conn_close` event.
    pub fn close(&self, conn: &Connection, reason: &'static str) {
        if conn.close_reason().is_some() {
            return;
        }
        *self
            .closed_by_node
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(reason);
        conn.close(CLOSE_OK, reason.as_bytes());
    }

    fn streams(&self) -> std::sync::MutexGuard<'_, Option<usize>> {
        self.open_streams.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Counts an open stream until the guard is dropped or the connection closes, and records
    /// activity at both ends.
    pub fn stream(self: &Arc<Self>) -> StreamGuard {
        if let Some(n) = self.streams().as_mut() {
            *n += 1;
            self.node_streams.fetch_add(1, Ordering::SeqCst);
        }
        self.touch();
        StreamGuard(self.clone())
    }

    /// Stops counting this connection's streams; called once the connection has closed.
    fn release(&self) {
        if let Some(n) = self.streams().take() {
            self.node_streams.fetch_sub(n, Ordering::SeqCst);
        }
    }

    /// Returns the time left until the connection is closed for inactivity, and the close
    /// reason: `idle` with no stream open (limit `idle`), `silent` otherwise (limit `silent`).
    fn close_deadline(&self, idle: Duration, silent: Duration) -> (Duration, &'static str) {
        let (limit, reason) = if self.streams().is_some_and(|n| n > 0) {
            (silent, "silent")
        } else {
            (idle, "idle")
        };
        let last = *self.last.lock().unwrap_or_else(|e| e.into_inner());
        (limit.saturating_sub(last.elapsed()), reason)
    }
}

/// Counts an open stream for the idle close until dropped or until its connection closes.
#[derive(Debug)]
pub struct StreamGuard(Arc<Activity>);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        if let Some(n) = self.0.streams().as_mut() {
            *n = n.saturating_sub(1);
            self.0.node_streams.fetch_sub(1, Ordering::SeqCst);
        }
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
        let activity = Activity::new(self.open_streams.clone());
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
                        activity.close(&conn, reason);
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
                match closed.await {
                    None => "dropped".to_string(),
                    Some(c) if matches!(c.reason, ConnectionError::LocallyClosed) => activity
                        .closed_by_node
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .unwrap_or("closed")
                        .to_string(),
                    Some(c) => c.reason.to_string(),
                }
            };
            let reason = n0_future::future::race(watch, closed).await;
            activity.release();
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

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE: Duration = Duration::from_secs(10);
    const SILENT: Duration = Duration::from_secs(60);

    /// Guards count toward the connection and the node until dropped, and stop counting once
    /// the connection has closed, also guards dropped or taken later.
    #[test]
    fn guards_stop_counting_once_the_connection_closes() {
        let node = Arc::new(AtomicUsize::new(0));
        let a = Activity::new(node.clone());
        assert_eq!(a.close_deadline(IDLE, SILENT).1, "idle");
        let g1 = a.stream();
        let g2 = a.stream();
        assert_eq!(node.load(Ordering::SeqCst), 2);
        assert_eq!(a.close_deadline(IDLE, SILENT).1, "silent");
        drop(g1);
        assert_eq!(node.load(Ordering::SeqCst), 1);
        assert_eq!(a.close_deadline(IDLE, SILENT).1, "silent");

        a.release();
        assert_eq!(node.load(Ordering::SeqCst), 0);
        assert_eq!(a.close_deadline(IDLE, SILENT).1, "idle");
        // A guard held past the close must not underflow the node count when it is finally
        // dropped.
        drop(g2);
        assert_eq!(node.load(Ordering::SeqCst), 0);
        let g3 = a.stream();
        assert_eq!(node.load(Ordering::SeqCst), 0);
        assert_eq!(a.close_deadline(IDLE, SILENT).1, "idle");
        drop(g3);
        assert_eq!(node.load(Ordering::SeqCst), 0);
        a.release();
        assert_eq!(node.load(Ordering::SeqCst), 0);
    }

    /// Two connections share the node count; closing one releases only its own streams.
    #[test]
    fn node_count_spans_connections() {
        let node = Arc::new(AtomicUsize::new(0));
        let a = Activity::new(node.clone());
        let b = Activity::new(node.clone());
        let _ga = a.stream();
        let _gb1 = b.stream();
        let _gb2 = b.stream();
        assert_eq!(node.load(Ordering::SeqCst), 3);
        b.release();
        assert_eq!(node.load(Ordering::SeqCst), 1);
        assert_eq!(a.close_deadline(IDLE, SILENT).1, "silent");
    }
}
