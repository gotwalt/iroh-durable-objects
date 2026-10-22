//! The `room` Durable Object class (spec §5.5–§5.8, §13.4).
//!
//! One object per EndpointId. It accepts peers' relay WebSockets as
//! hibernatable sockets, runs one ingress session per socket, and hands
//! authenticated peers and their datagrams to `iroh_do_core::Node`, which owns
//! the iroh `Endpoint`.
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
    Close, IngressConfig, IngressSession, Output, SessionSnapshot, Status, UpgradeRequest,
    close_code,
};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};
use worker::{
    DurableObject, Env, Request, Response, Result, State, WebSocket, WebSocketIncomingMessage,
    WebSocketPair, durable_object,
};

use iroh_do_chat_app::ChatApp;
use iroh_do_core::{APP_KEY_PREFIX, AppEntry, Config as NodeConfig, Node};

use super::{
    links,
    node::{self, DoHost},
    pkarr_publish, registry,
    router::{
        DestroyResponse, INTERNAL_DESTROY, INTERNAL_MINT, INTERNAL_STATUS, MintRequest,
        MintResponse, NAME_HEADER, ROOMS_BINDING,
    },
    store::{Due, Identity, Store},
};
use crate::pkarr::{REPUBLISH_INTERVAL_MS, RETRY_BASE_MS};

/// Tag on every relay socket (tags are fixed at accept time, before the peer
/// is known, so peer lookup goes through the attachment instead).
const TAG_RELAY: &str = "relay";
/// Tag prefix carrying the socket's random id, used to find its session.
const TAG_SID: &str = "sid:";
/// Close code for a socket superseded by a newer one from the same peer
/// (private-use range, RFC 6455 §7.4.2).
const CLOSE_SUPERSEDED: u16 = 4000;
/// Close code when the DO refuses traffic (no/invalid identity).
const CLOSE_REFUSED: u16 = 1011;
/// Close code for sockets of a destroyed endpoint (normal closure).
const CLOSE_DESTROYED: u16 = 1000;

/// Schedule key for the pkarr republish.
pub const SCHED_REPUBLISH: &str = "republish";
/// Schedule key for the unauthenticated-socket sweep (handshake deadline).
/// No `setTimeout`: pending timers would block hibernation.
pub const SCHED_SWEEP: &str = "sweep";
/// Delay before a schedule row whose handler failed is retried.
const ALARM_RETRY_MS: f64 = 30_000.0;

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

struct Slot {
    ws: WebSocket,
    session: IngressSession,
}

/// Loaded, verified identity of this object.
struct Me {
    id: EndpointId,
    secret: SecretKey,
    relay_url: String,
    pkarr_relay: String,
    cfg: IngressConfig,
}

/// An iroh endpoint as a Durable Object, exported to JS as `room`.
#[allow(non_camel_case_types)]
#[durable_object]
pub struct room {
    state: State,
    env: Env,
    store: Store,
    me: RefCell<Option<Me>>,
    /// The stored identity exists but violates the name invariant (§13.4.4).
    refused: Cell<bool>,
    sessions: RefCell<HashMap<String, Slot>>,
    node: RefCell<Option<Node>>,
    /// Persisted constructor-run count at this wake.
    wakes: Cell<i64>,
}

