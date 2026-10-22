//! `WasmNode`: the relay ingress and the iroh node of one object, driven by the TypeScript
//! class, and `Endpoint`, the app's handle for dialing.
//!
//! The TypeScript side owns the WebSockets (accept, tags, attachments, close) and calls in
//! with socket ids. This side owns what the sockets carry: one [`IngressSession`] per socket,
//! the peer → socket map that the transport writes through, and the [`Node`]. Every outbound
//! frame, including handshake replies and pongs, goes out through `NodeHost.sendFrame`.

use std::{collections::HashMap, sync::Arc, time::Duration};

use iroh::{EndpointId as Id, SecretKey};
use iroh_do_core::{AppEntry, Config, Ctx, Node};
use iroh_relay_ingress::{
    Close, Ecn, IngressConfig, IngressSession, Output, SessionSnapshot, Status, UpgradeRequest,
    close_code,
};
use js_sys::{Object, Promise, Reflect};
use tracing::{info, warn};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

use crate::{
    app::JsApp,
    host::{self, Entry, JsHost, NodeHost},
    ids::{EndpointAddr, EndpointId, peer_from_js},
    js_err,
    streams::Connection,
};

/// Close code for a socket superseded by a newer one from the same peer.
const CLOSE_SUPERSEDED: u16 = 4000;

static APP: AppEntry = AppEntry::of::<JsApp>();

#[wasm_bindgen(typescript_custom_section)]
const TS: &str = r#"
export interface NodeOptions {
  /** Object constructions so far, including this one. */
  wakes: number;
  /** Endpoints bound before this construction. */
  endpointBuilds: number;
  bundle: string;
  /** This endpoint's relay URL, https://<z32>.<zone>/. */
  relayUrl: string;
  /** The app's ALPNs, with an optional idle limit each. */
  protocols: { alpn: string; idleMs?: number }[];
  tIdleMs?: number;
  tSilentMs?: number;
  tLingerMs?: number;
}

/** What the object does with its sockets after a call into the node. */
export interface IngressResult {
  /** Sockets to close, with their close code and reason. Frames are already written. */
  close: { sid: string; code: number; reason: string }[];
  /** New attachment payload (the session snapshot) for the socket the call was about. */
  snapshot?: string;
  /** A handshake deadline (ms since the epoch) the alarm must enforce. */
  deadlineMs?: number;
}

export type UpgradeResult =
  | { ok: true; sid: string; protocol: string; snapshot: string; deadlineMs: number }
  | { ok: false; status: number; message: string };
"#;

