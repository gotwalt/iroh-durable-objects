//! Bridge between the DO and [`iroh_do_core::Node`]: the [`Host`] the node
//! calls back into.
//!
//! `Host` must be `Send + Sync`; DO handles are not. wasm32 is
//! single-threaded, so the host carries only the local EndpointId and reaches
//! the DO's storage through a thread-local keyed by that id (several DO
//! instances can share one isolate).

use std::{cell::RefCell, collections::HashMap, io, rc::Rc, time::Duration};

use iroh_base::EndpointId;
use iroh_do_core::{Counts, FrameSink, Host, LogKind, LogRow, LogStats, Member};
use tracing::error;
use worker::Storage;

use super::{
    endpoint::{SCHED_CALLBACK, now_ms},
    links,
    store::{LogRowSql, Store},
};

struct Handles {
    store: Store,
    storage: Rc<Storage>,
}

thread_local! {
    static HANDLES: RefCell<HashMap<EndpointId, Handles>> = RefCell::new(HashMap::new());
}

/// Register the storage handles of the DO serving `local` (each constructor run).
pub fn register(local: EndpointId, store: Store, storage: Storage) {
    HANDLES.with(|h| {
        h.borrow_mut().insert(
            local,
            Handles {
                store,
                storage: Rc::new(storage),
            },
        )
    });
}

/// Persisted counter of Endpoint instantiations (reported by `/status`).
pub const COUNTER_ENDPOINT_BUILDS: &str = "endpoint_builds";

/// The DO-side [`Host`].
#[derive(Debug, Clone, Copy)]
pub struct DoHost {
    /// The local endpoint.
    pub local: EndpointId,
}

impl FrameSink for DoHost {
    fn send_datagrams(
        &self,
        peer: &EndpointId,
        contents: &[u8],
        segment_size: Option<usize>,
    ) -> io::Result<()> {
        links::send(&self.local, peer, contents, segment_size)
    }
}

fn with_store<T>(local: &EndpointId, f: impl FnOnce(&Store) -> worker::Result<T>) -> Option<T> {
    HANDLES.with(|h| {
        let h = h.borrow();
        let handles = h.get(local)?;
        match f(&handles.store) {
            Ok(v) => Some(v),
            Err(e) => {
                error!(event = "log_sql_failed", error = %e);
                None
            }
        }
    })
}

fn u(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn row(r: LogRowSql) -> LogRow {
    LogRow {
        seq: u(r.seq),
        prev: u(r.prev),
        ts_ms: u(r.ts_ms),
        peer: r.peer,
        kind: r.kind,
        nick: r.nick,
        text: r.text,
    }
}

fn rows(r: Option<Vec<LogRowSql>>) -> Vec<LogRow> {
    r.unwrap_or_default().into_iter().map(row).collect()
}

impl Host for DoHost {
    fn log_event(&self, peer: EndpointId, kind: LogKind, text: &str) -> LogStats {
        let z = peer.to_z32();
        with_store(&self.local, |s| {
            let (seq, _) = s.log_insert(now_ms(), &z, kind.as_str(), "", text)?;
            Ok(LogStats {
                seq: u(seq),
                // The row just inserted has the largest `seq`.
                total: u(seq),
                yours: u(s.log_yours(&z)?),
            })
        })
        .unwrap_or_default()
    }

    fn append(&self, peer: EndpointId, kind: LogKind, nick: &str, text: &str) -> Option<LogRow> {
        let z = peer.to_z32();
        let ts = now_ms();
        let (seq, prev) = with_store(&self.local, |s| {
            s.log_insert(ts, &z, kind.as_str(), nick, text)
        })?;
        Some(LogRow {
            seq: u(seq),
            prev: u(prev),
            ts_ms: ts as u64,
            peer: z,
            kind: kind.as_str().to_string(),
            nick: nick.to_string(),
            text: text.to_string(),
        })
    }

    fn room_seq(&self) -> u64 {
        with_store(&self.local, |s| s.room_seq()).map_or(0, u)
    }

    fn events_after(&self, seq: u64, limit: usize) -> Vec<LogRow> {
        let seq = i64::try_from(seq).unwrap_or(i64::MAX);
        rows(with_store(&self.local, |s| s.events_after(seq, limit)))
    }

    fn events_recent(&self, n: usize) -> Vec<LogRow> {
        rows(with_store(&self.local, |s| s.events_recent(n)))
    }

    fn member(&self, peer: EndpointId) -> Option<Member> {
        let m = with_store(&self.local, |s| s.member(&peer.to_z32())).flatten()?;
        Some(Member {
            peer,
            nick: m.nick,
            joined: m.joined != 0,
        })
    }

    fn members(&self) -> Vec<Member> {
        with_store(&self.local, |s| s.members())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| {
                Some(Member {
                    peer: EndpointId::from_z32(&m.peer).ok()?,
                    nick: m.nick,
                    joined: m.joined != 0,
                })
            })
            .collect()
    }

    fn set_member(&self, m: &Member) {
        with_store(&self.local, |s| {
            s.set_member(&m.peer.to_z32(), &m.nick, m.joined, now_ms())
        });
    }

    fn count(&self, peer: EndpointId) -> Counts {
        let z = peer.to_z32();
        with_store(&self.local, |s| {
            Ok(Counts {
                total: u(s.log_total()?),
                yours: u(s.log_yours(&z)?),
                peers: u(s.log_peers()?),
            })
        })
        .unwrap_or_default()
    }

    fn total(&self) -> u64 {
        with_store(&self.local, |s| s.log_total()).map_or(0, u)
    }

    fn recent(&self, n: usize) -> Vec<LogRow> {
        rows(with_store(&self.local, |s| s.log_recent(n)))
    }

    fn endpoint_built(&self) {
        // One synchronous SQLite write per activation (never per packet).
        HANDLES.with(|h| {
            if let Some(handles) = h.borrow().get(&self.local)
                && let Err(e) = handles.store.bump(COUNTER_ENDPOINT_BUILDS)
            {
                error!(event = "builds_bump_failed", error = %e);
            }
        });
    }

    fn schedule_callback(&self, peer: EndpointId, delay: Duration) {
        let due = now_ms() + delay.as_millis() as f64;
        let key = format!("{SCHED_CALLBACK}{}", peer.to_z32());
        let storage = HANDLES.with(|h| {
            let h = h.borrow();
            let handles = h.get(&self.local)?;
            if let Err(e) = handles.store.schedule(&key, due, "") {
                error!(event = "schedule_failed", error = %e);
                return None;
            }
            let next = handles.store.next_due().ok().flatten().unwrap_or(due);
            Some((handles.storage.clone(), next))
        });
        if let Some((storage, next)) = storage {
            // Arming the alarm is a storage promise; it settles promptly and
            // does not pin the object.
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = storage.set_alarm(super::endpoint::at_ms(next)).await {
                    error!(event = "set_alarm_failed", error = %e);
                }
            });
        }
    }
}
