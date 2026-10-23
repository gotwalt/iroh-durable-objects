//! The endpoint object: what the `endpoint` Durable Object class that
//! [`worker!`](crate::worker) declares delegates to (spec §5.5–§5.8, §13.4).
//!
//! One object per EndpointId, running the app named in its identity. It accepts peers' relay
//! WebSockets as hibernatable sockets and feeds their events to the ingress's socket table
//! ([`Sockets`]), which keeps one session per socket and one socket per peer. It carries out
//! the table's effects: frames, attachments and closes on the sockets, and authenticated
//! peers and their datagrams for `iroh_do_core::Node`, which owns the iroh `Endpoint`.
//!
//! Memory is lost on hibernation, on eviction, and on a wasm panic (the
//! worker-build shim re-runs the constructor in place, design note 002 §3). The
//! constructor therefore rebuilds everything: identity from SQLite, sessions
//! from socket attachments, and the node from the authenticated sockets.
//!
//! Timers would keep the object from hibernating, so all deferred work
//! (republish, handshake sweep, dial-back) is a row in the `schedule` table
//! served by the single Durable Object alarm.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    time::Duration,
};

use iroh_base::{EndpointId, SecretKey};
use iroh_relay_ingress::{
    Effect, IngressConfig, SessionSnapshot, Sockets, UpgradeRequest, close_code,
};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};
use worker::{
    Env, Request, Response, Result, State, WebSocket, WebSocketIncomingMessage, WebSocketPair,
};

use iroh_do_core::{
    ALARM_RETRY_BASE, APP_KEY_PREFIX, AppEntry, Config as NodeConfig, Node, alarm_retry_delay,
};

use super::{
    links,
    node::{self, DoHost},
    pkarr_publish, registry,
    router::{
        DestroyResponse, ENDPOINTS_BINDING, INTERNAL_DESTROY, INTERNAL_MINT, INTERNAL_STATUS,
        MintRequest, MintResponse, NAME_HEADER,
    },
    store::{Due, Identity, Store},
    tx::TxStorage,
};
use crate::pkarr::{REPUBLISH_INTERVAL_MS, RETRY_BASE_MS};

/// Tag on every relay socket (tags are fixed at accept time, before the peer
/// is known, so peer lookup goes through the attachment instead).
const TAG_RELAY: &str = "relay";
/// Tag prefix carrying the socket's random id, used to find its session.
const TAG_SID: &str = "sid:";
/// Close code when the DO refuses traffic (no/invalid identity).
const CLOSE_REFUSED: u16 = 1011;
/// Close code for sockets of a destroyed endpoint (normal closure).
const CLOSE_DESTROYED: u16 = 1000;

/// Schedule key for the pkarr republish.
pub const SCHED_REPUBLISH: &str = "republish";
/// Schedule key for the unauthenticated-socket sweep (handshake deadline).
/// No `setTimeout`: pending timers would block hibernation.
pub const SCHED_SWEEP: &str = "sweep";

/// Attachment persisted on each hibernatable socket (≤ 16 KiB; this is ~150 B).
#[derive(Serialize, Deserialize)]
struct Attachment {
    /// Format version.
    v: u8,
    /// Random socket id, also in the `sid:` tag.
    sid: String,
    /// Ingress session snapshot.
    s: SessionSnapshot,
}

pub(crate) fn now_ms() -> f64 {
    js_sys::Date::now()
}

/// An absolute alarm time. (`From<i64>` for `ScheduledTime` is an *offset*.)
pub(crate) fn at_ms(ms: f64) -> worker::ScheduledTime {
    worker::ScheduledTime::new(js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms)))
}

fn now() -> Duration {
    Duration::from_millis(now_ms() as u64)
}

/// A random socket id. `None` if the CSPRNG (`crypto.getRandomValues`) fails: a
/// predictable id could collide with another socket's, so the caller refuses.
fn random_sid() -> Option<String> {
    let mut b = [0u8; 8];
    if getrandom::fill(&mut b).is_err() {
        error!(event = "rng_failure");
        return None;
    }
    Some(data_encoding::HEXLOWER.encode(&b))
}

/// Loaded, verified identity of this object.
struct Me {
    id: EndpointId,
    secret: SecretKey,
    app: AppEntry,
    relay_url: String,
    pkarr_relay: String,
}

