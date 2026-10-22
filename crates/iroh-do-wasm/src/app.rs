//! The TypeScript object's protocols as an `iroh_do_core::App`: one handler per ALPN that the
//! object's `protocols()` returned, each calling `NodeHost.accept`.
//!
//! # Connection lifetime
//!
//! Under iroh's `Router`, returning from `ProtocolHandler::accept` drops the connection, and
//! dropping the last handle closes it. Here JavaScript holds a handle in the `Connection`
//! wrapper until the garbage collector frees it, at an unspecified time. The handler therefore
//! closes the connection itself, with code 0 and reason `done`, when the promise returned by
//! the app's `accept` settles, whether it resolves or rejects. A handler that keeps using the
//! connection keeps its promise pending, for example until `conn.closed()`.
//!
//! Connections the app dials with `Endpoint.connect` have no such scope. They stay open
//! until the app or the peer closes them, or the node's idle close does
//! (`iroh_do_core::conn`).

use std::future::Future;

use iroh::{endpoint::Connection, protocol::AcceptError};
use iroh_do_core::{App, Ctx, Idle, LocalProtocolHandler, Protocols};
use wasm_bindgen_futures::JsFuture;

use crate::host::{self, Token};

/// Reason of the close when the handler's promise settles.
const REASON_DONE: &str = "done";

/// The app of every node this library builds. Its protocols come from the node's entry in
/// [`host`], which `WasmNode::new` filled in from `NodeOptions.protocols`.
pub(crate) struct JsApp {
    ctx: Ctx,
    /// The node's entry; `None` only if the app was built outside `WasmNode::new`.
    token: Option<Token>,
}

impl App for JsApp {
    const NAME: &'static str = "ts";

    fn new(ctx: &Ctx) -> Self {
        // `Node::new` builds the app synchronously, inside `host::building`.
        JsApp {
            ctx: ctx.clone(),
            token: host::current_build(),
        }
    }

    fn protocols(&self, _ctx: &Ctx, p: &mut Protocols) {
        let Some(token) = self.token else { return };
        let alpns = host::with(token, |e| e.alpns.clone()).unwrap_or_default();
        for (alpn, idle) in alpns {
            let h = JsHandler {
                token,
                ctx: self.ctx.clone(),
                alpn: alpn.clone(),
            };
            p.accept_local(alpn, h, idle.map(Idle::after));
        }
    }
}

struct JsHandler {
    token: Token,
    ctx: Ctx,
    alpn: Vec<u8>,
}

impl std::fmt::Debug for JsHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsHandler")
            .field("alpn", &String::from_utf8_lossy(&self.alpn))
            .finish()
    }
}

impl LocalProtocolHandler for JsHandler {
    fn accept(&self, conn: Connection) -> impl Future<Output = Result<(), AcceptError>> {
        let token = self.token;
        let alpn = String::from_utf8_lossy(&self.alpn).into_owned();
        let activity = self.ctx.activity(&conn);
        async move {
            let Some(h) = host::host(token) else {
                return Ok(());
            };
            let js_conn = crate::streams::Connection::new(conn.clone(), activity.clone());
            let res = match h.accept(&alpn, js_conn) {
                Ok(promise) => JsFuture::from(promise).await.map(drop),
                Err(e) => Err(e),
            };
            match &activity {
                Some(a) => a.close(&conn, REASON_DONE),
                None => conn.close(iroh_do_core::CLOSE_OK, REASON_DONE.as_bytes()),
            }
            res.map_err(|e| AcceptError::from_err(JsErr(crate::js_message(&e))))
        }
    }
}

#[derive(Debug)]
struct JsErr(String);

impl std::fmt::Display for JsErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JsErr {}
