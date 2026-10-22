// iroh-do for TypeScript Durable Objects (spike).
//
// The class below owns the platform: hibernatable WebSockets and their attachments,
// SQLite, the alarm and env. The wasm library (crates/iroh-do-wasm) owns the relay ingress
// sessions, the iroh endpoint and its lifecycle, and the Connection/stream API, whose names
// follow @number0/iroh 1.1.0.
import { DurableObject } from "cloudflare:workers";
import {
  EndpointId,
  WasmNode,
  generateSecretKey,
  installOnce,
  publicKeyOf,
  type Connection,
  type Endpoint,
  type IngressResult,
  type NodeHost,
} from "./wasm";

export {
  BiStream,
  Connection,
  Endpoint,
  EndpointAddr,
  EndpointId,
  RecvStream,
  SendStream,
} from "./wasm";

/** Handles accepted connections for one ALPN, like iroh's `ProtocolHandler`. */
export interface ProtocolHandler {
  /** Runs for each accepted connection; the connection is counted until it closes. */
  accept(conn: Connection): Promise<void>;
  /** Idle limit for this ALPN instead of the default 10 s. */
  idleMs?: number;
}

/** The app's protocol handlers, by ALPN. */
export type Protocols = Record<string, ProtocolHandler>;

/** Node timings; tests shorten them. Defaults are iroh-do-core's. */
export interface Timings {
  tIdleMs?: number;
  tSilentMs?: number;
  tLingerMs?: number;
}

/** Status of an endpoint, from its node. */
export type Status = Record<string, unknown>;

const SCHEMA = [
  `CREATE TABLE IF NOT EXISTS iroh_identity (
     k INTEGER PRIMARY KEY CHECK (k = 1),
     secret BLOB NOT NULL, id TEXT NOT NULL, relay_url TEXT NOT NULL, minted_at INTEGER NOT NULL)`,
  `CREATE TABLE IF NOT EXISTS iroh_counters (name TEXT PRIMARY KEY, value INTEGER NOT NULL)`,
  `CREATE TABLE IF NOT EXISTS iroh_schedule (key TEXT PRIMARY KEY, due_ms REAL NOT NULL, payload TEXT NOT NULL)`,
];

const SWEEP = "sweep";

interface Attachment {
  v: 1;
  sid: string;
  /** Ingress session snapshot, JSON from the wasm side. */
  s: string;
}

interface Identity {
  secret: Uint8Array;
  id: string;
  relayUrl: string;
}

/** A close code the object may send when completing a peer's close (RFC 6455 §7.4). */
function sendableCloseCode(code: number): number {
  return (code >= 1000 && code <= 1003) || (code >= 1007 && code <= 1014) || (code >= 3000 && code <= 4999)
    ? code
    : 1000;
}

/**
 * A Durable Object that is one iroh endpoint, named by its EndpointId (z-base-32).
 *
 * Subclasses return their protocol handlers from `protocols()`. Peers reach the object
 * through `irohRouter`, which forwards `wss://<id>.<zone>/relay` here.
 */
export abstract class IrohDurableObject<Env = unknown> extends DurableObject<Env> {
  /** The binding of this class's namespace, used to check that the key matches the name. */
  static namespace = "ENDPOINTS";

  /** The app's protocol handlers. Called once per object construction. */
  abstract protocols(): Protocols;

  /** Node timings; override to change them. */
  protected timings(): Timings {
    return {};
  }

  #node: WasmNode | null = null;
  #endpoint: Endpoint | null = null;
  #handlers: Protocols = {};
  #sockets = new Map<string, WebSocket>();
  #loaded = false;

  /** This endpoint: `id()`, `addr()`, `connect(peer, alpn)`. Throws before mint. */
  get endpoint(): Endpoint {
    this.#load();
    if (!this.#endpoint) throw new Error("endpoint not minted");
    return this.#endpoint;
  }

  #sql(query: string, ...params: unknown[]) {
    return this.ctx.storage.sql.exec(query, ...params);
  }

