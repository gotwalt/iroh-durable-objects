//! Synchronous storage transactions: `ctx.storage.transactionSync`, which workers-rs 0.8.7
//! does not bind.
//!
//! `transactionSync(fn)` runs `fn` and commits the SQL it ran, or rolls all of it back if
//! `fn` throws. A wasm trap inside `fn` reaches the runtime as a thrown `RuntimeError`, so it
//! rolls the transaction back too.

use wasm_bindgen::{JsCast, JsValue, closure::ScopedClosure, prelude::wasm_bindgen};
use worker::State;

#[wasm_bindgen]
extern "C" {
    /// `DurableObjectStorage`, for the one method workers-rs lacks.
    #[wasm_bindgen(extends = js_sys::Object)]
    pub(crate) type TxStorage;

    #[wasm_bindgen(method, catch, js_name = transactionSync)]
    fn transaction_sync(
        this: &TxStorage,
        body: &ScopedClosure<dyn FnMut() -> Result<JsValue, JsValue>>,
    ) -> Result<JsValue, JsValue>;
}

impl TxStorage {
    /// The storage of the object `state` belongs to, `None` if the runtime does not expose it.
    pub(crate) fn of(state: &State) -> Option<Self> {
        state
            .as_raw()
            .storage()
            .ok()
            .map(|s| s.unchecked_into::<TxStorage>())
    }

    /// Runs `body` in a transaction. Its SQL is committed if it returns `Ok`, and rolled back
    /// if it returns `Err`, panics, or the commit fails.
    pub(crate) fn transaction<T>(
        &self,
        body: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let mut body = Some(body);
        let mut out: Option<Result<T, String>> = None;
        let mut run = || -> Result<JsValue, JsValue> {
            let body = body
                .take()
                .ok_or_else(|| JsValue::from_str("transaction body called twice"))?;
            let res = body();
            // Throwing is what makes `transactionSync` roll back.
            let thrown = res.as_ref().err().map(|e| JsValue::from_str(e));
            out = Some(res);
            thrown.map_or(Ok(JsValue::UNDEFINED), Err)
        };
        // Called synchronously by `transactionSync`; the borrow ends with this function.
        // `panic = "abort"`: a panic traps, and the trap is thrown through the runtime.
        let closure =
            ScopedClosure::<dyn FnMut() -> Result<JsValue, JsValue>>::borrow_mut_aborting(&mut run);
        let res = self.transaction_sync(&closure);
        drop(closure);
        match (res, out) {
            (_, Some(Err(e))) => Err(e),
            (Ok(_), Some(Ok(v))) => Ok(v),
            (Err(e), _) => Err(format!("transaction failed: {}", js_text(&e))),
            (Ok(_), None) => Err("transaction body did not run".into()),
        }
    }
}

/// The message of a thrown JavaScript value.
pub(crate) fn js_text(e: &JsValue) -> String {
    e.as_string()
        .or_else(|| e.dyn_ref::<js_sys::Error>().map(|e| String::from(e.message())))
        .unwrap_or_else(|| format!("{e:?}"))
}
