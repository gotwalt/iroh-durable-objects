//! Bridge between the DO and [`iroh_do_core::Node`]: the [`Host`] the node
//! calls back into.
//!
//! `Host` must be `Send + Sync`; DO handles are not. wasm32 is
//! single-threaded, so the host carries only the local EndpointId and reaches
//! the DO's storage through a thread-local keyed by that id (several DO
//! instances can share one isolate).

use std::{cell::RefCell, collections::HashMap, io, rc::Rc, time::Duration};

use iroh_base::EndpointId;
use iroh_do_core::{FrameSink, Host, Sql, SqlError, Value};
use tracing::error;
use worker::{SqlStorage, SqlStorageValue, Storage};

use super::{endpoint::now_ms, links, store::Store, tx::TxStorage};

struct Handles {
    store: Store,
    storage: Rc<Storage>,
    tx: Option<Rc<TxStorage>>,
}

thread_local! {
    static HANDLES: RefCell<HashMap<EndpointId, Handles>> = RefCell::new(HashMap::new());
}

/// Register the storage handles of the DO serving `local` (each constructor run).
pub fn register(local: EndpointId, store: Store, storage: Storage, tx: Option<TxStorage>) {
    HANDLES.with(|h| {
        h.borrow_mut().insert(
            local,
            Handles {
                store,
                storage: Rc::new(storage),
                tx: tx.map(Rc::new),
            },
        )
    });
}

/// Drop the storage handles of `local`. Afterwards every [`Host`] storage call
/// for it is a no-op, so tasks of a destroyed endpoint that are still winding
/// down cannot write to its emptied storage.
pub fn unregister(local: &EndpointId) {
    HANDLES.with(|h| {
        h.borrow_mut().remove(local);
    });
}

/// Persisted counter of Endpoint instantiations (reported in the status).
pub const COUNTER_ENDPOINT_BUILDS: &str = "endpoint_builds";
/// When the bound `Endpoint` was bound, in ms since the UNIX epoch; absent while none is.
/// Found set by a constructor, it means the previous instance ended with an `Endpoint` bound,
/// and so with whatever QUIC connections it had (`evicted_active`).
pub const COUNTER_BOUND_AT: &str = "bound_at_ms";

/// Arms the alarm for the earliest schedule entry, or deletes it if there is none.
pub(crate) async fn rearm(store: &Store, storage: &Storage) -> worker::Result<()> {
    match store.next_due()? {
        Some(due) => {
            storage
                .set_alarm(super::endpoint::at_ms(due.max(now_ms())))
                .await
        }
        None => storage.delete_alarm().await,
    }
}

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

/// [`Sql`] over the DO's `sql.exec`.
pub struct DoSql<'a>(pub &'a SqlStorage);

impl Sql for DoSql<'_> {
    fn exec(&self, query: &str, params: &[Value]) -> Result<Vec<Vec<Value>>, SqlError> {
        let params: Vec<SqlStorageValue> = params
            .iter()
            .map(|p| match p {
                Value::Null => SqlStorageValue::Null,
                Value::Integer(i) => SqlStorageValue::Integer(*i),
                Value::Real(f) => SqlStorageValue::Float(*f),
                Value::Text(t) => SqlStorageValue::String(t.clone()),
                Value::Blob(b) => SqlStorageValue::Blob(b.clone()),
            })
            .collect();
        let err = |e: worker::Error| SqlError(e.to_string());
        let cursor = self.0.exec(query, params).map_err(err)?;
        let mut out = Vec::new();
        for row in cursor.raw() {
            out.push(
                row.map_err(err)?
                    .into_iter()
                    .map(|v| match v {
                        SqlStorageValue::Null => Value::Null,
                        SqlStorageValue::Boolean(b) => Value::Integer(i64::from(b)),
                        SqlStorageValue::Integer(i) => Value::Integer(i),
                        SqlStorageValue::Float(f) => Value::Real(f),
                        SqlStorageValue::String(s) => Value::Text(s),
                        SqlStorageValue::Blob(b) => Value::Blob(b),
                    })
                    .collect(),
            );
        }
        Ok(out)
    }
}

impl Host for DoHost {
    fn with_sql(&self, f: &mut dyn FnMut(&dyn Sql)) {
        // Clone the handle out of the thread-local so that `f` may call back into the host.
        let sql = HANDLES.with(|h| h.borrow().get(&self.local).map(|h| h.store.sql().clone()));
        if let Some(sql) = sql {
            f(&DoSql(&sql));
        }
    }

    fn transaction(
        &self,
        f: &mut dyn FnMut(&dyn Sql) -> Result<(), SqlError>,
    ) -> Option<Result<(), SqlError>> {
        let (sql, tx) = HANDLES.with(|h| {
            h.borrow()
                .get(&self.local)
                .map(|h| (h.store.sql().clone(), h.tx.clone()))
        })?;
        let Some(tx) = tx else {
            return Some(Err(SqlError("no transactionSync on this runtime".into())));
        };
        let mut failed = None;
        let res = tx.transaction(|| {
            f(&DoSql(&sql)).map_err(|e| {
                let text = e.0.clone();
                failed = Some(e);
                text
            })
        });
        Some(res.map_err(|e| failed.unwrap_or(SqlError(e))))
    }

    fn endpoint_built(&self) {
        // Two synchronous SQLite writes per activation (never per packet).
        HANDLES.with(|h| {
            let h = h.borrow();
            let Some(handles) = h.get(&self.local) else {
                return;
            };
            if let Err(e) = handles.store.bump(COUNTER_ENDPOINT_BUILDS) {
                error!(event = "builds_bump_failed", error = %e);
            }
            if let Err(e) = handles.store.set_counter(COUNTER_BOUND_AT, now_ms() as i64) {
                error!(event = "bound_marker_failed", error = %e);
            }
        });
    }

    fn endpoint_closed(&self) {
        HANDLES.with(|h| {
            if let Some(handles) = h.borrow().get(&self.local)
                && let Err(e) = handles.store.delete_counter(COUNTER_BOUND_AT)
            {
                error!(event = "bound_marker_failed", error = %e);
            }
        });
    }

    fn schedule(&self, key: &str, delay: Duration, payload: &str) {
        let due = now_ms() + delay.as_millis() as f64;
        self.update_schedule(|store| store.schedule(key, due, payload));
    }

    fn cancel(&self, key: &str) {
        self.update_schedule(|store| store.unschedule(key));
    }
}

impl DoHost {
    /// Applies `change` to the schedule and re-arms the alarm for the result.
    fn update_schedule(&self, change: impl FnOnce(&Store) -> worker::Result<()>) {
        let handles = HANDLES.with(|h| {
            let h = h.borrow();
            let handles = h.get(&self.local)?;
            if let Err(e) = change(&handles.store) {
                error!(event = "schedule_failed", error = %e);
                return None;
            }
            Some((handles.store.clone(), handles.storage.clone()))
        });
        if let Some((store, storage)) = handles {
            // A storage call: it settles promptly and does not pin the object. If the object
            // ends first, the entry is stored and the next alarm or schedule arms it.
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = rearm(&store, &storage).await {
                    error!(event = "set_alarm_failed", error = %e);
                }
            });
        }
    }
}
