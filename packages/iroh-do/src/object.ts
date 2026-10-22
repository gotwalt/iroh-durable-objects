// The Durable Object base class. It owns the platform: hibernatable WebSockets and their
// attachments, SQLite, the alarm and env. The wasm library owns the relay ingress sessions,
// the iroh endpoint and its lifecycle, and the Connection and stream API.
import { DurableObject } from "cloudflare:workers";
import { log } from "./log";
import {
  WasmNode,
  instance,
  noteTrap,
  pkarrConfirm,
  pkarrRecord,
  publicKeyOf,
  testPanicExport,
  type Connection,
  type Endpoint,
  type IngressResult,
  type NodeHost,
} from "./wasm";

/** Handles accepted connections for one ALPN, like iroh's `ProtocolHandler`. */
export interface ProtocolHandler {
  /**
   * Runs for each accepted connection. When the returned promise settles, the node closes
   * the connection with code 0 and reason `done`, as iroh's `Router` drops it when `accept`
   * returns. A handler that keeps using the connection keeps the promise pending, for
   * example until `conn.closed()`.
   */
  accept(conn: Connection): Promise<void>;
  /** Idle limit for this ALPN instead of the default 10 s. Also applies to dials with it. */
  idleMs?: number;
}

/** The app's protocol handlers, by ALPN. */
export type Protocols = Record<string, ProtocolHandler>;

/** Node timings. Defaults are iroh-do-core's: t_idle 10 s, t_silent 60 s, t_linger 2 s. */
export interface Timings {
  tIdleMs?: number;
  tSilentMs?: number;
  tLingerMs?: number;
}

/** Status of an endpoint: the node's status JSON and the host's fields. */
export type Status = Record<string, unknown>;

/** What `mint` returns. */
export interface MintResult {
  id: string;
  relay_url: string;
  /** Whether the pkarr record was published and confirmed by GET. */
  published: boolean;
}

/** What `destroy` returns for a minted endpoint. */
export interface DestroyResult {
  id: string;
  destroyed: true;
}

// The library's tables. An object writes nothing until it is minted, so probing an unminted
// name creates no storage.
const SCHEMA = [
  `CREATE TABLE IF NOT EXISTS iroh_identity (
     k INTEGER PRIMARY KEY CHECK (k = 1),
     secret BLOB NOT NULL, id TEXT NOT NULL, relay_url TEXT NOT NULL,
     pkarr_relay TEXT NOT NULL, minted_at INTEGER NOT NULL)`,
  // wakes, endpoint_builds, node_rebuilds, bound_at_ms, pkarr_failures, pkarr_status,
  // pkarr_ok_ms.
  `CREATE TABLE IF NOT EXISTS iroh_state (name TEXT PRIMARY KEY, value)`,
  // Alarm work: `sweep`, `republish`, and `app:` keys from the node. One alarm is armed for
  // the earliest row; a timer would keep the object from hibernating.
  `CREATE TABLE IF NOT EXISTS iroh_schedule (key TEXT PRIMARY KEY, due_ms REAL NOT NULL, payload TEXT NOT NULL)`,
];

const SWEEP = "sweep";
const REPUBLISH = "republish";
/** Close code and reason for the sockets of a destroyed endpoint. */
const CLOSE_DESTROYED = 1000;

// pkarr timings (crates/iroh-do-core/src/pkarr.rs, design note 004).
const REPUBLISH_INTERVAL_MS = 24 * 60 * 60 * 1000;
const RETRY_BASE_MS = 60 * 1000;
const RETRY_JITTER_MS = 30 * 1000;

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
  pkarrRelay: string;
}

/** A close code the object may send when completing a peer's close (RFC 6455 §7.4). */
function sendableCloseCode(code: number): number {
  return (code >= 1000 && code <= 1003) || (code >= 1007 && code <= 1014) || (code >= 3000 && code <= 4999)
    ? code
    : 1000;
}

type Vars = Record<string, unknown>;

/**
 * A Durable Object that is one iroh endpoint, named by its EndpointId in z-base-32.
 *
 * Subclasses return their protocol handlers from `protocols()`. Peers reach the object
 * through `irohRouter`, which forwards `wss://<id>.<zone>/relay` here. A subclass that
 * overrides an event method (`fetch`, `webSocketMessage`, `webSocketClose`,
 * `webSocketError`, `alarm`) must call the base method.
 */