/// An iroh endpoint as a Durable Object.
///
/// Not a Durable Object class itself: [`worker!`](crate::worker) declares the class
/// `endpoint` in the deployment's crate and delegates every handler to this type.
pub struct Object {
    state: State,
    env: Env,
    /// The apps this deployment was built with.
    apps: &'static [AppEntry],
    store: Store,
    /// `transactionSync`, for the mint.
    tx: Option<TxStorage>,
    me: RefCell<Option<Me>>,
    /// The stored identity exists but violates the name invariant (§13.4.4).
    refused: Cell<bool>,
    /// The ingress sessions of the relay sockets, once an identity is loaded.
    table: RefCell<Option<Sockets<String>>>,
    /// The relay sockets by id, for carrying out the table's effects.
    sockets: RefCell<HashMap<String, WebSocket>>,
    node: RefCell<Option<Node>>,
    /// Persisted constructor-run count at this wake.
    wakes: Cell<i64>,
}

impl Object {
    /// This object's EndpointId in z-base-32, or `""` before it is minted. Every event this
    /// object logs carries it as `endpoint`.
    fn endpoint(&self) -> String {
        self.me
            .borrow()
            .as_ref()
            .map(|m| m.id.to_z32())
            .unwrap_or_default()
    }

    fn peer_of(&self, sid: &str) -> Option<EndpointId> {
        self.table
            .borrow()
            .as_ref()
            .and_then(|t| t.peer_of(&sid.to_owned()))
    }

    /// Runs `f` on the socket table and carries out the effects it returns. Returns the
    /// handshake deadline among them, which the caller schedules.
    fn with_table(
        &self,
        f: impl FnOnce(&mut Sockets<String>) -> Vec<Effect<String>>,
    ) -> Option<Duration> {
        let effects = {
            let mut table = self.table.borrow_mut();
            match table.as_mut() {
                Some(t) => f(t),
                None => return None,
            }
        };
        self.apply(effects)
    }

    /// Carries out the socket table's effects, in order; returns the last deadline.
    fn apply(&self, effects: Vec<Effect<String>>) -> Option<Duration> {
        let local = self.me.borrow().as_ref().map(|m| m.id)?;
        let ws = |sid: &str| self.sockets.borrow().get(sid).cloned();
        let mut deadline = None;
        for effect in effects {
            match effect {
                Effect::Send(sid, frame) => {
                    if let Some(ws) = ws(&sid) {
                        let _ = ws.send_with_bytes(frame.as_bytes());
                    }
                }
                Effect::Persist(sid, s) => {
                    let Some(ws) = ws(&sid) else { continue };
                    if let Err(e) = ws.serialize_attachment(Attachment { v: 1, sid, s }) {
                        warn!(event = "attachment_write_failed", endpoint = %local.to_z32(), error = %e);
                    }
                }
                Effect::Close(sid, code, reason) => {
                    if let Some(ws) = self.sockets.borrow_mut().remove(&sid) {
                        let _ = ws.close(Some(code), Some(reason));
                    }
                }
                Effect::PeerUp(peer, sid) => {
                    if let Some(ws) = ws(&sid) {
                        links::bind(local, peer, ws);
                    }
                    self.ensure_node();
                    if let Some(node) = self.node.borrow().as_ref() {
                        node.socket_authenticated(peer);
                    }
                }
                Effect::PeerDown(peer) => {
                    links::unbind(&local, &peer);
                    if let Some(node) = self.node.borrow().as_ref() {
                        node.socket_closed(peer);
                    }
                }
                Effect::Datagrams {
                    from,
                    ecn,
                    segment_size,
                    contents,
                } => {
                    self.ensure_node();
                    if let Some(node) = self.node.borrow().as_ref() {
                        node.datagrams(
                            from,
                            ecn,
                            contents.into(),
                            segment_size.map(|s| usize::from(s.get())),
                        );
                    }
                }
                Effect::Deadline(d) => deadline = Some(d),
            }
        }
        deadline
    }

    fn id_matches_name(&self, id: &EndpointId) -> bool {
        let Ok(ns) = self.env.durable_object(ENDPOINTS_BINDING) else {
            return false;
        };
        match ns.id_from_name(&id.to_z32()) {
            Ok(expected) => expected == self.state.id(),
            Err(_) => false,
        }
    }

    fn load(&self) {
        let ident = match self.store.load_identity() {
            Ok(i) => i,
            Err(e) => {
                error!(event = "identity_load_failed", error = %e);
                self.refused.set(true);
                return;
            }
        };
        let Some(ident) = ident else { return };
        let id = ident.secret.public();
        let Some(app) = AppEntry::find(self.apps, &ident.app) else {
            // Minted by a build with an app this one lacks: refuse rather than run another
            // app on its storage.
            error!(event = "unknown_app", endpoint = %id.to_z32(), app = %ident.app);
            self.refused.set(true);
            return;
        };
        if !self.id_matches_name(&id) {
            // §13.4.4: fatal configuration error. Refuse everything, log once.
            error!(
                event = "identity_invariant_violated",
                object = %self.state.id(),
                stored_public = %id.to_z32(),
            );
            self.refused.set(true);
            return;
        }
        *self.table.borrow_mut() = Some(Sockets::new(IngressConfig::new(id)));
        *self.me.borrow_mut() = Some(Me {
            id,
            app,
            secret: ident.secret,
            relay_url: ident.relay_url,
            pkarr_relay: ident.pkarr_relay,
        });
    }

