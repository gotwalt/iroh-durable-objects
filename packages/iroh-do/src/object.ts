// The Durable Object base class. It owns the platform: hibernatable WebSockets and their
// attachments, SQLite, the alarm and env. The wasm library owns the relay ingress sessions,
// the iroh endpoint and its lifecycle, and the Connection and stream API.
import { DurableObject } from "cloudflare:workers";
import { namespaceVar, stringVar } from "./env.js";
import { NodeBuildError, NodeLostError, type NodeLostCause, NodeTrappedError, NotMintedError } from "./errors.js";
import { log } from "./log.js";
import type { Connection, Endpoint } from "./types.js";
import {
  type IngressResult,
  instance,
  liveNodes,
  type NodeHost,
  noteTrap,
  pkarrConfirm,
  pkarrRecord,
  publicKeyOf,
  testPanicExport,
  WasmNode,
} from "./wasm.js";

/** Handles accepted connections for one ALPN, like iroh's `ProtocolHandler`. */
export interface ProtocolHandler {
  /**
   * Runs for each accepted connection. When the returned promise settles, the node closes
   * the connection with code 0 and reason `done`, as iroh's `Router` drops it when `accept`
   * returns. A handler that keeps using the connection keeps the promise pending, for
   * example until `conn.closed()`.
   */
  accept(conn: Connection): Promise<void>;
  /** Idle limit for this ALPN instead of `tIdleMs`. Also applies to dials with it. */
  idleMs?: number;
}

/** The app's protocol handlers, by ALPN. */
export type Protocols = Record<string, ProtocolHandler>;

/** Node timings. Defaults are iroh-do-core's: t_idle 10 s, t_silent 60 s, t_linger 2 s. */
export interface Timings {
  tIdleMs?: number | undefined;
  tSilentMs?: number | undefined;
  tLingerMs?: number | undefined;
}

/**
 * The status JSON of a node, as `iroh_do_core::Ctx::status` gives it to a Rust app, plus the
 * app's fields. The field names are those of the JSON the Rust host serves.
 */
export interface EndpointStatus {
  /** EndpointId, z-base-32. */
  id: string;
  /** The app's name (`static app`). */
  app: string;
  state: "Cold" | "Warm" | "Active";
  /** Constructions of this object, this one included. */
  wakes: number;
  /** Endpoints bound since mint. */
  endpoint_builds: number;
  /** Open QUIC connections. */
  connections: number;
  /** First 12 characters of `BUNDLE_SHA256`. */
  bundle: string;
  since_wake_ms: number;
  /** The app's fields (`appStatus()`), as JSON values: the status crosses Workers RPC. */
  [field: string]: StatusValue;
}

/** A JSON value without nesting. */
export type JsonScalar = string | number | boolean | null;

/**
 * A status field: a JSON scalar, an array of them, or an object of them. Not any JSON value:
 * the status crosses Workers RPC, whose types do not take recursive ones.
 */
export type StatusValue = JsonScalar | JsonScalar[] | { [key: string]: JsonScalar };

/** `EndpointStatus` and the host's fields, for operators (`GET /status/<E>` with the token). */
export interface HostStatus extends EndpointStatus {
  /** Node tasks alive; 0 in Warm. */
  tasks: number;
  /** Stream halves counted open. */
  open_streams: number;
  /** Socket ids of the relay sockets. */
  sockets: string[];
  /** The wasm instance generation the node was built for. */
  generation: number;
  /** Node rebuilds after a wasm reset since this construction. */
  rebuilds: number;
  /** Nodes alive in this isolate's wasm instance, all objects together. */
  isolate_nodes: number;
  pkarr: {
    failures: number | null;
    status: string | null;
    ok_ms: number | null;
    next_ms: number | null;
  };
}

/** What `irohMint` returns. */
export interface MintResult {
  /** EndpointId, z-base-32. */
  id: string;
  relayUrl: string;
  /** Whether the pkarr record was published and confirmed by GET. */
  published: boolean;
}

/** What `irohDestroy` returns for a minted endpoint. */
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
  "CREATE TABLE IF NOT EXISTS iroh_state (name TEXT PRIMARY KEY, value)",
  // Alarm work: `sweep`, `republish`, and `app:` keys. One alarm is armed for the earliest
  // row; a timer would keep the object from hibernating.
  "CREATE TABLE IF NOT EXISTS iroh_schedule (key TEXT PRIMARY KEY, due_ms REAL NOT NULL, payload TEXT NOT NULL)",
];