export abstract class IrohDurableObject<Env = unknown> extends DurableObject<Env> {
  /** The binding of this class's namespace in `env`, to check that a key matches the name. */
  static namespace = "ENDPOINTS";

  /** The app's protocol handlers. Called once per node build. */
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
  /** The wasm generation the node was built for (wasm.ts). */
  #generation = -1;
  /** Node builds since construction caused by a wasm reset. */
  #rebuilds = 0;
  /** Set by `#lose`: the next `#load` is a rebuild, not a wake. */
  #rebuilding = false;

  /** This endpoint: `id()`, `addr()`, `connect(peer, alpn)`. Throws before mint. */
  get endpoint(): Endpoint {
    // Handlers run inside wasm calls, where `#enter` must not replace the instance; by then
    // the node is built.
    if (!this.#loaded) this.#enter();
    if (!this.#endpoint) throw new Error("endpoint not minted");
    return this.#endpoint;
  }

  get #vars(): Vars {
    return this.env as Vars;
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
    const row = this.#sql("SELECT secret, id, relay_url, pkarr_relay FROM iroh_identity WHERE k = 1").toArray()[0];
    if (!row) return null;
    return {
      secret: new Uint8Array(row.secret as ArrayBuffer),
      id: row.id as string,
      relayUrl: row.relay_url as string,
      pkarrRelay: row.pkarr_relay as string,
    };
  }

  #get(name: string): unknown {
    return this.#sql("SELECT value FROM iroh_state WHERE name = ?", name).toArray()[0]?.value ?? null;
  }

