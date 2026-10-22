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

use super::{endpoint::now_ms, links, store::Store};

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

    fn schedule(&self, key: &str, delay: Duration, payload: &str) {
        let due = now_ms() + delay.as_millis() as f64;
        let storage = HANDLES.with(|h| {
            let h = h.borrow();
            let handles = h.get(&self.local)?;
            if let Err(e) = handles.store.schedule(key, due, payload) {
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

    fn cancel(&self, key: &str) {
        HANDLES.with(|h| {
            if let Some(handles) = h.borrow().get(&self.local)
                && let Err(e) = handles.store.unschedule(key)
            {
                error!(event = "unschedule_failed", error = %e);
            }
        });
    }
}