fn get(obj: &JsValue, key: &str) -> JsValue {
    Reflect::get(obj, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}

fn set(obj: &Object, key: &str, v: impl Into<JsValue>) {
    let _ = Reflect::set(obj, &JsValue::from_str(key), &v.into());
}

fn ms(v: JsValue) -> Option<Duration> {
    v.as_f64()
        .filter(|m| m.is_finite() && *m >= 0.0)
        .map(|m| Duration::from_millis(m as u64))
}

fn now(now_ms: f64) -> Duration {
    Duration::from_millis(now_ms as u64)
}

fn random_sid() -> Result<String, JsValue> {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).map_err(js_err)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

fn snapshot_json(s: &SessionSnapshot) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// Effects collected under the state borrow and applied after it.
#[derive(Default)]
struct Effects {
    close: Vec<(String, u16, &'static str)>,
    /// Frames to write: (sid, bytes).
    frames: Vec<(String, Vec<u8>)>,
    snapshot: Option<String>,
    deadline_ms: Option<f64>,
}

/// The node of one object.
#[wasm_bindgen]
pub struct WasmNode {
    id: Id,
    node: Node,
    cfg: IngressConfig,
    relay_url: String,
}

#[wasm_bindgen]
impl WasmNode {
    /// Creates the node in the Cold state. After hibernation, call `restore` for every socket.
    #[wasm_bindgen(constructor)]
    pub fn new(
        secret: &[u8],
        #[wasm_bindgen(unchecked_param_type = "NodeOptions")] opts: JsValue,
        host: NodeHost,
    ) -> Result<WasmNode, JsError> {
        let arr: [u8; 32] = secret
            .try_into()
            .map_err(|_| JsError::new("secret key must be 32 bytes"))?;
        let secret = SecretKey::from_bytes(&arr);
        let id = secret.public();
        let mut alpns = Vec::new();
        let protocols = get(&opts, "protocols");
        for p in js_sys::Array::from(&protocols).iter() {
            let alpn = get(&p, "alpn")
                .as_string()
                .ok_or_else(|| JsError::new("protocols[].alpn must be a string"))?;
            alpns.push((alpn.into_bytes(), ms(get(&p, "idleMs"))));
        }
        host::install(
            id,
            Entry {
                host,
                sessions: HashMap::new(),
                peers: HashMap::new(),
                alpns,
            },
        );
        let wakes = get(&opts, "wakes").as_f64().unwrap_or(0.0) as u64;
        let bundle = get(&opts, "bundle").as_string().unwrap_or_default();
        let mut config = Config::new(secret, wakes, bundle);
        config.endpoint_builds = get(&opts, "endpointBuilds").as_f64().unwrap_or(0.0) as u64;
        if let Some(d) = ms(get(&opts, "tIdleMs")) {
            config.t_idle = d;
        }
        if let Some(d) = ms(get(&opts, "tSilentMs")) {
            config.t_silent = d;
        }
        if let Some(d) = ms(get(&opts, "tLingerMs")) {
            config.t_linger = d;
        }
        let node = Node::new(config, Arc::new(JsHost { local: id }), &APP);
        Ok(WasmNode {
            id,
            node,
            cfg: IngressConfig::new(id),
            relay_url: get(&opts, "relayUrl").as_string().unwrap_or_default(),
        })
    }

    /// This endpoint's id.
    pub fn id(&self) -> EndpointId {
        EndpointId(self.id)
    }

    /// The app's handle to the endpoint.
    pub fn endpoint(&self) -> Endpoint {
        Endpoint {
            ctx: self.node.ctx(),
            relay_url: self.relay_url.clone(),
        }
    }

    /// `Cold`, `Warm` or `Active`.
    pub fn state(&self) -> String {
        self.node.state().as_str().to_owned()
    }

    /// The status JSON (`iroh_do_core::Node::status`).
    pub fn status(&self) -> String {
        self.node.status_json()
    }

    /// Node tasks currently alive; 0 in Warm.
    pub fn tasks(&self) -> usize {
        self.node.stats().tasks
    }

    /// Runs Rust-app work scheduled through `NodeHost.schedule`.
    pub fn alarm(&self, key: &str, payload: &str) {
        self.node.alarm(key, payload);
    }

    /// Answers a relay upgrade request. On success the caller accepts a WebSocket tagged
    /// with `sid`, stores `snapshot` in its attachment, and answers 101 with
    /// `Sec-WebSocket-Protocol: <protocol>`. The challenge is written to the socket through
    /// `sendFrame` when the caller calls `opened(sid)`.
    #[wasm_bindgen(unchecked_return_type = "UpgradeResult")]
    pub fn upgrade(
        &self,
        method: &str,
        path: &str,
        protocol: Option<String>,
        now_ms: f64,
    ) -> Result<JsValue, JsValue> {
        let req = UpgradeRequest {
            method,
            path,
            sec_websocket_protocol: protocol.as_deref(),
        };
        let out = Object::new();
        let mut rng = rand_core::UnwrapErr(getrandom::SysRng);
        let (resp, session) = match IngressSession::on_upgrade(&req, &self.cfg, &mut rng, now(now_ms))
        {
            Ok(x) => x,
            Err(rej) => {
                set(&out, "ok", false);
                set(&out, "status", rej.http_status());
                set(&out, "message", rej.to_string());
                return Ok(out.into());
            }
        };
        let sid = random_sid()?;
        set(&out, "ok", true);
        set(&out, "sid", sid.as_str());
        set(&out, "protocol", resp.subprotocol_header().1);
        set(
            &out,
            "snapshot",
            session.snapshot().map(|s| snapshot_json(&s)).unwrap_or_default(),
        );
        set(
            &out,
            "deadlineMs",
            session
                .handshake_deadline()
                .map_or(0.0, |d| d.as_millis() as f64),
        );
        let challenge = resp.challenge.into_vec();
        host::with(&self.id, |e| e.sessions.insert(sid.clone(), session));
        PENDING_CHALLENGES.with(|p| p.borrow_mut().insert(sid, challenge));
        Ok(out.into())
    }

    /// Writes the challenge of a socket accepted after `upgrade`.
    pub fn opened(&self, sid: &str) -> Result<(), JsValue> {
        let challenge = PENDING_CHALLENGES.with(|p| p.borrow_mut().remove(sid));
        match challenge {
            Some(c) => host::send(&self.id, sid, &c),
            None => Err(js_err("no pending challenge")),
        }
    }

    /// Rebuilds the session of a socket that survived hibernation, from its attachment.
    #[wasm_bindgen(unchecked_return_type = "IngressResult")]
    pub fn restore(&self, sid: String, snapshot: Option<String>, now_ms: f64) -> JsValue {
        let mut fx = Effects::default();
        let snap = snapshot.and_then(|s| serde_json::from_str::<SessionSnapshot>(&s).ok());
        let Some(snap) = snap else {
            fx.close
                .push((sid, close_code::PROTOCOL_ERROR, "lost session"));
            return self.apply(fx);
        };
        let session = IngressSession::restore(snap, &self.cfg);
        if session.handshake_deadline().is_some_and(|d| d <= now(now_ms)) {
            warn!(event = "ingress_auth", endpoint = %self.id.to_z32(), ok = false, reason = "handshake timeout");
            fx.close
                .push((sid, close_code::POLICY_VIOLATION, "handshake timeout"));
            return self.apply(fx);
        }
        let peer = session.peer();
        fx.deadline_ms = session.handshake_deadline().map(|d| d.as_millis() as f64);
        host::with(&self.id, |e| {
            if let Some(peer) = peer
                && let Some(old) = e.peers.insert(peer, sid.clone())
            {
                // Two attachments for one peer: keep the later, close the other.
                e.sessions.remove(&old);
                fx.close.push((old, CLOSE_SUPERSEDED, "superseded"));
            }
            e.sessions.insert(sid, session);
        });
        if let Some(peer) = peer {
            self.node.socket_authenticated(peer);
        }
        self.apply(fx)
    }

    /// Handles one binary message from socket `sid`.
    #[wasm_bindgen(unchecked_return_type = "IngressResult")]
    pub fn message(&self, sid: String, bytes: &[u8], now_ms: f64) -> JsValue {
        let mut fx = Effects::default();
        let mut authed = None;
        let mut datagrams = Vec::new();
        let local = self.id;
        let handled = host::with(&self.id, |e| {
            let Some(session) = e.sessions.get_mut(&sid) else {
                fx.close
                    .push((sid.clone(), close_code::PROTOCOL_ERROR, "lost session"));
                return;
            };
            let violations = session.dst_violations();
            match session.on_message(bytes, now(now_ms)) {
                Ok(outputs) => {
                    for out in outputs {
                        match out {
                            Output::Send(f) => fx.frames.push((sid.clone(), f.into_vec())),
                            Output::Authenticated { peer } => authed = Some(peer),
                            Output::Datagrams { from, batch } => datagrams.push((
                                from,
                                Ecn::to_bits(batch.ecn),
                                batch.segment_size.map(|s| usize::from(s.get())),
                                batch.contents.to_vec(),
                            )),
                        }
                    }
                    // The count lives in the attachment, so hibernation cannot reset it.
                    if session.dst_violations() != violations {
                        warn!(event = "dst_violation", endpoint = %local.to_z32(), count = session.dst_violations());
                        fx.snapshot = session.snapshot().map(|s| snapshot_json(&s));
                    }
                }
                Err(close) => close_session(e, &sid, close, &mut fx, &local),
            }
            if let Some(peer) = authed {
                // A newer socket from the same peer replaces the older one.
                let older: Vec<String> = e
                    .sessions
                    .iter()
                    .filter(|(k, s)| **k != sid && s.peer() == Some(peer))
                    .map(|(k, _)| k.clone())
                    .collect();
                for k in &older {
                    if let Some(old) = e.sessions.remove(k) {
                        if let Some(f) = old.status_frame(Status::SameEndpointIdConnected) {
                            fx.frames.push((k.clone(), f.into_vec()));
                        }
                        fx.close.push((k.clone(), CLOSE_SUPERSEDED, "superseded"));
                    }
                }
                e.peers.insert(peer, sid.clone());
                fx.snapshot = e
                    .sessions
                    .get(&sid)
                    .and_then(|s| s.snapshot())
                    .map(|s| snapshot_json(&s));
                info!(event = "ingress_auth", endpoint = %local.to_z32(), ok = true, peer = %peer.to_z32(), superseded = older.len());
            }
        });
        if handled.is_none() {
            fx.close.push((sid, 1011, "refused"));
        }
        if let Some(peer) = authed {
            self.node.socket_authenticated(peer);
        }
        let out = self.apply(fx);
        for (from, ecn, seg, contents) in datagrams {
            self.node.datagrams(from, ecn, contents.into(), seg);
        }
        out
    }

    /// Handles a text message, which the relay protocol does not allow.
    #[wasm_bindgen(js_name = textMessage, unchecked_return_type = "IngressResult")]
    pub fn text_message(&self, sid: String) -> JsValue {
        let mut fx = Effects::default();
        let local = self.id;
        host::with(&self.id, |e| {
            if let Some(s) = e.sessions.get_mut(&sid) {
                let close = s.on_text_message();
                close_session(e, &sid, close, &mut fx, &local);
            }
        });
        self.apply(fx)
    }

    /// Forgets socket `sid`, which the peer or the runtime closed.
    #[wasm_bindgen(js_name = socketClosed)]
    pub fn socket_closed(&self, sid: &str) {
        let gone = host::with(&self.id, |e| drop_session(e, sid)).flatten();
        if let Some(peer) = gone {
            self.node.socket_closed(peer);
        }
    }

    /// Closes sockets still unauthenticated at their deadline. `deadlineMs` in the result is
    /// the earliest remaining deadline.
    #[wasm_bindgen(unchecked_return_type = "IngressResult")]
    pub fn sweep(&self, now_ms: f64) -> JsValue {
        let mut fx = Effects::default();
        let t = now(now_ms);
        host::with(&self.id, |e| {
            let expired: Vec<String> = e
                .sessions
                .iter()
                .filter(|(_, s)| s.handshake_deadline().is_some_and(|d| d <= t))
                .map(|(k, _)| k.clone())
                .collect();
            for sid in expired {
                e.sessions.remove(&sid);
                fx.close
                    .push((sid, close_code::POLICY_VIOLATION, "handshake timeout"));
            }
            fx.deadline_ms = e
                .sessions
                .values()
                .filter_map(|s| s.handshake_deadline())
                .min()
                .map(|d| d.as_millis() as f64);
        });
        self.apply(fx)
    }

    /// Writes collected frames, then returns the `IngressResult`.
    fn apply(&self, fx: Effects) -> JsValue {
        for (sid, f) in &fx.frames {
            let _ = host::send(&self.id, sid, f);
        }
        let out = Object::new();
        let close = js_sys::Array::new();
        for (sid, code, reason) in fx.close {
            let c = Object::new();
            set(&c, "sid", sid.as_str());
            set(&c, "code", code);
            set(&c, "reason", reason);
            close.push(&c);
        }
        set(&out, "close", close);
        if let Some(s) = fx.snapshot {
            set(&out, "snapshot", s);
        }
        if let Some(d) = fx.deadline_ms {
            set(&out, "deadlineMs", d);
        }
        out.into()
    }
}

thread_local! {
    /// Challenges of accepted upgrades, until the socket is open for writing.
    static PENDING_CHALLENGES: std::cell::RefCell<HashMap<String, Vec<u8>>> =
        std::cell::RefCell::new(HashMap::new());
}

/// Removes `sid`; returns its peer if no other socket of that peer remains.
fn drop_session(e: &mut Entry, sid: &str) -> Option<Id> {
    let peer = e.sessions.remove(sid)?.peer()?;
    if e.peers.get(&peer).map(String::as_str) == Some(sid) {
        e.peers.remove(&peer);
        return Some(peer);
    }
    None
}

fn close_session(e: &mut Entry, sid: &str, close: Close, fx: &mut Effects, local: &Id) {
    match e.sessions.get(sid).and_then(|s| s.peer()) {
        None => warn!(event = "ingress_auth", endpoint = %local.to_z32(), ok = false, code = close.code, reason = close.reason_text()),
        Some(p) => info!(event = "socket_close", endpoint = %local.to_z32(), peer = %p.to_z32(), by = "object", code = close.code, reason = close.reason_text()),
    }
    fx.close.push((sid.to_owned(), close.code, close.reason_text()));
    if let Some(f) = close.frame {
        fx.frames.push((sid.to_owned(), f.into_vec()));
    }
    // The peer's socket_closed reaches the node when the object reports the close.
}

/// The app's handle to its endpoint: `this.endpoint` in the TypeScript class.
#[wasm_bindgen]
pub struct Endpoint {
    ctx: Ctx,
    relay_url: String,
}

#[wasm_bindgen]
impl Endpoint {
    /// This endpoint's id.
    pub fn id(&self) -> EndpointId {
        EndpointId(self.ctx.id())
    }

    /// This endpoint's id and its relay URL, which the object serves itself.
    pub fn addr(&self) -> EndpointAddr {
        EndpointAddr {
            id: self.ctx.id(),
            relay_url: Some(self.relay_url.clone()),
        }
    }

    /// The peers holding a relay socket to this object: the only peers `connect` can reach.
    pub fn peers(&self) -> Vec<EndpointId> {
        self.ctx.peers().into_iter().map(EndpointId).collect()
    }

    /// Dials `peer` over its relay socket to this object (dial-back), binding the endpoint if
    /// needed. Fails at once if the peer holds no socket: the object never dials out.
    #[wasm_bindgen(unchecked_return_type = "Promise<Connection>")]
    pub fn connect(
        &self,
        #[wasm_bindgen(unchecked_param_type = "EndpointId | EndpointAddr")] peer: JsValue,
        #[wasm_bindgen(unchecked_param_type = "string | Uint8Array")] alpn: JsValue,
    ) -> Promise {
        let ctx = self.ctx.clone();
        let parsed = peer_from_js(&peer);
        let alpn = match alpn.as_string() {
            Some(s) => s.into_bytes(),
            None => js_sys::Uint8Array::new(&alpn).to_vec(),
        };
        future_to_promise(async move {
            let peer = parsed?;
            let conn = ctx.connect(peer, &alpn).await.map_err(js_err)?;
            let activity = ctx.activity(&conn);
            Ok(Connection::new(conn, activity).into())
        })
    }
}