    /// Rebuilds the socket table from the hibernated sockets' attachments; the table closes
    /// any socket whose attachment is missing or whose handshake deadline passed. Returns the
    /// number of sockets restored and closed.
    fn restore_sessions(&self) -> (usize, usize) {
        let local = self.me.borrow().as_ref().map(|m| m.id);
        let Some(local) = local else {
            // No identity: nothing may be served. Sockets cannot exist
            // (upgrades are refused), but close defensively.
            let mut closed = 0;
            for ws in self.state.get_websockets() {
                let _ = ws.close(Some(CLOSE_REFUSED), Some("no identity"));
                closed += 1;
            }
            return (0, closed);
        };
        links::reset(&local);
        let t = now();
        let mut closed = 0;
        for ws in self.state.get_websockets() {
            let att = match ws.deserialize_attachment::<Attachment>() {
                Ok(Some(a)) if a.v == 1 => a,
                _ => {
                    let _ = ws.close(Some(close_code::PROTOCOL_ERROR), Some("lost session"));
                    closed += 1;
                    continue;
                }
            };
            self.sockets.borrow_mut().insert(att.sid.clone(), ws);
            self.with_table(|t_| {
                let fx = t_.restore(att.sid, Some(att.s), t);
                closed += fx.iter().filter(|e| matches!(e, Effect::Close(..))).count();
                fx
            });
        }
        (self.table.borrow().as_ref().map_or(0, |t| t.len()), closed)
    }

    fn sid_of(&self, ws: &WebSocket) -> Option<String> {
        self.state
            .get_tags(ws)
            .into_iter()
            .find_map(|t| t.strip_prefix(TAG_SID).map(str::to_owned))
    }

    async fn upgrade(&self, req: Request) -> Result<Response> {
        // §13.4.5: no identity → reject without writing storage.
        if self.me.borrow().is_none() {
            return Response::error("not found", 404);
        }
        let proto = req.headers().get("sec-websocket-protocol")?;
        let path = req.path();
        let method = req.method().to_string();
        let ureq = UpgradeRequest {
            method: &method,
            path: &path,
            sec_websocket_protocol: proto.as_deref(),
        };
        let mut rng = rand_core::UnwrapErr(getrandom::SysRng);
        let Some(sid) = random_sid() else {
            return Response::error("internal error", 500);
        };
        let upgraded = {
            let mut table = self.table.borrow_mut();
            let Some(table) = table.as_mut() else {
                return Response::error("not found", 404);
            };
            match table.upgrade(sid.clone(), &ureq, &mut rng, now()) {
                Ok(u) => u,
                Err(rej) => return Response::error(rej.to_string(), rej.http_status()),
            }
        };
        let resp = upgraded.response;
        let pair = WebSocketPair::new()?;
        let sid_tag = format!("{TAG_SID}{sid}");
        self.state
            .accept_websocket_with_tags(&pair.server, &[TAG_RELAY, &sid_tag]);
        // Registered before any fallible or awaited step: a message on an
        // accepted socket must always find its session.
        self.sockets
            .borrow_mut()
            .insert(sid.clone(), pair.server.clone());
        let deadline = self.apply(upgraded.effects);
        if let Err(e) = pair.server.send_with_bytes(resp.challenge.as_bytes()) {
            error!(event = "upgrade_failed", endpoint = %self.endpoint(), error = %e);
            let _ = pair.server.close(Some(1011), Some("internal error"));
            self.sockets.borrow_mut().remove(&sid);
            self.with_table(|t| t.closed(&sid));
            return Err(e);
        }
        // Handshake deadline: enforced lazily by `on_message`, and for silent
        // sockets by the alarm-driven sweep (one schedule row, earliest wins).
        // Failing to arm it only delays that sweep, so the socket stays usable.
        if let Some(deadline) = deadline
            && let Err(e) = self.schedule_sweep(deadline.as_millis() as f64).await
        {
            error!(event = "schedule_failed", endpoint = %self.endpoint(), error = %e);
        }
        let (name, value) = resp.subprotocol_header();
        let mut out = Response::from_websocket(pair.client)?;
        out.headers_mut().set(name, value)?;
        Ok(out)
    }