const SWEEP = "sweep";
const REPUBLISH = "republish";
/** Prefix of the app's schedule keys, the one `iroh_do_core::Ctx::schedule` uses. */
const APP_KEY = "app:";
/** Delay before alarm work that failed runs again (design.md, "Failure handling"). */
const RETRY_MS = 30 * 1000;
/** Close code and reason for the sockets of a destroyed endpoint. */
const CLOSE_DESTROYED = 1000;

// pkarr timings (crates/iroh-do-core/src/pkarr.rs, design note 004).
const REPUBLISH_INTERVAL_MS = 24 * 60 * 60 * 1000;
const RETRY_BASE_MS = 60 * 1000;
const RETRY_JITTER_MS = 30 * 1000;
/** Limit for each pkarr request; the mint waits for both. */
const PKARR_TIMEOUT_MS = 10 * 1000;

interface Attachment {
  v: 1;
  sid: string;
  /** Ingress session snapshot, JSON from the wasm side. */
  s: string;
}

function isAttachment(v: unknown): v is Attachment {
  if (typeof v !== "object" || v === null) return false;
  const a = v as Record<string, unknown>;
  return a.v === 1 && typeof a.sid === "string" && typeof a.s === "string";
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

/** A non-negative finite number of milliseconds, or undefined (and a log line) for anything else. */
function timing(name: string, v: number | undefined): number | undefined {
  if (v === undefined) return undefined;
  if (Number.isFinite(v) && v >= 0) return v;
  log("error", "config_error", { invalid: name, value: String(v) });
  return undefined;
}

function str(v: unknown): string | null {
  return typeof v === "string" ? v : null;
}

function num(v: unknown): number | null {
  return typeof v === "number" ? v : null;
}

/**
 * A Durable Object that is one iroh endpoint, named by its EndpointId in z-base-32.
 *
 * Subclasses return their protocol handlers from `protocols()`. Peers reach the object
 * through `irohRouter`, which forwards `wss://<id>.<zone>/relay` here. A subclass that
 * overrides an event method (`fetch`, `webSocketMessage`, `webSocketClose`,
 * `webSocketError`, `alarm`) must call the base method.
 *
 * Methods whose names start with `iroh` are the router's Workers RPC surface. They are
 * reachable by any code of the deployment that holds the namespace binding, as every public
 * method of a Durable Object is.
 */
export abstract class IrohDurableObject<Env = unknown> extends DurableObject<Env> {
  /**
   * The binding of this class's namespace in `env`. The object checks that the key it is
   * given at mint names it, and that its stored identity does on every build.
   */
  static namespace = "ENDPOINTS";

  /**
   * The app's name, reported as `app` in the status. Unset, the status reports the wasm
   * library's placeholder, `ts`.
   */
  static app: string | undefined = undefined;

  /**
   * The app's protocol handlers. Called once per node build: on the first event after
   * construction, and again after a wasm reset. Called only in a minted object, inside the
   * transaction that builds the node, so it is also where an app creates its tables. It must
   * not use `this.endpoint`, which exists only once the node is built; handlers may.
   */
  abstract protocols(): Protocols;

  /** Node timings; override to change them. */
  protected timings(): Timings {
    return {};
  }

  /**
   * Runs work the app scheduled with `schedule(key, ...)`. Called from the alarm, with the
   * node built but not necessarily bound. Runs at least once per schedule: if it throws, or
   * the instance traps or the object is evicted while it runs, it runs again, after 30 s in
   * the first two cases. It must therefore be idempotent.
   */
  protected onAlarm(_key: string, _payload: string): void | Promise<void> {}

  /** Fields the app adds to the endpoint's status. */
  protected appStatus(): Record<string, unknown> {
    return {};
  }

  /**
   * Schedules `onAlarm(key, payload)` after `delayMs`, replacing pending work with the same
   * key. The row is stored, so the work runs even if the object hibernates in between.
   * Throws `NotMintedError` before mint.
   */
  protected schedule(key: string, delayMs: number, payload = ""): void {
    this.#requireMinted();
    this.#schedule(APP_KEY + key, Date.now() + delayMs, payload);
  }

  /** Cancels pending work scheduled with `key`. Throws `NotMintedError` before mint. */
  protected cancel(key: string): void {
    this.#requireMinted();
    this.#unschedule(APP_KEY + key);
  }

  /**
   * The endpoint's status as `iroh_do_core::Ctx::status` gives it to a Rust app, keys
   * sorted as serde_json writes them. Throws `NotMintedError` before mint.
   */
  protected endpointStatus(): EndpointStatus {
    const node = this.#enter();
    if (!node) throw new NotMintedError();
    return this.#nodeStatus(node);
  }

  /**
   * This endpoint: `id()`, `addr()`, `peers()`, `connect(peer, alpn)`. Throws
   * `NotMintedError` before mint. After a wasm reset, the next access rebuilds the node and
   * returns the new endpoint; objects obtained from the old one are stale (`nodeSignal`).
   */
  get endpoint(): Endpoint {
    this.#enter();
    if (!this.#endpoint) throw new NotMintedError();
    return this.#endpoint;
  }

  /**
   * Aborted when the current node stops being usable: after a wasm reset (another object's
   * panic can cause one) and on destroy. Its `reason` is a `NodeLostError`. Every `Endpoint`,
   * `Connection` and stream obtained from the node is stale from then on, and calls on them
   * throw. A new signal comes with each node build; `protocols()` is the place to subscribe,
   * for example to drop cached connections. Throws `NotMintedError` before mint.
   */
  protected get nodeSignal(): AbortSignal {
    this.#enter();
    if (!this.#nodeAbort) throw new NotMintedError();
    return this.#nodeAbort.signal;
  }

  /**
   * Runs `body` with the endpoint, for a subclass's own RPC methods and other entry points
   * the base class does not wrap. If `body` traps the wasm instance, the instance is replaced,
   * this object's node rebuilt, and a `NodeTrappedError` thrown.
   */
  protected async withNode<T>(body: (endpoint: Endpoint) => T | Promise<T>): Promise<T> {
    try {
      return await body(this.endpoint);
    } catch (e) {
      if (!noteTrap(e)) throw e;
      this.#enter();
      throw new NodeTrappedError(e);
    }
  }

  #node: WasmNode | null = null;
  #endpoint: Endpoint | null = null;
  /** The node holds its host weakly (crates/iroh-do-wasm/src/host.rs): keep it alive here. */
  #host: NodeHost | null = null;
  #nodeAbort: AbortController | null = null;
  #handlers: Protocols = {};
  #sockets = new Map<string, WebSocket>();
  /** Whether this construction has decided its node: built, or found no identity. */
  #loaded = false;
  /** Set while `#load` runs: `#enter` must not start a second build from inside it. */
  #building = false;
  #inTransaction = false;
  /** The wasm generation the node was built for (wasm.ts). */
  #generation = -1;
  /** Node builds since construction caused by a wasm reset. */
  #rebuilds = 0;
  /** Set by `#lose`: the next `#load` is a rebuild, not a wake. */
  #rebuilding = false;
  /** This endpoint's id, once known; for log lines. */
  #id: string | null = null;
  /** Whether the `iroh_*` tables exist; null until checked. */
  #tables: boolean | null = null;
  /** Bumped by destroy: work that awaited across it must not write. */
  #epoch = 0;
  /** The alarm time last set: null for none, undefined when not known. */
  #armed: number | null | undefined = undefined;

  #nodeStatus(node: WasmNode): EndpointStatus {
    const app = (this.constructor as typeof IrohDurableObject).app;
    const all: Record<string, unknown> = {
      ...(JSON.parse(node.status()) as Record<string, unknown>),
      ...(app ? { app } : {}),
      ...this.appStatus(),
    };
    return Object.fromEntries(
      Object.keys(all)
        .sort()
        .map((k) => [k, all[k]]),
    ) as EndpointStatus;
  }

  #sql(query: string, ...params: unknown[]) {
    return this.ctx.storage.sql.exec(query, ...params);
  }

  /** Runs `fn` in one storage transaction; inside another, as part of that one. */
  #transaction<T>(fn: () => T): T {
    if (this.#inTransaction) return fn();
    this.#inTransaction = true;
    try {
      return this.ctx.storage.transactionSync(fn);
    } finally {
      this.#inTransaction = false;
    }
  }

  /** Whether the object was minted: the identity table exists only after mint. */
  #minted(): boolean {
    if (this.#tables === null) {
      this.#tables =
        this.#sql("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'iroh_identity'").toArray().length > 0;
    }
    return this.#tables;
  }

  #requireMinted(): void {
    if (!this.#minted()) throw new NotMintedError();
  }

  #identity(): Identity | null {
    if (!this.#minted()) return null;
    const row = this.#sql("SELECT secret, id, relay_url, pkarr_relay FROM iroh_identity WHERE k = 1").toArray()[0];
    if (!row || !(row.secret instanceof ArrayBuffer)) return null;
    const id = str(row.id);
    const relayUrl = str(row.relay_url);
    const pkarrRelay = str(row.pkarr_relay);
    if (id === null || relayUrl === null || pkarrRelay === null) return null;
    return { secret: new Uint8Array(row.secret), id, relayUrl, pkarrRelay };
  }

  #get(name: string): unknown {
    return this.#sql("SELECT value FROM iroh_state WHERE name = ?", name).toArray()[0]?.value ?? null;
  }

  #put(name: string, value: string | number | null): void {
    if (value === null) this.#sql("DELETE FROM iroh_state WHERE name = ?", name);
    else
      this.#sql(
        "INSERT INTO iroh_state (name, value) VALUES (?, ?) ON CONFLICT(name) DO UPDATE SET value = excluded.value",
        name,
        value,
      );
  }

  #bump(name: string): number {
    const row = this.#sql(
      "INSERT INTO iroh_state (name, value) VALUES (?, 1) ON CONFLICT(name) DO UPDATE SET value = value + 1 RETURNING value",
      name,
    ).one();
    return Number(row.value);
  }

  /** Whether `id` (z-base-32) names this object. */
  #isMe(id: string): boolean {
    const binding = (this.constructor as typeof IrohDurableObject).namespace;
    const ns = namespaceVar(this.env, binding);
    if (!ns) {
      log("error", "config_error", { missing: binding, class: this.constructor.name });
      throw new Error(`binding ${binding} (static namespace of ${this.constructor.name}) is not a Durable Object namespace`);
    }
    return ns.idFromName(id).equals(this.ctx.id);
  }

  /**
   * Starts every entry point: replaces a trapped wasm instance, rebuilds the node if it
   * belongs to an older instance, and builds it on the first event of this construction.
   */
  #enter(): WasmNode | null {
    if (this.#building) return this.#node;
    const generation = instance(stringVar(this.env, "LOG_LEVEL"));
    if (this.#node && this.#generation !== generation) {
      this.#lose("wasm_reset");
      this.#rebuilding = true;
      this.#loaded = false;
    }
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

  /** Drops the node, aborting its signal. The caller rebuilds or forgets it. */
  #lose(cause: NodeLostCause): void {
    if (cause === "wasm_reset") {
      const boundAt = this.#get("bound_at_ms");
      this.#put("bound_at_ms", null);
      this.#rebuilds++;
      const rebuilds = this.#bump("node_rebuilds");
      log("warn", "node_rebuild", {
        endpoint: this.#id,
        generation: instance(),
        rebuilds,
        // The endpoint was bound: its QUIC connections are gone, and peers redial.
        was_active: boundAt !== null,
      });
    }
    const abort = this.#nodeAbort;
    this.#node = null;
    this.#endpoint = null;
    this.#host = null;
    this.#nodeAbort = null;
    this.#sockets.clear();
    abort?.abort(new NodeLostError(cause));
  }

  /**
   * Builds the node: loads the identity, creates the wasm node, and rebuilds the session of
   * every socket from its attachment. Runs on the first event after construction (a wake)
   * and after a wasm reset. The storage writes, the app's `protocols()` and the node's
   * construction are one transaction: if any throws, nothing is written, the error is
   * logged as `node_build_failed` and thrown as `NodeBuildError`, and the next event tries
   * again.
   *
   * Not done in the constructor: `protocols()` belongs to the subclass, whose fields are not
   * initialized while the base constructor runs.
   */
  #load(generation: number): void {
    const ident = this.#identity();
    if (!ident || !this.#isMe(ident.id)) {
      if (ident) log("error", "identity_invariant_violated", { object: this.ctx.id.toString() });
      for (const ws of this.ctx.getWebSockets()) ws.close(1011, "no identity");
      this.#loaded = true;
      return;
    }
    this.#id = ident.id;
    const rebuild = this.#rebuilding;
    this.#building = true;
    try {
      this.#transaction(() => this.#build(ident, generation, rebuild));
    } catch (e) {
      // The transaction rolled back; drop what it made in memory.
      this.#node?.detach();
      this.#node?.free();
      this.#node = null;
      this.#endpoint = null;
      this.#host = null;
      this.#nodeAbort = null;
      if (noteTrap(e)) throw e;
      log("error", "node_build_failed", { endpoint: ident.id, error: String(e) });
      throw new NodeBuildError(e);
    } finally {
      this.#building = false;
    }
    this.#rebuilding = false;
    this.#loaded = true;
    // Republishing must never stop: a run cut short may have lost the row.
    if (ident.pkarrRelay && this.#sql("SELECT 1 FROM iroh_schedule WHERE key = ?", REPUBLISH).toArray().length === 0) {
      this.#schedule(REPUBLISH, Date.now() + RETRY_BASE_MS);
    }
    this.#restoreSockets(ident.id, rebuild);
  }

  #build(ident: Identity, generation: number, rebuild: boolean): void {
    const wakes = rebuild ? Number(this.#get("wakes") ?? 0) : this.#bump("wakes");
    if (!rebuild) {
      // Set while an endpoint is bound, cleared when it closes (iroh_do_core::Host). Found
      // set by a new construction, it means the previous instance was evicted, not
      // hibernated, with QUIC connections open.
      const boundAt = num(this.#get("bound_at_ms"));
      if (boundAt !== null) {
        log("warn", "evicted_active", { endpoint: ident.id, since_bind_ms: Date.now() - boundAt, wakes });
        this.#put("bound_at_ms", null);
      }
    }
    this.#nodeAbort = new AbortController();
    this.#handlers = this.protocols();
    const host: NodeHost = {
      sendFrame: (sid, frame) => {
        const ws = this.#sockets.get(sid);
        if (!ws) throw new Error("no socket");
        // Copies `frame`, a view into wasm memory, before returning.
        ws.send(frame);
      },
      accept: (alpn, conn) => {
        const h = this.#handlers[alpn];
        // On a later microtask, so that the handler never runs inside a wasm call, and a
        // promise even if it throws synchronously: its settling closes the connection.
        return h ? Promise.resolve().then(() => h.accept(conn)) : Promise.resolve();
      },
      schedule: (key, delayMs, payload) => this.#schedule(key, Date.now() + delayMs, payload),
      cancel: (key) => this.#unschedule(key),
      endpointBuilt: () => {
        this.#bump("endpoint_builds");
        this.#put("bound_at_ms", Date.now());
      },
      endpointClosed: () => this.#put("bound_at_ms", null),
    };
    const t = this.timings();
    const node = new WasmNode(
      ident.secret,
      {
        wakes,
        endpointBuilds: Number(this.#get("endpoint_builds") ?? 0),
        bundle: stringVar(this.env, "BUNDLE_SHA256") ?? "",
        relayUrl: ident.relayUrl,
        protocols: Object.entries(this.#handlers).map(([alpn, h]) => {
          const idleMs = timing(`protocols["${alpn}"].idleMs`, h.idleMs);
          return idleMs === undefined ? { alpn } : { alpn, idleMs };
        }),
        ...defined({
          tIdleMs: timing("tIdleMs", t.tIdleMs),
          tSilentMs: timing("tSilentMs", t.tSilentMs),
          tLingerMs: timing("tLingerMs", t.tLingerMs),
        }),
      },
      host,
    );
    this.#node = node;
    this.#host = host;
    this.#endpoint = node.endpoint() as Endpoint;
    this.#generation = generation;
    this.#tables = true;
  }

  /** Rebuilds the ingress session of every socket from its attachment. */
  #restoreSockets(id: string, rebuild: boolean): void {
    const node = this.#node;
    if (!node) return;
    const sockets = this.ctx.getWebSockets();
    const restored: [WebSocket, string, Attachment | null][] = [];
    for (const ws of sockets) {
      const raw: unknown = ws.deserializeAttachment();
      const att = isAttachment(raw) ? raw : null;
      const sid = att?.sid ?? this.#sidOf(ws);
      if (sid === undefined) {
        // Neither an attachment nor a tag: not a socket this class accepted.
        ws.close(1011, "lost session");
        continue;
      }
      this.#sockets.set(sid, ws);
      restored.push([ws, sid, att]);
    }
    let closed = sockets.length - restored.length;
    for (const [ws, sid, att] of restored) {
      const res = node.restore(sid, att?.s, Date.now());
      closed += res.close.length;
      this.#apply(res, ws, sid);
    }
    log("info", "wake", {
      endpoint: id,
      wakes: Number(this.#get("wakes") ?? 0),
      cause: rebuild ? "wasm_reset" : "construct",
      sockets: sockets.length - closed,
      sockets_closed: closed,
      state: node.state(),
    });
  }

  #sidOf(ws: WebSocket): string | undefined {
    return this.ctx
      .getTags(ws)
      .find((t) => t.startsWith("sid:"))
      ?.slice(4);
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
      key,
      dueMs,
      payload,
    );
    this.#rearm();
  }

  #unschedule(key: string): void {
    this.#sql("DELETE FROM iroh_schedule WHERE key = ?", key);
    this.#rearm();
  }

  #scheduleEarliest(key: string, dueMs: number): void {
    this.#sql(
      "INSERT INTO iroh_schedule (key, due_ms, payload) VALUES (?, ?, '') ON CONFLICT(key) DO UPDATE SET due_ms = min(due_ms, excluded.due_ms)",
      key,
      dueMs,
    );
    this.#rearm();
  }

  /**
   * Arms the alarm for the earliest row, or deletes it if there is none. Storage writes are
   * not awaited: the runtime applies them in order and holds outgoing messages until they
   * are durable; a failure resets the object.
   */
  #rearm(): void {
    if (!this.#minted()) return;
    const next = num(this.#sql("SELECT min(due_ms) AS due FROM iroh_schedule").one().due);
    if (next === null) {
      if (this.#armed !== null) {
        this.#armed = null;
        this.ctx.storage.deleteAlarm().catch((e: unknown) => log("error", "storage_failed", { op: "deleteAlarm", error: String(e) }));
      }
      return;
    }
    if (this.#armed === next) return;
    this.#armed = next;
    // A time in the past fires at once.
    this.ctx.storage.setAlarm(next).catch((e: unknown) => log("error", "storage_failed", { op: "setAlarm", error: String(e) }));
  }

  override async fetch(req: Request): Promise<Response> {
    try {
      return await this.#run(
        () => new Response("unavailable", { status: 503 }),
        (node) => {
          if (!node) return new Response("not found", { status: 404 });
          const url = new URL(req.url);
          const r = node.upgrade(req.method, url.pathname, req.headers.get("sec-websocket-protocol") ?? undefined, Date.now());
          if (!r.ok) return new Response(r.message, { status: r.status });
          const { 0: client, 1: server } = new WebSocketPair();
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
        },
      );
    } catch (e) {
      if (e instanceof NodeBuildError) return new Response("unavailable", { status: 503 });
      throw e;
    }
  }

  override async webSocketMessage(ws: WebSocket, msg: string | ArrayBuffer): Promise<void> {
    await this.#run(
      () => undefined,
      (node) => {
        const sid = this.#sidOf(ws);
        if (!node || !sid) {
          ws.close(1011, "refused");
          return;
        }
        const res = typeof msg === "string" ? node.textMessage(sid) : node.message(sid, new Uint8Array(msg), Date.now());
        this.#apply(res, ws, sid);
      },
    );
  }

  override async webSocketClose(ws: WebSocket, code: number, reason: string, wasClean: boolean): Promise<void> {
    await this.#run(
      () => undefined,
      (node) => {
        const sid = this.#sidOf(ws);
        if (sid && this.#sockets.has(sid)) {
          log("info", "socket_close", { endpoint: this.#id, by: "peer", code, reason, clean: wasClean });
          this.#sockets.delete(sid);
          node?.socketClosed(sid);
        }
      },
    );
    try {
      ws.close(sendableCloseCode(code), "bye");
    } catch {
      // Already closed.
    }
  }

  override async webSocketError(ws: WebSocket, error: unknown): Promise<void> {
    await this.#run(
      () => undefined,
      (node) => {
        const sid = this.#sidOf(ws);
        if (sid && this.#sockets.has(sid)) {
          log("warn", "socket_close", { endpoint: this.#id, by: "error", error: String(error) });
          this.#sockets.delete(sid);
          node?.socketClosed(sid);
        }
      },
    );
  }

  /**
   * Runs the due rows of `iroh_schedule`. A row is deleted only after its work has run; if
   * the work throws or traps the instance, the row is moved 30 s on. The alarm is armed again
   * for the earliest remaining row whatever happens, so that a trap, which `#run` turns into
   * a normal return, cannot leave due rows without an alarm. A row whose work was cut short
   * by an eviction stays due, and the runtime retries the alarm.
   */
  override async alarm(): Promise<void> {
    // The runtime cleared the alarm to run this.
    this.#armed = null;
    const epoch = this.#epoch;
    try {
      await this.#run(
        () => undefined,
        async (node) => {
          if (node) await this.#runDue(node, epoch);
        },
      );
    } finally {
      if (this.#epoch === epoch) this.#rearm();
    }
  }

  async #runDue(node: WasmNode, epoch: number): Promise<void> {
    const t0 = Date.now();
    const due = this.#sql("SELECT key, due_ms, payload FROM iroh_schedule WHERE due_ms <= ? ORDER BY due_ms", t0).toArray();
    const kinds: string[] = [];
    let failed = 0;
    for (const row of due) {
      const key = str(row.key);
      const dueMs = num(row.due_ms);
      const payload = str(row.payload) ?? "";
      if (key === null || dueMs === null) continue;
      // Kinds, not keys: an app key may carry a peer's EndpointId.
      const kind = key.startsWith(APP_KEY) ? "app" : key;
      kinds.push(kind);
      try {
        if (key === SWEEP) {
          const res = node.sweep(Date.now());
          this.#complete(key, dueMs);
          this.#apply(res);
        } else if (key === REPUBLISH) {
          // Schedules the next attempt itself.
          await this.#publish(epoch);
        } else if (key.startsWith(APP_KEY)) {
          await this.onAlarm(key.slice(APP_KEY.length), payload);
          this.#complete(key, dueMs);
        } else {
          node.alarm(key, payload);
          this.#complete(key, dueMs);
        }
      } catch (e) {
        failed++;
        const trap = noteTrap(e);
        if (this.#epoch === epoch) this.#retry(key, dueMs);
        log(trap ? "error" : "warn", "alarm_failed", {
          endpoint: this.#id,
          due: kind,
          error: String(e),
          trap,
          retry_in_s: RETRY_MS / 1000,
        });
        // The node belongs to a trapped instance: the rest waits for the next alarm, which
        // the rearm in `alarm` sets at once since the rows are due.
        if (trap) throw e;
      }
      if (this.#epoch !== epoch) return;
    }
    log("info", "alarm", { endpoint: this.#id, due: kinds.join(","), ran: kinds.length, failed, ms: Date.now() - t0 });
  }

  /** Deletes `key` if it still has the due time it ran for, not one its work set again. */
  #complete(key: string, dueMs: number): void {
    this.#sql("DELETE FROM iroh_schedule WHERE key = ? AND due_ms = ?", key, dueMs);
  }

  /** Moves `key` on by the retry delay, unless its work scheduled it again itself. */
  #retry(key: string, dueMs: number): void {
    this.#sql("UPDATE iroh_schedule SET due_ms = ? WHERE key = ? AND due_ms = ?", Date.now() + RETRY_MS, key, dueMs);
  }

  /**
   * Publishes the pkarr record and confirms it with a GET, records the outcome, and
   * schedules the next attempt: after a day, or after a linear backoff with jitter. Writes
   * nothing if the endpoint was destroyed while it waited.
   */
  async #publish(epoch: number): Promise<boolean> {
    const ident = this.#identity();
    if (!ident?.pkarrRelay) return false;
    const t0 = Date.now();
    let ok = false;
    let status = "ok";
    try {
      const rec = pkarrRecord(ident.secret, ident.relayUrl, ident.pkarrRelay);
      const put = await fetch(rec.url, { method: "PUT", body: rec.body, signal: AbortSignal.timeout(PKARR_TIMEOUT_MS) });
      await put.body?.cancel();
      if (!put.ok) {
        status = `put status ${put.status}`;
      } else {
        const get = await fetch(rec.url, {
          headers: { accept: "application/x-pkarr-signed-packet" },
          signal: AbortSignal.timeout(PKARR_TIMEOUT_MS),
        });
        if (get.status !== 200) {
          await get.body?.cancel();
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
    if (this.#epoch !== epoch) return false;
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
   * identity yet and the key's id names this object, builds the node, then publishes the
   * pkarr record. The identity and the node build are one transaction: if the build fails,
   * nothing is stored. `pkarrRelay` "" disables publishing.
   */
  async irohMint(secret: Uint8Array, relayUrl: string, pkarrRelay: string): Promise<MintResult> {
    const trapped = (): MintResult => {
      throw new Error("wasm instance trapped during mint");
    };
    return this.#run(trapped, async () => {
      const t0 = Date.now();
      const key = publicKeyOf(secret);
      const id = key.toZ32();
      key.free();
      if (!this.#isMe(id)) {
        log("error", "mint_name_mismatch", { object: this.ctx.id.toString() });
        throw new Error("name mismatch");
      }
      if (this.#identity()) throw new Error("identity exists");
      this.#transaction(() => {
        for (const s of SCHEMA) this.#sql(s);
        this.#sql(
          "INSERT INTO iroh_identity (k, secret, id, relay_url, pkarr_relay, minted_at) VALUES (1, ?, ?, ?, ?, ?)",
          secret,
          id,
          relayUrl,
          pkarrRelay,
          Date.now(),
        );
        this.#tables = true;
        // The first event found no identity and built no node. A failed build throws out
        // of the transaction, which rolls back the identity.
        this.#loaded = false;
        try {
          this.#enter();
        } catch (e) {
          this.#tables = null;
          this.#loaded = false;
          throw e;
        }
      });
      const published = await this.#publish(this.#epoch);
      log("info", "mint", { endpoint: id, published, ms: Date.now() - t0 });
      return { id, relayUrl, published };
    });
  }

  /**
   * RPC from the router: closes every socket, drops the node, and deletes the alarm and all
   * storage, identity included. Afterwards the object is indistinguishable from one that was
   * never minted. Returns null if there is no identity to destroy.
   */
  async irohDestroy(): Promise<DestroyResult | null> {
    return this.#run(
      () => null,
      async (node) => {
        // An object that refuses traffic (identity invariant violated) holds an identity too.
        const ident = this.#identity();
        if (!ident) return null;
        const t0 = Date.now();
        this.#epoch++;
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
        this.#lose("destroy");
        this.#loaded = true;
        this.#tables = false;
        this.#armed = null;
        await this.ctx.storage.deleteAlarm();
        await this.ctx.storage.deleteAll();
        log("info", "destroy", { endpoint: ident.id, sockets: sockets.length, ms: Date.now() - t0 });
        this.#id = null;
        return { id: ident.id, destroyed: true };
      },
    );
  }

  /** RPC from the router: the node's status and the host's fields, or null before mint. */
  async irohStatus(): Promise<HostStatus | null> {
    return this.#run(
      () => null,
      (node) => {
        if (!node) return null;
        const next = num(this.#sql("SELECT due_ms FROM iroh_schedule WHERE key = ?", REPUBLISH).toArray()[0]?.due_ms);
        return {
          ...this.#nodeStatus(node),
          tasks: node.tasks(),
          open_streams: node.openStreams(),
          sockets: [...this.#sockets.keys()],
          generation: this.#generation,
          rebuilds: this.#rebuilds,
          isolate_nodes: liveNodes(),
          pkarr: {
            failures: num(this.#get("pkarr_failures")),
            status: str(this.#get("pkarr_status")),
            ok_ms: num(this.#get("pkarr_ok_ms")),
            next_ms: next,
          },
        };
      },
    );
  }

  /**
   * Test only: panics in the wasm instance, directly or (`inTask`) in a task on a later
   * microtask. Throws unless the module was built with the `test-panic` feature, which
   * production bundles refuse.
   */
  async irohTestPanic(inTask: boolean): Promise<void> {
    const f = testPanicExport();
    if (!f) throw new Error("not built with test-panic");
    await this.#run(
      () => undefined,
      () => f(inTask),
    );
  }
}

/** `o` without its undefined fields, for `exactOptionalPropertyTypes`. */
function defined<T extends Record<string, unknown>>(o: T): { [K in keyof T]?: Exclude<T[K], undefined> } {
  return Object.fromEntries(Object.entries(o).filter(([, v]) => v !== undefined)) as {
    [K in keyof T]?: Exclude<T[K], undefined>;
  };
}