impl room {
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
        self.sessions
            .borrow()
            .get(sid)
            .and_then(|s| s.session.peer())
    }

    fn id_matches_name(&self, id: &EndpointId) -> bool {
        let Ok(ns) = self.env.durable_object(ROOMS_BINDING) else {
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
        *self.me.borrow_mut() = Some(Me {
            id,
            cfg: IngressConfig::new(id),
            secret: ident.secret,
            relay_url: ident.relay_url,
            pkarr_relay: ident.pkarr_relay,
        });
    }

    /// Rebuilds sessions from the hibernated sockets' attachments, closing any
    /// socket whose attachment is missing or whose handshake deadline passed.
    /// Returns the number of sockets restored and closed.
    fn restore_sessions(&self) -> (usize, usize) {
        let me = self.me.borrow();
        let Some(me) = me.as_ref() else {
            // No identity: nothing may be served. Sockets cannot exist
            // (upgrades are refused), but close defensively.
            let mut closed = 0;
            for ws in self.state.get_websockets() {
                let _ = ws.close(Some(CLOSE_REFUSED), Some("no identity"));
                closed += 1;
            }
            return (0, closed);
        };
        let mut closed = 0;
        links::reset(&me.id);
        let t = now();
        let mut sessions = self.sessions.borrow_mut();
        for ws in self.state.get_websockets() {
            let att = match ws.deserialize_attachment::<Attachment>() {
                Ok(Some(a)) if a.v == 1 => a,
                _ => {
                    let _ = ws.close(Some(close_code::PROTOCOL_ERROR), Some("lost session"));
                    closed += 1;
                    continue;
                }
            };
            let session = IngressSession::restore(att.s, &me.cfg);
            if session.handshake_deadline().is_some_and(|d| d <= t) {
                let _ = ws.close(
                    Some(close_code::POLICY_VIOLATION),
                    Some("handshake timeout"),
                );
                warn!(
                    event = "ingress_auth",
                    endpoint = %me.id.to_z32(),
                    ok = false,
                    reason = "handshake timeout",
                );
                closed += 1;
                continue;
            }
            // Two attachments claiming the same peer (crash between supersede
            // steps): keep the later, close the other.
            if let Some(peer) = session.peer()
                && let Some(old) = links::bind(me.id, peer, ws.clone())
            {
                let _ = old.close(Some(CLOSE_SUPERSEDED), Some("superseded"));
                closed += 1;
            }
            sessions.insert(att.sid, Slot { ws, session });
        }
        (sessions.len(), closed)
    }

    fn sid_of(&self, ws: &WebSocket) -> Option<String> {
        self.state
            .get_tags(ws)
            .into_iter()
            .find_map(|t| t.strip_prefix(TAG_SID).map(str::to_owned))
    }

    async fn upgrade(&self, req: Request) -> Result<Response> {
        let cfg = {
            let me = self.me.borrow();
            match me.as_ref() {
                Some(m) => m.cfg.clone(),
                // §13.4.5: no identity → reject without writing storage.
                None => return Response::error("not found", 404),
            }
        };
        let proto = req.headers().get("sec-websocket-protocol")?;
        let path = req.path();
        let method = req.method().to_string();
        let ureq = UpgradeRequest {
            method: &method,
            path: &path,
            sec_websocket_protocol: proto.as_deref(),
        };
        let mut rng = rand_core::UnwrapErr(getrandom::SysRng);
        let (resp, session) = match IngressSession::on_upgrade(&ureq, &cfg, &mut rng, now()) {
            Ok(x) => x,
            Err(rej) => return Response::error(rej.to_string(), rej.http_status()),
        };
        let Some(sid) = random_sid() else {
            return Response::error("internal error", 500);
        };
        let pair = WebSocketPair::new()?;
        let sid_tag = format!("{TAG_SID}{sid}");
        let snapshot = session.snapshot();
        let deadline = session.handshake_deadline();
        self.state
            .accept_websocket_with_tags(&pair.server, &[TAG_RELAY, &sid_tag]);
        // Registered before any fallible or awaited step: a message on an
        // accepted socket must always find its session.
        self.sessions.borrow_mut().insert(
            sid.clone(),
            Slot {
                ws: pair.server.clone(),
                session,
            },
        );
        let opened = (|| {
            if let Some(snap) = snapshot {
                pair.server.serialize_attachment(Attachment {
                    v: 1,
                    sid: sid.clone(),
                    s: snap,
                })?;
            }
            pair.server.send_with_bytes(resp.challenge.as_bytes())
        })();
        if let Err(e) = opened {
            error!(event = "upgrade_failed", endpoint = %self.endpoint(), error = %e);
            let _ = pair.server.close(Some(1011), Some("internal error"));
            self.drop_session(&sid);
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

    /// Closes a socket the ingress session rejected. Before authentication that is a
    /// failed handshake, logged as `ingress_auth`.
    fn close_with(&self, ws: &WebSocket, sid: &str, close: &Close) {
        match self.peer_of(sid) {
            None => warn!(
                event = "ingress_auth",
                endpoint = %self.endpoint(),
                ok = false,
                code = close.code,
                reason = close.reason_text(),
            ),
            Some(peer) => info!(
                event = "socket_close",
                endpoint = %self.endpoint(),
                peer = %peer.to_z32(),
                by = "object",
                code = close.code,
                reason = close.reason_text(),
            ),
        }
        if let Some(f) = &close.frame {
            let _ = ws.send_with_bytes(f.as_bytes());
        }
        let _ = ws.close(Some(close.code), Some(close.reason_text()));
        self.drop_session(sid);
    }

    fn drop_session(&self, sid: &str) {
        let removed = self.sessions.borrow_mut().remove(sid);
        let Some(peer) = removed.and_then(|slot| slot.session.peer()) else {
            return;
        };
        let Some(local) = self.me.borrow().as_ref().map(|m| m.id) else {
            return;
        };
        // Only unbind if no newer socket of the same peer took over.
        if self
            .sessions
            .borrow()
            .values()
            .all(|s| s.session.peer() != Some(peer))
        {
            links::unbind(&local, &peer);
            if let Some(node) = self.node.borrow().as_ref() {
                node.socket_closed(peer);
            }
        }
    }

    fn on_authenticated(&self, sid: &str, peer: EndpointId) {
        let Some(local) = self.me.borrow().as_ref().map(|m| m.id) else {
            return;
        };
        // Supersede any older socket from the same peer (§5.2 item 8).
        let older: Vec<String> = self
            .sessions
            .borrow()
            .iter()
            .filter(|(k, s)| k.as_str() != sid && s.session.peer() == Some(peer))
            .map(|(k, _)| k.clone())
            .collect();
        let superseded = older.len();
        for k in older {
            if let Some(slot) = self.sessions.borrow_mut().remove(&k) {
                if let Some(f) = slot.session.status_frame(Status::SameEndpointIdConnected) {
                    let _ = slot.ws.send_with_bytes(f.as_bytes());
                }
                let _ = slot.ws.close(Some(CLOSE_SUPERSEDED), Some("superseded"));
            }
        }
        let sessions = self.sessions.borrow();
        let Some(slot) = sessions.get(sid) else {
            return;
        };
        if let Some(snap) = slot.session.snapshot()
            && let Err(e) = slot.ws.serialize_attachment(Attachment {
                v: 1,
                sid: sid.to_owned(),
                s: snap,
            })
        {
            warn!(event = "attachment_write_failed", endpoint = %local.to_z32(), error = %e);
        }
        info!(
            event = "ingress_auth",
            endpoint = %local.to_z32(),
            ok = true,
            peer = %peer.to_z32(),
            superseded,
        );
        links::bind(local, peer, slot.ws.clone());
        drop(sessions);
        self.ensure_node();
        if let Some(node) = self.node.borrow().as_ref() {
            node.socket_authenticated(peer);
        }
    }

    async fn internal_mint(&self, mut req: Request) -> Result<Response> {
        if self.refused.get() {
            return Response::error("refused", 500);
        }
        if self.me.borrow().is_some() {
            return Response::error("identity exists", 409);
        }
        let body: MintRequest = req.json().await?;
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
            secret: secret.clone(),
            relay_url: body.relay_url.clone(),
            pkarr_relay: body.pkarr_relay.clone(),
        };
        let t0 = now_ms();
        self.store.insert_identity(&ident, t0)?;
        self.store.create_schema((app().schema)())?;
        self.wakes.set(self.store.bump("wakes")?);
        self.load();
        self.ensure_node();
        let published = self.publish_now().await;
        self.register(body.label.clone()).await;
        info!(
            event = "mint",
            endpoint = %id.to_z32(),
            published,
            label = %body.label,
            ms = now_ms() - t0,
        );
        Response::from_json(&MintResponse {
            id: id.to_z32(),
            relay_url: body.relay_url,
            published,
        })
    }

    /// Destroys this endpoint: closes every socket, drops the node, deletes the
    /// alarm and all storage, identity included. Afterwards the object is
    /// indistinguishable from one that was never minted: it refuses traffic and
    /// writes nothing (§13.4.5). Answers 404 if there is no identity to destroy.
    async fn internal_destroy(&self, req: Request) -> Result<Response> {
        let Some(name) = req.headers().get(NAME_HEADER)? else {
            return Response::error("missing name", 400);
        };
        let is_me = match self.env.durable_object(ROOMS_BINDING) {
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
        self.sessions.borrow_mut().clear();
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
        let Some(id) = self.me.borrow().as_ref().map(|m| m.id) else {
            return;
        };
        let entry = crate::registry::Entry {
            id: id.to_z32(),
            label,
            minted_at: self.store.minted_at().unwrap_or(0),
        };
        register_entry(&self.env, &self.store, &entry, "mint").await;
    }

    /// Publish + confirm, record the outcome, schedule the next attempt.
    async fn publish_now(&self) -> bool {
        let (secret, relay_url, pkarr_relay) = {
            let me = self.me.borrow();
            let Some(m) = me.as_ref() else { return false };
            (m.secret.clone(), m.relay_url.clone(), m.pkarr_relay.clone())
        };
        if pkarr_relay.is_empty() {
            return false;
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
        if let Err(e) = self.schedule(SCHED_REPUBLISH, next, "").await {
            error!(event = "schedule_failed", endpoint = %self.endpoint(), error = %e);
        }
        ok
    }

    /// Close sockets still unauthenticated past their handshake deadline;
    /// re-schedule for the earliest remaining one.
    fn sweep_unauthenticated(&self) -> Result<()> {
        let t = now();
        let expired: Vec<(String, WebSocket)> = self
            .sessions
            .borrow()
            .iter()
            .filter(|(_, s)| s.session.handshake_deadline().is_some_and(|d| d <= t))
            .map(|(k, s)| (k.clone(), s.ws.clone()))
            .collect();
        for (sid, ws) in expired {
            let _ = ws.close(
                Some(close_code::POLICY_VIOLATION),
                Some("handshake timeout"),
            );
            warn!(
                event = "ingress_auth",
                endpoint = %self.endpoint(),
                ok = false,
                reason = "handshake timeout",
            );
            self.drop_session(&sid);
        }
        let next = self
            .sessions
            .borrow()
            .values()
            .filter_map(|s| s.session.handshake_deadline())
            .min();
        if let Some(d) = next {
            self.store
                .schedule_earliest(SCHED_SWEEP, d.as_millis() as f64)?;
        }
        Ok(())
    }

    /// Claim one due schedule row and run it.
    async fn run_due(&self, d: &Due) -> Result<()> {
        self.store.claim(d)?;
        if d.key == SCHED_REPUBLISH {
            self.publish_now().await;
        } else if d.key == SCHED_SWEEP {
            self.sweep_unauthenticated()?;
        } else if d.key.starts_with(APP_KEY_PREFIX) {
            self.ensure_node();
            if let Some(node) = self.node.borrow().as_ref() {
                node.alarm(&d.key, &d.payload);
            }
        } else {
            warn!(event = "bad_schedule_key", key = %d.key);
        }
        Ok(())
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
        match self.store.next_due()? {
            Some(due) => {
                self.state
                    .storage()
                    .set_alarm(at_ms(due.max(now_ms())))
                    .await
            }
            None => Ok(()),
        }
    }
}

impl DurableObject for room {
    fn new(state: State, env: Env) -> Self {
        super::logging::install(&env);
        super::timers::install();
        let store = Store::new(state.storage().sql());
        let this = Self {
            state,
            env,
            store,
            me: RefCell::new(None),
            refused: Cell::new(false),
            sessions: RefCell::new(HashMap::new()),
            node: RefCell::new(None),
            wakes: Cell::new(0),
        };
        this.load();
        if this.me.borrow().is_some() {
            // Only objects with an identity ever write (§13.4.5).
            if let Err(e) = this.store.create_schema((app().schema)()) {
                error!(event = "schema_failed", endpoint = %this.endpoint(), error = %e);
            }
            match this.store.bump("wakes") {
                Ok(n) => this.wakes.set(n),
                Err(e) => {
                    error!(event = "wakes_bump_failed", endpoint = %this.endpoint(), error = %e)
                }
            }
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

    async fn fetch(&self, req: Request) -> Result<Response> {
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

    async fn websocket_message(&self, ws: WebSocket, msg: WebSocketIncomingMessage) -> Result<()> {
        if self.refused.get() || self.me.borrow().is_none() {
            let _ = ws.close(Some(CLOSE_REFUSED), Some("refused"));
            return Ok(());
        }
        let Some(sid) = self.sid_of(&ws) else {
            let _ = ws.close(Some(close_code::PROTOCOL_ERROR), Some("unknown socket"));
            return Ok(());
        };
        let bytes = match msg {
            WebSocketIncomingMessage::Binary(b) => b,
            WebSocketIncomingMessage::String(_) => {
                let close = self
                    .sessions
                    .borrow_mut()
                    .get_mut(&sid)
                    .map(|s| s.session.on_text_message());
                if let Some(c) = close {
                    self.close_with(&ws, &sid, &c);
                }
                return Ok(());
            }
        };
        let t = now();
        // Process synchronously; collect effects that need other borrows.
        let mut authed = None;
        let mut datagrams: Vec<(EndpointId, u8, Option<std::num::NonZeroU16>, Vec<u8>)> =
            Vec::new();
        let result = {
            let mut sessions = self.sessions.borrow_mut();
            let Some(slot) = sessions.get_mut(&sid) else {
                drop(sessions);
                let _ = ws.close(Some(close_code::PROTOCOL_ERROR), Some("lost session"));
                return Ok(());
            };
            let violations = slot.session.dst_violations();
            let result = match slot.session.on_message(&bytes, t) {
                Ok(outputs) => {
                    for out in outputs {
                        match out {
                            // Pong and ServerConfirmsAuth: answered here, no Endpoint needed.
                            Output::Send(frame) => {
                                let _ = ws.send_with_bytes(frame.as_bytes());
                            }
                            Output::Authenticated { peer } => authed = Some(peer),
                            Output::Datagrams { from, batch } => datagrams.push((
                                from,
                                iroh_relay_ingress::Ecn::to_bits(batch.ecn),
                                batch.segment_size,
                                batch.contents.to_vec(),
                            )),
                        }
                    }
                    Ok(())
                }
                Err(close) => Err(close),
            };
            // The count lives in the attachment, so hibernation cannot reset it
            // (§5.2 item 4). Written only when it changes, never per datagram.
            let now_violations = slot.session.dst_violations();
            if result.is_ok() && now_violations != violations {
                warn!(
                    event = "dst_violation",
                    endpoint = %self.endpoint(),
                    peer = %slot.session.peer().map(|p| p.to_z32()).unwrap_or_default(),
                    count = now_violations,
                );
                if let Some(snap) = slot.session.snapshot()
                    && let Err(e) = slot.ws.serialize_attachment(Attachment {
                        v: 1,
                        sid: sid.clone(),
                        s: snap,
                    })
                {
                    warn!(
                        event = "attachment_write_failed",
                        endpoint = %self.endpoint(),
                        error = %e,
                    );
                }
            }
            result
        };
        if let Err(close) = result {
            self.close_with(&ws, &sid, &close);
            return Ok(());
        }
        if let Some(peer) = authed {
            self.on_authenticated(&sid, peer);
        }
        for (from, ecn, seg, contents) in datagrams {
            self.ensure_node();
            if let Some(node) = self.node.borrow().as_ref() {
                node.datagrams(
                    from,
                    ecn,
                    contents.into(),
                    seg.map(|s| usize::from(s.get())),
                );
            }
        }
        Ok(())
    }

    async fn websocket_close(
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
            self.drop_session(&sid);
        }
        // Complete the close handshake (compat dates before auto-reply).
        let _ = ws.close(Some(crate::route::sendable_close_code(code)), Some("bye"));
        Ok(())
    }

    async fn websocket_error(&self, ws: WebSocket, error: worker::Error) -> Result<()> {
        if let Some(sid) = self.sid_of(&ws) {
            warn!(
                event = "socket_close",
                endpoint = %self.endpoint(),
                peer = %self.peer_of(&sid).map(|p| p.to_z32()).unwrap_or_default(),
                by = "error",
                error = %error,
            );
            self.drop_session(&sid);
        }
        Ok(())
    }

    async fn alarm(&self) -> Result<Response> {
        if self.refused.get() || self.me.borrow().is_none() {
            return Response::ok("refused");
        }
        // Each row stays in the table until it is claimed immediately before it
        // runs. A row whose handler returns an error is logged and rescheduled,
        // and the remaining rows still run.
        let t0 = now_ms();
        let due = self.store.due(t0)?;
        let mut failed = 0;
        for d in &due {
            if let Err(e) = self.run_due(d).await {
                failed += 1;
                error!(
                    event = "alarm_entry_failed",
                    endpoint = %self.endpoint(),
                    key = %d.key,
                    error = %e,
                );
                let retry = now_ms() + ALARM_RETRY_MS;
                if let Err(e) = self.store.schedule_earliest(&d.key, retry) {
                    error!(event = "schedule_failed", endpoint = %self.endpoint(), error = %e);
                }
            }
        }
        // Kinds, not keys: an app key may carry a peer's EndpointId.
        let kinds = due
            .iter()
            .map(|d| {
                if d.key.starts_with(APP_KEY_PREFIX) {
                    "app"
                } else {
                    d.key.as_str()
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        info!(
            event = "alarm",
            endpoint = %self.endpoint(),
            due = kinds,
            ran = due.len(),
            failed,
            ms = now_ms() - t0,
        );
        // Republishing must never stop: if no row survived (e.g. its write
        // failed inside publish_now), put one back.
        if !self.store.scheduled(SCHED_REPUBLISH).unwrap_or(true)
            && let Err(e) = self
                .store
                .schedule(SCHED_REPUBLISH, now_ms() + ALARM_RETRY_MS, "")
        {
            error!(event = "schedule_failed", endpoint = %self.endpoint(), error = %e);
        }
        // An error here makes the runtime retry the alarm; every row is still stored.
        self.rearm().await?;
        Response::ok("ok")
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

impl room {
    /// Registers an endpoint minted before the registry existed, once
    /// (design note 009). Runs from the constructor without blocking it.
    fn backfill_registry(&self) {
        if self.refused.get() {
            return;
        }
        let Some(id) = self.me.borrow().as_ref().map(|m| m.id) else {
            return;
        };
        if self.store.registered().unwrap_or(true) {
            return;
        }
        let entry = crate::registry::Entry {
            id: id.to_z32(),
            label: String::new(),
            minted_at: self.store.minted_at().unwrap_or(0),
        };
        let (env, store) = (self.env.clone(), self.store.clone());
        wasm_bindgen_futures::spawn_local(async move {
            register_entry(&env, &store, &entry, "backfill").await;
        });
    }

    /// Creates the node once an identity is loaded. Cheap: no `Endpoint` is
    /// bound until the first datagram or dial-back.
    fn ensure_node(&self) {
        if self.node.borrow().is_some() {
            return;
        }
        let me = self.me.borrow();
        let Some(me) = me.as_ref() else { return };
        node::register(me.id, self.store.clone(), self.state.storage());
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
        let n = Node::new(config, std::sync::Arc::new(DoHost { local: me.id }), &app());
        for peer in links::peers(&me.id) {
            n.socket_authenticated(peer);
        }
        *self.node.borrow_mut() = Some(n);
    }
}

/// The app every endpoint runs.
fn app() -> AppEntry {
    AppEntry::of::<ChatApp>()
}
