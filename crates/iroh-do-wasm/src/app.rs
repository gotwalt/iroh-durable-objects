//! The TypeScript app as an `iroh_do_core::App`: one iroh `ProtocolHandler` per ALPN that the
//! object's `protocols()` returned, each calling the host's `accept`.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use iroh::{
    EndpointId,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use iroh_do_core::{App, Ctx, Idle, Protocols};
use wasm_bindgen_futures::JsFuture;

use crate::host;

/// The app of every object built by this library. Its protocols come from the object's
/// state in [`host`], which the TypeScript constructor filled in.
pub(crate) struct JsApp {
    ctx: Ctx,
}

impl App for JsApp {
    const NAME: &'static str = "ts";

    fn new(ctx: &Ctx) -> Self {
        JsApp { ctx: ctx.clone() }
    }

    fn protocols(&self, _ctx: &Ctx, p: &mut Protocols) {
        let id = self.ctx.id();
        let alpns = host::with(&id, |e| e.alpns.clone()).unwrap_or_default();
        for (alpn, idle) in alpns {
            let h = JsHandler {
                id,
                ctx: self.ctx.clone(),
                alpn: alpn.clone(),
            };
            match idle {
                Some(d) => p.accept_with(alpn, h, Idle::after(d)),
                None => p.accept(alpn, h),
            };
        }
    }
}

struct JsHandler {
    id: EndpointId,
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

impl ProtocolHandler for JsHandler {
    fn accept(&self, conn: Connection) -> impl Future<Output = Result<(), AcceptError>> + Send {
        let id = self.id;
        let alpn = String::from_utf8_lossy(&self.alpn).into_owned();
        let activity = self.ctx.activity(&conn);
        // iroh requires `Send`; awaiting a JS promise is not. See `AssertSend`.
        AssertSend(async move {
            let Some(h) = host::host(&id) else {
                return Ok(());
            };
            let js_conn = crate::streams::Connection::new(conn, activity);
            let promise = h
                .accept(&alpn, js_conn)
                .map_err(|e| AcceptError::from_err(JsErr(format!("{e:?}"))))?;
            JsFuture::from(promise)
                .await
                .map_err(|e| AcceptError::from_err(JsErr(format!("{e:?}"))))?;
            Ok(())
        })
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

/// Declares a future `Send`.
///
/// iroh's `ProtocolHandler::accept` returns a `Send` future on every target, but a future
/// holding a JS promise is not `Send`. wasm32-unknown-unknown without the atomics feature has
/// exactly one thread, so the future is never moved to another thread. (The node itself spawns
/// with `spawn_local` on wasm, which needs no `Send`; the bound comes from iroh's trait.)
struct AssertSend<F>(F);

// SAFETY: this crate builds only for wasm32 without threads (see `lib.rs`).
unsafe impl<F> Send for AssertSend<F> {}

impl<F: Future> Future for AssertSend<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: structural pinning of the only field.
        unsafe { self.map_unchecked_mut(|s| &mut s.0) }.poll(cx)
    }
}