    async fn internal_mint(&self, mut req: Request) -> Result<Response> {
        if self.refused.get() {
            return Response::error("refused", 500);
        }
        if self.me.borrow().is_some() {
            return Response::error("identity exists", 409);
        }
        let body: MintRequest = req.json().await?;
        let Some(app) = AppEntry::find(self.apps, &body.app) else {
            return Response::error("unknown app", 400);
        };
        let bytes = data_encoding::HEXLOWER
            .decode(body.secret_hex.as_bytes())
            .map_err(|_| worker::Error::RustError("bad secret".into()))?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| worker::Error::RustError("bad secret length".into()))?;
        let secret = SecretKey::from_bytes(&arr);
        let id = secret.public();
        // The key must belong to this object: public(secret) == E == its name.
        let claimed = req.headers().get(NAME_HEADER)?;
        if claimed.as_deref() != Some(id.to_z32().as_str()) || !self.id_matches_name(&id) {
            error!(event = "mint_name_mismatch", object = %self.state.id());
            return Response::error("name mismatch", 400);
        }
        // Re-check after the `req.json()` await: the input gate stays open
        // during non-storage awaits, so a concurrent mint could have run.
        if self.me.borrow().is_some() || self.store.load_identity()?.is_some() {
            return Response::error("identity exists", 409);
        }
        let ident = Identity {
            app: app.name.to_string(),
            secret: secret.clone(),
            relay_url: body.relay_url.clone(),
            pkarr_relay: body.pkarr_relay.clone(),
        };
        let t0 = now_ms();
        if let Err(e) = self.mint_atomically(&ident, &app, t0) {
            // Nothing was stored: the object is as unminted as before, and the router
            // passes this status on and does not register the id.
            error!(event = "mint_failed", endpoint = %id.to_z32(), app = app.name, error = %e);
            self.forget_identity(&id);
            return Response::error("mint failed", 503);
        }
        let (published, _) = self.publish_now().await;
        self.register(body.label.clone()).await;
        info!(
            event = "mint",
            endpoint = %id.to_z32(),
            app = app.name,
            published,
            label = %body.label,
            ms = now_ms() - t0,
        );
        Response::from_json(&MintResponse {
            id: id.to_z32(),
            app: app.name.to_string(),
            relay_url: body.relay_url,
            published,
        })
    }

    /// Stores the identity, creates the tables, and builds the node and the app, in one
    /// transaction: if any step fails, or the app's constructor panics, nothing is stored.
    fn mint_atomically(
        &self,
        ident: &Identity,
        app: &AppEntry,
        t0: f64,
    ) -> std::result::Result<(), String> {
        let Some(tx) = &self.tx else {
            return Err("no transactionSync on this runtime".into());
        };
        tx.transaction(|| {
            let e = |e: worker::Error| e.to_string();
            self.store.insert_identity(ident, t0).map_err(e)?;
            self.store.create_schema((app.schema)()).map_err(e)?;
            self.wakes.set(self.store.bump("wakes").map_err(e)?);
            self.load();
            if self.me.borrow().is_none() {
                return Err("the stored identity did not load".into());
            }
            // `App::new` runs here, so that work it schedules is part of the transaction.
            self.ensure_node();
            if self.node.borrow().is_none() {
                return Err("no node".into());
            }
            Ok(())
        })
    }

    /// Drops what a failed mint loaded into memory; its storage was rolled back.
    fn forget_identity(&self, id: &EndpointId) {
        self.table.borrow_mut().take();
        self.node.borrow_mut().take();
        self.me.borrow_mut().take();
        node::unregister(id);
        links::reset(id);
        self.refused.set(false);
        self.wakes.set(0);
    }

    /// Destroys this endpoint: closes every socket, drops the node, deletes the
    /// alarm and all storage, identity included. Afterwards the object is
    /// indistinguishable from one that was never minted: it refuses traffic and
    /// writes nothing (§13.4.5). Answers 404 if there is no identity to destroy.
    async fn internal_destroy(&self, req: Request) -> Result<Response> {
        let Some(name) = req.headers().get(NAME_HEADER)? else {
            return Response::error("missing name", 400);
        };
        let is_me = match self.env.durable_object(ENDPOINTS_BINDING) {
            Ok(ns) => ns.id_from_name(&name).is_ok_and(|id| id == self.state.id()),
            Err(_) => false,
        };
        if !is_me {
            error!(event = "destroy_name_mismatch", object = %self.state.id());
            return Response::error("name mismatch", 400);
        }
        // A refused object (identity invariant violated) holds an identity too.
        if self.me.borrow().is_none() && !self.refused.get() {
            return Response::error("not found", 404);
        }
        let t0 = now_ms();
        let local = self.me.borrow().as_ref().map(|m| m.id);
        let sockets = self.state.get_websockets();
        for ws in &sockets {
            let _ = ws.close(Some(CLOSE_DESTROYED), Some("destroyed"));
        }
        if let Some(t) = self.table.borrow_mut().as_mut() {
            t.clear();
        }
        self.sockets.borrow_mut().clear();
        if let Some(local) = local {
            links::reset(&local);
            // Node tasks still winding down then find no storage to write to.
            node::unregister(&local);
        }
        self.node.borrow_mut().take();
        self.me.borrow_mut().take();
        self.refused.set(false);
        self.wakes.set(0);
        let storage = self.state.storage();
        storage.delete_alarm().await?;
        storage.delete_all().await?;
        info!(
            event = "destroy",
            endpoint = %name,
            sockets = sockets.len(),
            ms = now_ms() - t0,
        );
        Response::from_json(&DestroyResponse {
            id: name,
            destroyed: true,
        })
    }

    /// Records this endpoint in the registry and sets the `registered` flag.
    /// A failure is logged; the next wake tries again.
    async fn register(&self, label: String) {
        let Some((id, app_name)) = self
            .me
            .borrow()
            .as_ref()
            .map(|m| (m.id, m.app.name.to_string()))
        else {
            return;
        };
        let entry = crate::registry::Entry {
            id: id.to_z32(),
            app: app_name,
            label,
            minted_at: self.store.minted_at().unwrap_or(0),
        };
        register_entry(&self.env, &self.store, &entry, "mint").await;
    }

    /// Publish + confirm, record the outcome, schedule the next attempt. Returns whether the
    /// record was published, and whether the next attempt was scheduled.
    async fn publish_now(&self) -> (bool, Result<()>) {
        let (secret, relay_url, pkarr_relay) = {
            let me = self.me.borrow();
            let Some(m) = me.as_ref() else {
                return (false, Ok(()));
            };
            (m.secret.clone(), m.relay_url.clone(), m.pkarr_relay.clone())
        };
        if pkarr_relay.is_empty() {
            return (false, Ok(()));
        }
        let t0 = now_ms();
        let outcome = pkarr_publish::publish(&secret, &relay_url, &pkarr_relay).await;
        let t = now_ms();
        let (ok, status) = match &outcome {
            Ok(()) => (true, "ok".to_string()),
            Err(e) => (false, e.clone()),
        };
        let failures = self.store.pkarr_result(ok, &status, t).unwrap_or(0);
        let next = if ok {
            t + REPUBLISH_INTERVAL_MS as f64
        } else {
            // Linear backoff with jitter (note 004 §4), capped at the interval.
            let jitter = (now_ms() as u64 % 1000) as f64 * 30.0;
            let backoff = (RETRY_BASE_MS * failures.max(1) as u64).min(REPUBLISH_INTERVAL_MS);
            t + backoff as f64 + jitter
        };
        let endpoint = secret.public().to_z32();
        let next_in_s = ((next - t) / 1000.0).round();
        if ok {
            info!(
                event = "pkarr_publish",
                endpoint,
                ok,
                ms = t - t0,
                next_in_s
            );
        } else {
            warn!(
                event = "pkarr_publish",
                endpoint,
                ok,
                ms = t - t0,
                status,
                failures,
                next_in_s,
            );
        }
        let scheduled = self.schedule(SCHED_REPUBLISH, next, "").await;
        if let Err(e) = &scheduled {
            error!(event = "schedule_failed", endpoint = %self.endpoint(), error = %e);
        }
        (ok, scheduled)
    }

    /// Close sockets still unauthenticated past their handshake deadline;
    /// re-schedule for the earliest remaining one.
    fn sweep_unauthenticated(&self) -> Result<()> {
        // All remaining deadlines are known here, so this replaces the row, which the alarm
        // leased to its retry time, rather than keeping the earlier of the two.
        if let Some(d) = self.with_table(|t| t.sweep(now())) {
            self.store.schedule(SCHED_SWEEP, d.as_millis() as f64, "")?;
        }
        Ok(())
    }

    /// Runs the library's work of one leased schedule row.
    async fn run_due(&self, d: &Due, _attempt: u32) -> std::result::Result<(), String> {
        if d.key == SCHED_REPUBLISH {
            // A failed publish schedules its own retry; only failing to schedule it fails.
            self.publish_now().await.1.map_err(|e| e.to_string())
        } else if d.key == SCHED_SWEEP {
            self.sweep_unauthenticated().map_err(|e| e.to_string())
        } else {
            warn!(event = "bad_schedule_key", key = %d.key);
            Ok(())
        }
    }

    /// Runs the app's work of the leased row `d` in a task, which deletes the row once the
    /// work succeeded and re-arms the alarm. The endpoint stays bound while it runs.
    fn spawn_app_alarm(&self, d: &Due, attempt: u32) {
        self.ensure_node();
        let Some(node) = self.node.borrow().clone() else {
            return;
        };
        let (store, storage, endpoint) =
            (self.store.clone(), self.state.storage(), self.endpoint());
        let d = Due {
            key: d.key.clone(),
            due_ms: d.due_ms,
            payload: d.payload.clone(),
            attempt: d.attempt,
        };
        wasm_bindgen_futures::spawn_local(async move {
            let t0 = now_ms();
            let res = match node.alarm(&d.key, &d.payload, attempt).await {
                Ok(()) => store.claim(&d).map_err(|e| format!("claim: {e}")),
                Err(e) => Err(e.to_string()),
            };
            match res {
                Ok(()) => info!(
                    event = "alarm_app",
                    endpoint,
                    attempt,
                    ok = true,
                    ms = now_ms() - t0
                ),
                Err(e) => error!(
                    event = "alarm_entry_failed",
                    endpoint,
                    key = "app",
                    attempt,
                    retry_in_s = ((d.due_ms - now_ms()) / 1000.0).round(),
                    error = %e,
                ),
            }
            if let Err(e) = node::rearm(&store, &storage).await {
                error!(event = "set_alarm_failed", error = %e);
            }
        });
    }

    /// Schedule the unauthenticated-socket sweep (earliest wins) and arm the alarm.
    async fn schedule_sweep(&self, due_ms: f64) -> Result<()> {
        self.store.schedule_earliest(SCHED_SWEEP, due_ms)?;
        self.rearm().await
    }

    /// Add/replace a schedule entry and (re)arm the single DO alarm.
    pub(crate) async fn schedule(&self, key: &str, due_ms: f64, payload: &str) -> Result<()> {
        self.store.schedule(key, due_ms, payload)?;
        self.rearm().await
    }

    async fn rearm(&self) -> Result<()> {
        node::rearm(&self.store, &self.state.storage()).await
    }
}