  #put(name: string, value: unknown): void {
    if (value === null) this.#sql("DELETE FROM iroh_state WHERE name = ?", name);
    else this.#sql("INSERT INTO iroh_state (name, value) VALUES (?, ?) ON CONFLICT(name) DO UPDATE SET value = excluded.value", name, value);
  }

  #bump(name: string): number {
    const row = this.#sql(
      "INSERT INTO iroh_state (name, value) VALUES (?, 1) ON CONFLICT(name) DO UPDATE SET value = value + 1 RETURNING value",
      name,
    ).one();
    return row.value as number;
  }

  /** Whether `id` (z-base-32) names this object. */
  #isMe(id: string): boolean {
    const binding = (this.constructor as typeof IrohDurableObject).namespace;
    const ns = this.#vars[binding] as DurableObjectNamespace | undefined;
    return ns?.idFromName(id).equals(this.ctx.id) ?? false;
  }

  /**
   * Starts every event: replaces a trapped wasm instance, rebuilds the node if it belongs to
   * an older instance, and builds it on the first event of this construction.
   */
  #enter(): WasmNode | null {
    const generation = instance(this.#vars.LOG_LEVEL as string | undefined);
    if (this.#loaded && this.#node && this.#generation !== generation) this.#lose(generation);
    if (!this.#loaded) this.#load(generation);
    return this.#node;
  }

  /**
   * Runs an event body. On a wasm trap the instance is replaced and this object's node
   * rebuilt at once; the event that trapped is dropped and `onTrap()` returned.
   */
  async #run<T>(onTrap: () => T, body: (node: WasmNode | null) => T | Promise<T>): Promise<T> {
    try {
      return await body(this.#enter());
    } catch (e) {
      if (!noteTrap(e)) throw e;
      this.#enter();
      return onTrap();
    }
  }

  /** Drops a node built for an older wasm instance; the caller then builds a new one. */
  #lose(generation: number): void {
    const boundAt = this.#get("bound_at_ms") as number | null;
    this.#put("bound_at_ms", null);
    this.#rebuilds++;
    const rebuilds = this.#bump("node_rebuilds");
    log("warn", "node_rebuild", {
      endpoint: this.#identity()?.id,
      generation,
      rebuilds,
      // The endpoint was bound: its QUIC connections are gone, and peers redial.
      was_active: boundAt !== null,
    });
    this.#node = null;
    this.#endpoint = null;
    this.#sockets.clear();
    this.#loaded = false;
    this.#rebuilding = true;
  }

  /**
   * Builds the node: loads the identity, creates the wasm node, and rebuilds the session of
   * every socket from its attachment. Runs on the first event after construction (a wake)
   * and after a wasm reset.
   *
   * Not done in the constructor: `protocols()` belongs to the subclass, whose fields are not
   * initialized while the base constructor runs.
   */
  #load(generation: number): void {
    this.#loaded = true;
    const rebuild = this.#rebuilding;
    this.#rebuilding = false;
    const ident = this.#identity();
    if (!ident || !this.#isMe(ident.id)) {
      if (ident) log("error", "identity_invariant_violated", { object: this.ctx.id.toString() });
      for (const ws of this.ctx.getWebSockets()) ws.close(1011, "no identity");
      return;
    }
    this.#generation = generation;
    const wakes = rebuild ? Number(this.#get("wakes") ?? 0) : this.#bump("wakes");
    if (!rebuild) {
      // Set while an endpoint is bound, cleared when it closes (iroh_do_core::Host). Found
      // set by a new construction, it means the previous instance was evicted, not
      // hibernated, with QUIC connections open.
      const boundAt = this.#get("bound_at_ms") as number | null;
      if (boundAt !== null) {
        log("warn", "evicted_active", { endpoint: ident.id, since_bind_ms: Date.now() - boundAt, wakes });
        this.#put("bound_at_ms", null);
      }
    }
    this.#handlers = this.protocols();
    const host: NodeHost = {
      sendFrame: (sid, frame) => {
        const ws = this.#sockets.get(sid);
        if (!ws) throw new Error("no socket");
        ws.send(frame);
      },
      accept: (alpn, conn) => {
        const h = this.#handlers[alpn];
        // A promise even if the handler throws synchronously: its settling closes the
        // connection (ProtocolHandler.accept).
        return h ? Promise.resolve().then(() => h.accept(conn)) : Promise.resolve();
      },
      schedule: (key, delayMs, payload) => this.#schedule(key, Date.now() + delayMs, payload),
      cancel: (key) => void this.#sql("DELETE FROM iroh_schedule WHERE key = ?", key),
      endpointBuilt: () => {
        this.#bump("endpoint_builds");
        this.#put("bound_at_ms", Date.now());
      },
      endpointClosed: () => this.#put("bound_at_ms", null),
    };
    this.#node = new WasmNode(ident.secret, {
      wakes,
      endpointBuilds: Number(this.#get("endpoint_builds") ?? 0),
      bundle: String(this.#vars.BUNDLE_SHA256 ?? ""),
      relayUrl: ident.relayUrl,
      protocols: Object.entries(this.#handlers).map(([alpn, h]) => ({ alpn, idleMs: h.idleMs })),
      ...this.timings(),
    }, host);
    this.#endpoint = this.#node.endpoint();
    // Republishing must never stop: an alarm run cut short by a trap may have lost the row.
    if (ident.pkarrRelay && this.#sql("SELECT 1 FROM iroh_schedule WHERE key = ?", REPUBLISH).toArray().length === 0) {
      this.#schedule(REPUBLISH, Date.now() + RETRY_BASE_MS);
    }
    const sockets = this.ctx.getWebSockets();
    const sids = sockets.map((ws) => (ws.deserializeAttachment() as Attachment | null)?.sid ?? this.#sidOf(ws) ?? "");
    sockets.forEach((ws, i) => this.#sockets.set(sids[i], ws));
    let closed = 0;
    sockets.forEach((ws, i) => {
      const att = ws.deserializeAttachment() as Attachment | null;
      const res = this.#node!.restore(sids[i], att?.v === 1 ? att.s : undefined, Date.now());
      closed += res.close.length;
      this.#apply(res, ws, sids[i]);
    });
    log("info", "wake", {
      endpoint: ident.id,
      wakes,
      cause: rebuild ? "wasm_reset" : "construct",
      sockets: sockets.length - closed,
      sockets_closed: closed,
      state: this.#node.state(),
    });
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
      } catch {
        // Already closed by the peer.
      }
      this.#node?.socketClosed(c.sid);
    }
    if (res.snapshot && ws && sid && this.#sockets.has(sid)) {
      ws.serializeAttachment({ v: 1, sid, s: res.snapshot } satisfies Attachment);
    }
    if (res.deadlineMs) this.#scheduleEarliest(SWEEP, res.deadlineMs);
  }

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
    return this.#run(() => new Response("unavailable", { status: 503 }), (node) => {
      if (!node) return new Response("not found", { status: 404 });
      const url = new URL(req.url);
      const r = node.upgrade(req.method, url.pathname, req.headers.get("sec-websocket-protocol") ?? undefined, Date.now());
      if (!r.ok) return new Response(r.message, { status: r.status });
      const [client, server] = Object.values(new WebSocketPair());
      this.ctx.acceptWebSocket(server, ["relay", `sid:${r.sid}`]);
      server.serializeAttachment({ v: 1, sid: r.sid, s: r.snapshot } satisfies Attachment);
      this.#sockets.set(r.sid, server);
      node.opened(r.sid);
      // The alarm enforces the handshake deadline; a timer would block hibernation.
      this.#scheduleEarliest(SWEEP, r.deadlineMs);
      return new Response(null, {
        status: 101,
        webSocket: client,
        headers: { "Sec-WebSocket-Protocol": r.protocol },
      });
    });
  }

  override async webSocketMessage(ws: WebSocket, msg: string | ArrayBuffer): Promise<void> {
    await this.#run(() => undefined, (node) => {
      const sid = this.#sidOf(ws);
      if (!node || !sid) {
        ws.close(1011, "refused");
        return;
      }
      const res = typeof msg === "string"
        ? node.textMessage(sid)
        : node.message(sid, new Uint8Array(msg), Date.now());
      this.#apply(res, ws, sid);
    });
  }

  override async webSocketClose(ws: WebSocket, code: number, reason: string, wasClean: boolean): Promise<void> {
    await this.#run(() => undefined, (node) => {
      const sid = this.#sidOf(ws);
      if (sid && this.#sockets.has(sid)) {
        log("info", "socket_close", { endpoint: this.#identity()?.id, by: "peer", code, reason, clean: wasClean });
        this.#sockets.delete(sid);
        node?.socketClosed(sid);
      }
    });
    try {
      ws.close(sendableCloseCode(code), "bye");
    } catch {
      // Already closed.
    }
  }

  override async webSocketError(ws: WebSocket, error: unknown): Promise<void> {
    await this.#run(() => undefined, (node) => {
      const sid = this.#sidOf(ws);
      if (sid && this.#sockets.has(sid)) {
        log("warn", "socket_close", { endpoint: this.#identity()?.id, by: "error", error: String(error) });
        this.#sockets.delete(sid);
        node?.socketClosed(sid);
      }
    });
  }

  override async alarm(): Promise<void> {
    await this.#run(() => undefined, async (node) => {
      if (!node) return;
      const t0 = Date.now();
      const due = this.#sql("SELECT key, payload FROM iroh_schedule WHERE due_ms <= ? ORDER BY due_ms", t0).toArray();
      for (const d of due) {
        const key = d.key as string;
        this.#sql("DELETE FROM iroh_schedule WHERE key = ?", key);
        if (key === SWEEP) this.#apply(node.sweep(Date.now()));
        else if (key === REPUBLISH) await this.#publish();
        else node.alarm(key, d.payload as string);
      }
      // Kinds, not keys: an app key may carry a peer's EndpointId.
      const kinds = due.map((d) => ((d.key as string).startsWith("app:") ? "app" : d.key)).join(",");
      log("info", "alarm", { endpoint: this.#identity()?.id, due: kinds, ran: due.length, ms: Date.now() - t0 });
      this.#rearm();
    });
  }

  /**
   * Publishes the pkarr record and confirms it with a GET, records the outcome, and
   * schedules the next attempt: after a day, or after a linear backoff with jitter.
   */
  async #publish(): Promise<boolean> {
    const ident = this.#identity();
    if (!ident || !ident.pkarrRelay) return false;
    const t0 = Date.now();
    let ok = false;
    let status = "ok";
    try {
      const rec = pkarrRecord(ident.secret, ident.relayUrl, ident.pkarrRelay);
      const put = await fetch(rec.url, { method: "PUT", body: rec.body });
      if (!put.ok) {
        status = `put status ${put.status}`;
      } else {
        const get = await fetch(rec.url, { headers: { accept: "application/x-pkarr-signed-packet" } });
        if (get.status !== 200) {
          status = `get status ${get.status}`;
        } else {
          const body = new Uint8Array(await get.arrayBuffer());
          // A stale PUT also answers 204; only the record read back confirms (note 004).
          ok = pkarrConfirm(ident.secret, body, rec.timestampUs, ident.relayUrl);
          if (!ok) status = "get returned a different record";
        }
      }
    } catch (e) {
      if (noteTrap(e)) throw e;
      status = `fetch: ${String(e)}`;
    }
    const t = Date.now();
    const failures = ok ? 0 : Number(this.#get("pkarr_failures") ?? 0) + 1;
    this.#put("pkarr_failures", failures);
    this.#put("pkarr_status", status);
    if (ok) this.#put("pkarr_ok_ms", t);
    const next = ok
      ? t + REPUBLISH_INTERVAL_MS
      : t + Math.min(RETRY_BASE_MS * failures, REPUBLISH_INTERVAL_MS) + Math.floor(Math.random() * RETRY_JITTER_MS);
    this.#schedule(REPUBLISH, next);
    log(ok ? "info" : "warn", "pkarr_publish", {
      endpoint: ident.id,
      ok,
      ms: t - t0,
      ...(ok ? {} : { status, failures }),
      next_in_s: Math.round((next - t) / 1000),
    });
    return ok;
  }

  /**
   * RPC from the router: stores `secret` as this endpoint's key, if the object has no
   * identity yet and the key's id names this object, then publishes the pkarr record.
   * `pkarrRelay` "" disables publishing.
   */
  async mint(secret: Uint8Array, relayUrl: string, pkarrRelay: string): Promise<MintResult> {
    const trapped = (): MintResult => {
      throw new Error("wasm instance trapped during mint");
    };
    return this.#run(trapped, async () => {
      const t0 = Date.now();
      const id = publicKeyOf(secret).toZ32();
      if (!this.#isMe(id)) {
        log("error", "mint_name_mismatch", { object: this.ctx.id.toString() });
        throw new Error("name mismatch");
      }
      if (this.#identity()) throw new Error("identity exists");
      for (const s of SCHEMA) this.#sql(s);
      this.#sql(
        "INSERT INTO iroh_identity (k, secret, id, relay_url, pkarr_relay, minted_at) VALUES (1, ?, ?, ?, ?, ?)",
        secret, id, relayUrl, pkarrRelay, Date.now(),
      );
      // The first event found no identity and built no node.
      this.#loaded = false;
      this.#enter();
      const published = await this.#publish();
      log("info", "mint", { endpoint: id, published, ms: Date.now() - t0 });
      return { id, relay_url: relayUrl, published };
    });
  }

  /**
   * RPC from the router: closes every socket, drops the node, and deletes the alarm and all
   * storage, identity included. Afterwards the object is indistinguishable from one that was
   * never minted. Returns null if there is no identity to destroy.
   */
  async destroy(): Promise<DestroyResult | null> {
    return this.#run(() => null, async (node) => {
      // An object that refuses traffic (identity invariant violated) holds an identity too.
      const ident = this.#identity();
      if (!ident) return null;
      const t0 = Date.now();
      const sockets = this.ctx.getWebSockets();
      for (const ws of sockets) {
        try {
          ws.close(CLOSE_DESTROYED, "destroyed");
        } catch {
          // Already closed.
        }
      }
      // Node tasks still winding down then reach no socket and no storage.
      node?.detach();
      this.#node = null;
      this.#endpoint = null;
      this.#sockets.clear();
      this.#loaded = true;
      await this.ctx.storage.deleteAlarm();
      await this.ctx.storage.deleteAll();
      log("info", "destroy", { endpoint: ident.id, sockets: sockets.length, ms: Date.now() - t0 });
      return { id: ident.id, destroyed: true };
    });
  }

  /** RPC from the router: the node's status and the host's fields, or null before mint. */
  async status(): Promise<Status | null> {
    return this.#run(() => null, (node) => {
      if (!node) return null;
      return {
        ...JSON.parse(node.status()),
        tasks: node.tasks(),
        open_streams: node.openStreams(),
        sockets: [...this.#sockets.keys()],
        generation: this.#generation,
        rebuilds: this.#rebuilds,
        pkarr: {
          failures: this.#get("pkarr_failures"),
          status: this.#get("pkarr_status"),
          ok_ms: this.#get("pkarr_ok_ms"),
          next_ms: this.#sql("SELECT due_ms FROM iroh_schedule WHERE key = ?", REPUBLISH).toArray()[0]?.due_ms ?? null,
        },
      };
    });
  }

  /**
   * Test only: panics in the wasm instance, directly or (`inTask`) in a task on a later
   * microtask. Throws unless the module was built with the `test-panic` feature.
   */
  async testPanic(inTask: boolean): Promise<void> {
    const f = testPanicExport();
    if (!f) throw new Error("not built with test-panic");
    await this.#run(() => undefined, () => f(inTask));
  }
}