  #minted(): boolean {
    // Unminted objects write nothing, so the identity table exists only after mint.
    return this.#sql("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'iroh_identity'").toArray().length > 0;
  }

  #identity(): Identity | null {
    if (!this.#minted()) return null;
    const row = this.#sql("SELECT secret, id, relay_url FROM iroh_identity WHERE k = 1").toArray()[0];
    if (!row) return null;
    return { secret: new Uint8Array(row.secret as ArrayBuffer), id: row.id as string, relayUrl: row.relay_url as string };
  }

  #bump(name: string): number {
    const row = this.#sql(
      "INSERT INTO iroh_counters (name, value) VALUES (?, 1) ON CONFLICT(name) DO UPDATE SET value = value + 1 RETURNING value",
      name,
    ).one();
    return row.value as number;
  }

  #counter(name: string): number {
    const row = this.#sql("SELECT value FROM iroh_counters WHERE name = ?", name).toArray()[0];
    return (row?.value as number | undefined) ?? 0;
  }

  /** Whether `id` (z-base-32) names this object. */
  #isMe(id: string): boolean {
    const binding = (this.constructor as typeof IrohDurableObject).namespace;
    const ns = (this.env as Record<string, DurableObjectNamespace>)[binding];
    return ns?.idFromName(id).equals(this.ctx.id) ?? false;
  }

  /**
   * Builds the node on the first event of this construction: loads the identity, creates
   * the wasm node, and rebuilds the sessions of sockets that survived hibernation.
   *
   * Not done in the constructor, because `protocols()` belongs to the subclass, whose
   * fields are not initialized while the base constructor runs.
   */
  #load(): void {
    if (this.#loaded) return;
    this.#loaded = true;
    installOnce((this.env as Record<string, string | undefined>).LOG_LEVEL);
    const ident = this.#identity();
    if (!ident || !this.#isMe(ident.id)) {
      if (ident) console.error({ event: "identity_invariant_violated", object: this.ctx.id.toString() });
      for (const ws of this.ctx.getWebSockets()) ws.close(1011, "no identity");
      return;
    }
    const wakes = this.#bump("wakes");
    this.#handlers = this.protocols();
    const t = this.timings();
    const host: NodeHost = {
      sendFrame: (sid, frame) => {
        const ws = this.#sockets.get(sid);
        if (!ws) throw new Error("no socket");
        ws.send(frame);
      },
      accept: (alpn, conn) => {
        const h = this.#handlers[alpn];
        return h ? h.accept(conn) : Promise.resolve();
      },
      schedule: (key, delayMs, payload) => this.#schedule(key, Date.now() + delayMs, payload),
      cancel: (key) => void this.#sql("DELETE FROM iroh_schedule WHERE key = ?", key),
      endpointBuilt: () => void this.#bump("endpoint_builds"),
    };
    this.#node = new WasmNode(ident.secret, {
      wakes,
      endpointBuilds: this.#counter("endpoint_builds"),
      bundle: String((this.env as Record<string, unknown>).BUNDLE_SHA256 ?? ""),
      relayUrl: ident.relayUrl,
      protocols: Object.entries(this.#handlers).map(([alpn, h]) => ({ alpn, idleMs: h.idleMs })),
      ...t,
    }, host);
    this.#endpoint = this.#node.endpoint();
    let restored = 0;
    const sockets = this.ctx.getWebSockets();
    for (const ws of sockets) {
      const att = ws.deserializeAttachment() as Attachment | null;
      const sid = att?.sid ?? this.#sidOf(ws) ?? "";
      this.#sockets.set(sid, ws);
    }
    for (const ws of sockets) {
      const att = ws.deserializeAttachment() as Attachment | null;
      const sid = att?.sid ?? this.#sidOf(ws) ?? "";
      this.#apply(this.#node.restore(sid, att?.v === 1 ? att.s : undefined, Date.now()), ws, sid);
      restored++;
    }
    console.log({ event: "wake", endpoint: ident.id, wakes, sockets: restored, state: this.#node.state() });
  }

  #sidOf(ws: WebSocket): string | undefined {
    return this.ctx.getTags(ws).find((t) => t.startsWith("sid:"))?.slice(4);
  }

  /** Applies the wasm side's verdict: closes sockets, stores the snapshot, arms the sweep. */
  #apply(res: IngressResult, ws?: WebSocket, sid?: string): void {
    for (const c of res.close) {
      const s = this.#sockets.get(c.sid);
      this.#sockets.delete(c.sid);
      try {
        s?.close(c.code, c.reason);
      } catch {}
      this.#node?.socketClosed(c.sid);
    }
    if (res.snapshot && ws && sid && this.#sockets.has(sid)) {
      ws.serializeAttachment({ v: 1, sid, s: res.snapshot } satisfies Attachment);
    }
    if (res.deadlineMs) this.#scheduleEarliest(SWEEP, res.deadlineMs);
  }

  // Deferred work is a row served by the single alarm: a timer would keep the object
  // from hibernating.
  #schedule(key: string, dueMs: number, payload = ""): void {
    this.#sql(
      "INSERT INTO iroh_schedule (key, due_ms, payload) VALUES (?, ?, ?) ON CONFLICT(key) DO UPDATE SET due_ms = excluded.due_ms, payload = excluded.payload",
      key, dueMs, payload,
    );
    this.#rearm();
  }

  #scheduleEarliest(key: string, dueMs: number): void {
    this.#sql(
      "INSERT INTO iroh_schedule (key, due_ms, payload) VALUES (?, ?, '') ON CONFLICT(key) DO UPDATE SET due_ms = min(due_ms, excluded.due_ms)",
      key, dueMs,
    );
    this.#rearm();
  }

  #rearm(): void {
    const next = this.#sql("SELECT min(due_ms) AS due FROM iroh_schedule").one().due as number | null;
    if (next !== null) void this.ctx.storage.setAlarm(Math.max(next, Date.now()));
  }

  override async fetch(req: Request): Promise<Response> {
    this.#load();
    const node = this.#node;
    if (!node) return new Response("not found", { status: 404 });
    const url = new URL(req.url);
    const r = node.upgrade(req.method, url.pathname, req.headers.get("sec-websocket-protocol") ?? undefined, Date.now());
    if (!r.ok) return new Response(r.message, { status: r.status });
    const [client, server] = Object.values(new WebSocketPair());
    this.ctx.acceptWebSocket(server, ["relay", `sid:${r.sid}`]);
    server.serializeAttachment({ v: 1, sid: r.sid, s: r.snapshot } satisfies Attachment);
    this.#sockets.set(r.sid, server);
    node.opened(r.sid);
    // The handshake deadline is enforced by the alarm, not a timer.
    this.#scheduleEarliest(SWEEP, r.deadlineMs);
    return new Response(null, {
      status: 101,
      webSocket: client,
      headers: { "Sec-WebSocket-Protocol": r.protocol },
    });
  }

  override async webSocketMessage(ws: WebSocket, msg: string | ArrayBuffer): Promise<void> {
    this.#load();
    const sid = this.#sidOf(ws);
    if (!this.#node || !sid) {
      ws.close(1011, "refused");
      return;
    }
    const res = typeof msg === "string"
      ? this.#node.textMessage(sid)
      : this.#node.message(sid, new Uint8Array(msg), Date.now());
    this.#apply(res, ws, sid);
  }

  override async webSocketClose(ws: WebSocket, code: number, reason: string, wasClean: boolean): Promise<void> {
    this.#load();
    const sid = this.#sidOf(ws);
    if (sid) {
      console.log({ event: "socket_close", endpoint: this.#endpoint?.id().toZ32(), by: "peer", code, reason, clean: wasClean });
      this.#sockets.delete(sid);
      this.#node?.socketClosed(sid);
    }
    try {
      ws.close(sendableCloseCode(code), "bye");
    } catch {}
  }

  override async webSocketError(ws: WebSocket, error: unknown): Promise<void> {
    this.#load();
    const sid = this.#sidOf(ws);
    if (sid) {
      console.warn({ event: "socket_close", endpoint: this.#endpoint?.id().toZ32(), by: "error", error: String(error) });
      this.#sockets.delete(sid);
      this.#node?.socketClosed(sid);
    }
  }

  override async alarm(): Promise<void> {
    this.#load();
    const node = this.#node;
    if (!node) return;
    const now = Date.now();
    const due = this.#sql("SELECT key, payload FROM iroh_schedule WHERE due_ms <= ? ORDER BY due_ms", now).toArray();
    for (const d of due) {
      const key = d.key as string;
      this.#sql("DELETE FROM iroh_schedule WHERE key = ?", key);
      if (key === SWEEP) this.#apply(node.sweep(now));
      else node.alarm(key, d.payload as string);
    }
    console.log({ event: "alarm", endpoint: this.#endpoint?.id().toZ32(), ran: due.length });
    this.#rearm();
  }

  /**
   * RPC from the router: stores `secret` as this endpoint's key if the object has no
   * identity yet and the key's id names this object.
   */
  async mint(secret: Uint8Array, relayUrl: string): Promise<{ id: string; relay_url: string }> {
    const id = publicKeyOf(secret).toZ32();
    if (!this.#isMe(id)) throw new Error("name mismatch");
    if (this.#minted() && this.#identity()) throw new Error("identity exists");
    for (const s of SCHEMA) this.#sql(s);
    this.#sql("INSERT INTO iroh_identity (k, secret, id, relay_url, minted_at) VALUES (1, ?, ?, ?, ?)", secret, id, relayUrl, Date.now());
    this.#loaded = false;
    this.#load();
    console.log({ event: "mint", endpoint: id });
    return { id, relay_url: relayUrl };
  }

  /** RPC from the router: the node's status, with the wasm side's live task count. */
  async status(): Promise<Status | null> {
    this.#load();
    if (!this.#node) return null;
    return { ...JSON.parse(this.#node.status()), tasks: this.#node.tasks(), sockets: this.#sockets.size };
  }
}

const Z32 = /^[ybndrfg8ejkmcpqxot1uwisza345h769]{52}$/;

function parseEndpointLabel(label: string): EndpointId | null {
  if (!Z32.test(label)) return null;
  try {
    // PublicKey::from_z32 in Rust, never FromStr (design note 004).
    return EndpointId.fromZ32(label);
  } catch {
    return null;
  }
}

function labelFor(host: string, zone: string): string | null {
  const h = host.replace(/:\d+$/, "").replace(/\.$/, "").toLowerCase();
  const dot = h.indexOf(".");
  if (dot <= 0 || h.slice(dot + 1) !== zone.toLowerCase()) return null;
  return h.slice(0, dot);
}

function tokenOk(auth: string | null, expected: string | undefined): boolean {
  if (!expected) return true; // spike: no token configured, minting is open (local only)
  const m = /^bearer\s+(.+)$/i.exec(auth?.trim() ?? "");
  if (!m) return false;
  const a = new TextEncoder().encode(m[1].trim());
  const b = new TextEncoder().encode(expected);
  return a.length === b.length && crypto.subtle.timingSafeEqual(a, b);
}

/** Options of `irohRouter`. */
export interface RouterOptions {
  /** The Durable Object namespace binding of the endpoint class. */
  binding: string;
}

type RouterEnv = Record<string, unknown> & { ZONE_SUFFIX?: string; ADMIN_TOKEN?: string };

/**
 * The Worker's `fetch`: routes `<z32>.<zone>/relay` to the object named by the label, and
 * serves `admin.<zone>`: `POST /mint` and `GET /status/<z32>`, both over Workers RPC.
 */
export function irohRouter(opts: RouterOptions) {
  return async (req: Request, env: RouterEnv): Promise<Response> => {
    installOnce(env.LOG_LEVEL as string | undefined);
    const zone = env.ZONE_SUFFIX;
    if (!zone) return new Response("misconfigured", { status: 500 });
    const ns = env[opts.binding] as DurableObjectNamespace<IrohDurableObject>;
    const url = new URL(req.url);
    const label = labelFor(url.host, zone);
    if (label === null || url.pathname.startsWith("/_internal/")) return new Response("not found", { status: 404 });

    if (label === "admin") {
      if (req.method === "POST" && url.pathname === "/mint") {
        if (!tokenOk(req.headers.get("authorization"), env.ADMIN_TOKEN)) return new Response("unauthorized", { status: 401 });
        const secret = new Uint8Array(generateSecretKey());
        const id = publicKeyOf(secret).toZ32();
        const relayUrl = `${url.protocol}//${id}.${zone}${url.port ? `:${url.port}` : ""}/`;
        const stub = ns.get(ns.idFromName(id));
        const minted = await stub.mint(secret, relayUrl);
        return Response.json(minted);
      }
      const m = /^\/status\/([^/]+)$/.exec(url.pathname);
      if (req.method === "GET" && m && parseEndpointLabel(m[1])) {
        const status = await ns.get(ns.idFromName(m[1])).status();
        return status ? Response.json(status) : new Response("not found", { status: 404 });
      }
      return new Response("not found", { status: 404 });
    }

    if (!parseEndpointLabel(label)) return new Response("not found", { status: 404 });
    switch (url.pathname) {
      case "/relay": {
        if (req.method !== "GET") return new Response("method not allowed", { status: 405 });
        const upgrade = req.headers.get("upgrade") ?? "";
        if (!upgrade.split(",").some((t) => t.trim().toLowerCase() === "websocket")) {
          return new Response("upgrade required", { status: 426 });
        }
        return ns.get(ns.idFromName(label)).fetch(req);
      }
      // Probes iroh's net_report sends to a home relay, answered without waking an object.
      case "/ping":
        return new Response(null, { status: 200, headers: { "Access-Control-Allow-Origin": "*" } });
      case "/generate_204": {
        const c = req.headers.get("x-iroh-challenge") ?? "";
        const headers: Record<string, string> = {};
        if (c && c.length < 64 && /^[A-Za-z0-9._-]+$/.test(c)) headers["X-Iroh-Response"] = `response ${c}`;
        return new Response(null, { status: 204, headers });
      }
      default:
        return new Response("not found", { status: 404 });
    }
  };
}