impl Object {
    /// The class constructor: rebuilds the object from storage and its sockets.
    pub fn new(state: State, env: Env, apps: &'static [AppEntry]) -> Self {
        super::logging::install(&env);
        super::timers::install();
        let store = Store::new(state.storage().sql());
        let tx = TxStorage::of(&state);
        let this = Self {
            state,
            env,
            apps,
            tx,
            store,
            me: RefCell::new(None),
            refused: Cell::new(false),
            table: RefCell::new(None),
            sockets: RefCell::new(HashMap::new()),
            node: RefCell::new(None),
            wakes: Cell::new(0),
        };
        this.load();
        if this.me.borrow().is_some() {
            // Only objects with an identity ever write (§13.4.5).
            let schema = this
                .me
                .borrow()
                .as_ref()
                .map_or(&[][..], |m| (m.app.schema)());
            if let Err(e) = this.store.create_schema(schema) {
                error!(event = "schema_failed", endpoint = %this.endpoint(), error = %e);
            }
            match this.store.bump("wakes") {
                Ok(n) => this.wakes.set(n),
                Err(e) => {
                    error!(event = "wakes_bump_failed", endpoint = %this.endpoint(), error = %e)
                }
            }
            this.note_evicted_active();
        }
        let (restored, closed) = this.restore_sessions();
        this.ensure_node();
        this.backfill_registry();
        info!(
            event = "wake",
            endpoint = %this.endpoint(),
            minted = this.me.borrow().is_some(),
            wakes = this.wakes.get(),
            sockets = restored,
            sockets_closed = closed,
            refused = this.refused.get(),
        );
        this
    }

    /// The class's `fetch`: relay upgrades and the router's internal calls.
    pub async fn fetch(&self, req: Request) -> Result<Response> {
        let path = req.path();
        if path == INTERNAL_MINT && req.method() == worker::Method::Post {
            return self.internal_mint(req).await;
        }
        if path == INTERNAL_DESTROY && req.method() == worker::Method::Post {
            return self.internal_destroy(req).await;
        }
        if self.refused.get() {
            return Response::error("refused", 503);
        }
        if path == INTERNAL_STATUS {
            // An unminted object answers 404 without writing (§13.4.5).
            self.ensure_node();
            let Some(node) = self.node.borrow().clone() else {
                return Response::error("not found", 404);
            };
            let h = worker::Headers::new();
            h.set("content-type", "application/json")?;
            return Ok(Response::ok(node.status_json())?.with_headers(h));
        }
        self.upgrade(req).await
    }

    /// The class's `webSocketMessage`.
    pub async fn websocket_message(
        &self,
        ws: WebSocket,
        msg: WebSocketIncomingMessage,
    ) -> Result<()> {
        if self.refused.get() || self.me.borrow().is_none() {
            let _ = ws.close(Some(CLOSE_REFUSED), Some("refused"));
            return Ok(());
        }
        let Some(sid) = self.sid_of(&ws) else {
            let _ = ws.close(Some(close_code::PROTOCOL_ERROR), Some("unknown socket"));
            return Ok(());
        };
        if !self.sockets.borrow().contains_key(&sid) {
            // Accepted by an earlier instance and not restored: the table answers with a close.
            self.sockets.borrow_mut().insert(sid.clone(), ws);
        }
        let bytes = match msg {
            WebSocketIncomingMessage::Binary(b) => b,
            WebSocketIncomingMessage::String(_) => {
                self.with_table(|t| t.text(&sid));
                return Ok(());
            }
        };
        let t = now();
        self.with_table(|table| table.message(&sid, &bytes, t));
        Ok(())
    }

    /// The class's `webSocketClose`.
    pub async fn websocket_close(
        &self,
        ws: WebSocket,
        code: usize,
        reason: String,
        clean: bool,
    ) -> Result<()> {
        if let Some(sid) = self.sid_of(&ws) {
            info!(
                event = "socket_close",
                endpoint = %self.endpoint(),
                peer = %self.peer_of(&sid).map(|p| p.to_z32()).unwrap_or_default(),
                by = "peer",
                code,
                reason,
                clean,
            );
            self.sockets.borrow_mut().remove(&sid);
            self.with_table(|t| t.closed(&sid));
        }
        // Complete the close handshake (compat dates before auto-reply).
        let _ = ws.close(Some(crate::route::sendable_close_code(code)), Some("bye"));
        Ok(())
    }

    /// The class's `webSocketError`.
    pub async fn websocket_error(&self, ws: WebSocket, error: worker::Error) -> Result<()> {
        if let Some(sid) = self.sid_of(&ws) {
            warn!(
                event = "socket_close",
                endpoint = %self.endpoint(),
                peer = %self.peer_of(&sid).map(|p| p.to_z32()).unwrap_or_default(),
                by = "error",
                error = %error,
            );
            self.sockets.borrow_mut().remove(&sid);
            self.with_table(|t| t.closed(&sid));
        }
        Ok(())
    }

    /// The class's `alarm`.
    ///
    /// Before any work runs, each due row is leased: moved to its retry time, with its attempt
    /// counted, and the alarm is armed for the result. A row is deleted only once its work has
    /// succeeded. Work that fails, and work cut short by a trap or an eviction, therefore runs
    /// again after [`alarm_retry_delay`], and the rows after it are not lost with it.
    ///
    /// The library's own work runs inside the handler. The app's runs in tasks started as the
    /// handler returns: workers-rs leaves the promise of a handler whose instance traps
    /// unsettled, and the runtime delivers no further alarm while one is unsettled, so app
    /// work that panicked inside the handler would never be retried
    /// (`tests/workerd/fault-test.mjs`).
    pub async fn alarm(&self) -> Result<Response> {
        if self.refused.get() || self.me.borrow().is_none() {
            // No work can be stored without an identity. The alarm of a mint that rolled back,
            // or of a refused object, is dropped; that writes nothing.
            self.state.storage().delete_alarm().await?;
            return Response::ok("refused");
        }
        let t0 = now_ms();
        let mut leased = Vec::new();
        for d in self.store.due(t0)? {
            let retry =
                now_ms() + alarm_retry_delay(ALARM_RETRY_BASE, d.attempt).as_millis() as f64;
            // An error here makes the runtime retry the alarm; every row is still stored.
            if let Some(l) = self.store.lease(&d, retry)? {
                leased.push((d.attempt, l));
            }
        }
        self.rearm().await?;
        let mut failed = 0;
        for (attempt, d) in &leased {
            if d.key.starts_with(APP_KEY_PREFIX) {
                continue;
            }
            let res = match self.run_due(d, *attempt).await {
                Ok(()) => self.store.claim(d).map_err(|e| format!("claim: {e}")),
                Err(e) => Err(e),
            };
            if let Err(e) = res {
                failed += 1;
                error!(
                    event = "alarm_entry_failed",
                    endpoint = %self.endpoint(),
                    key = kind(&d.key),
                    attempt,
                    retry_in_s = ((d.due_ms - now_ms()) / 1000.0).round(),
                    error = %e,
                );
            }
        }
        let kinds = leased
            .iter()
            .map(|(_, d)| kind(&d.key))
            .collect::<Vec<_>>()
            .join(",");
        info!(
            event = "alarm",
            endpoint = %self.endpoint(),
            due = kinds,
            ran = leased.len(),
            failed,
            ms = now_ms() - t0,
        );
        self.rearm().await?;
        // Last, with nothing awaited after it: the tasks first run once this handler has
        // returned, so that a trap in them cannot leave the alarm event unsettled.
        for (attempt, d) in &leased {
            if d.key.starts_with(APP_KEY_PREFIX) {
                self.spawn_app_alarm(d, *attempt);
            }
        }
        Response::ok("ok")
    }
}

/// A schedule key for logs: an app's key may carry a peer's EndpointId, so app work is `app`.
fn kind(key: &str) -> &str {
    if key.starts_with(APP_KEY_PREFIX) {
        "app"
    } else {
        key
    }
}

/// `from` is `mint` or `backfill`.
async fn register_entry(
    env: &Env,
    store: &Store,
    entry: &crate::registry::Entry,
    from: &'static str,
) {
    let res = match registry::register(env, entry).await {
        Ok(()) => store.set_registered(),
        Err(e) => Err(e),
    };
    match res {
        Ok(()) => info!(event = "registry_register", endpoint = %entry.id, from, ok = true),
        Err(e) => warn!(
            event = "registry_register",
            endpoint = %entry.id,
            from,
            ok = false,
            error = %e,
        ),
    }
}

impl Object {
    /// Registers an endpoint minted before the registry existed, once
    /// (design note 009). Runs from the constructor without blocking it.
    fn backfill_registry(&self) {
        if self.refused.get() {
            return;
        }
        let Some((id, app)) = self
            .me
            .borrow()
            .as_ref()
            .map(|m| (m.id, m.app.name.to_string()))
        else {
            return;
        };
        if self.store.registered().unwrap_or(true) {
            return;
        }
        let entry = crate::registry::Entry {
            id: id.to_z32(),
            app,
            label: String::new(),
            minted_at: self.store.minted_at().unwrap_or(0),
        };
        let (env, store) = (self.env.clone(), self.store.clone());
        wasm_bindgen_futures::spawn_local(async move {
            register_entry(&env, &store, &entry, "backfill").await;
        });
    }

    /// Logs `evicted_active` if the previous instance ended while its `Endpoint` was bound:
    /// evicted, crashed or trapped with QUIC connections that its peers now wait out
    /// (`docs/design.md`, Failure handling). A constructor runs before any `Endpoint`, so
    /// the marker it finds is always the previous instance's.
    fn note_evicted_active(&self) {
        match self.store.counter_opt(node::COUNTER_BOUND_AT) {
            Ok(Some(at)) => {
                warn!(
                    event = "evicted_active",
                    endpoint = %self.endpoint(),
                    since_bind_ms = now_ms() as i64 - at,
                    wakes = self.wakes.get(),
                );
                if let Err(e) = self.store.delete_counter(node::COUNTER_BOUND_AT) {
                    error!(event = "bound_marker_failed", endpoint = %self.endpoint(), error = %e);
                }
            }
            Ok(None) => {}
            Err(e) => {
                error!(event = "bound_marker_failed", endpoint = %self.endpoint(), error = %e)
            }
        }
    }

    /// Creates the node once an identity is loaded. Cheap: no `Endpoint` is
    /// bound until the first datagram or dial-back.
    fn ensure_node(&self) {
        if self.node.borrow().is_some() {
            return;
        }
        let me = self.me.borrow();
        let Some(me) = me.as_ref() else { return };
        node::register(
            me.id,
            self.store.clone(),
            self.state.storage(),
            TxStorage::of(&self.state),
        );
        let bundle = self
            .env
            .var("BUNDLE_SHA256")
            .map(|v| v.to_string())
            .unwrap_or_default();
        let wakes = u64::try_from(self.wakes.get()).unwrap_or(0);
        let mut config = NodeConfig::new(me.secret.clone(), wakes, bundle);
        config.endpoint_builds = self
            .store
            .counter(node::COUNTER_ENDPOINT_BUILDS)
            .ok()
            .and_then(|n| u64::try_from(n).ok())
            .unwrap_or(0);
        let n = Node::new(
            config,
            std::sync::Arc::new(DoHost { local: me.id }),
            &me.app,
        );
        for peer in links::peers(&me.id) {
            n.socket_authenticated(peer);
        }
        *self.node.borrow_mut() = Some(n);
    }
}
