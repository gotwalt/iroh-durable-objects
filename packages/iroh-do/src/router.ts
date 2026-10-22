// The Worker's `fetch`: routes relay connections to the object named by the host label, and
// serves the admin surface over Workers RPC to the objects.
import { namespaceVar, stringVar } from "./env.js";
import { log } from "./log.js";
import type { EndpointStatus, HostStatus, IrohDurableObject } from "./object.js";
import { type IrohRegistry, isEndpointId, listPage, mintBody, REGISTRY_NAME, type RegistryEntry } from "./registry.js";
import { generateSecretKey, instance, publicKeyOf } from "./wasm.js";

/** Default pkarr relay (iroh 1.4.0, `N0_DNS_PKARR_RELAY_PROD`); `PKARR_RELAY` overrides it. */
export const DEFAULT_PKARR_RELAY = "https://dns.iroh.link/pkarr";

const Z32 = /^[ybndrfg8ejkmcpqxot1uwisza345h769]{52}$/;

/** Whether `label` is 52 characters of z-base-32 that decode to a valid key. */
function isEndpointLabel(label: string): boolean {
  return Z32.test(label) && isEndpointId(label);
}

/** The leftmost label of `host` if the rest is `zone`, lower-cased; port ignored. */
function labelFor(host: string, zone: string): string | null {
  const h = host.replace(/:\d+$/, "").replace(/\.$/, "").toLowerCase();
  const dot = h.indexOf(".");
  if (dot <= 0 || h.slice(dot + 1) !== zone.toLowerCase().replace(/\.$/, "")) return null;
  return h.slice(0, dot);
}

/** One app of a deployment with several (`RouterOptions.apps`). */
export interface AppRoute {
  /** The Durable Object namespace binding of the app's class. */
  binding: string;
  /** The command the endpoint list shows for an endpoint of this app. Default: the id. */
  joinHint?: (id: string) => string;
}

/**
 * Options of `irohRouter`. Give `binding` for a deployment with one endpoint class, or `apps`
 * and `registry` for one with several.
 */
export interface RouterOptions {
  /** The Durable Object namespace binding of the endpoint class, for a single-class deployment. */
  binding?: string;
  /** The deployment's apps by name, each with its class's binding. Requires `registry`. */
  apps?: Record<string, AppRoute>;
  /** The app a mint that names none gets. Default: the first key of `apps`. */
  defaultApp?: string;
  /**
   * The binding of the registry's namespace (class `IrohRegistry`). Mint records `E → app`
   * there; relay, status and destroy requests find `E`'s class through it.
   */
  registry?: string;
}

/** The bindings the router reads, besides the namespaces named in `RouterOptions`. */
export interface RouterEnv {
  /** The zone the endpoints live under: `<E>.<ZONE_SUFFIX>`, `admin.<ZONE_SUFFIX>`. */
  ZONE_SUFFIX?: string;
  /** Bearer token for mint, destroy and the full status. Unset or empty disables them. */
  ADMIN_TOKEN?: string;
  /** pkarr relay base URL for new endpoints; "" disables publishing. */
  PKARR_RELAY?: string;
  /** tracing directives for the wasm side's log (design note 010). */
  LOG_LEVEL?: string;
}

/** What `irohRouter` returns. */
export interface IrohRouter {
  /** The Worker's `fetch`: every request gets a response, 404 outside the zone. */
  fetch(req: Request, env: unknown): Promise<Response>;
  /**
   * The same, but null for a request whose host is not in the zone, for a Worker that serves
   * other hosts too.
   */
  handle(req: Request, env: unknown): Promise<Response | null>;
}

type Namespace = DurableObjectNamespace<IrohDurableObject>;
type Registry = DurableObjectStub<IrohRegistry>;

/** The fields of the status anyone may read: the node's and the app's, not the host's. */
const PRIVATE_STATUS: readonly (keyof HostStatus)[] = [
  "tasks",
  "open_streams",
  "sockets",
  "generation",
  "rebuilds",
  "isolate_nodes",
  "pkarr",
];

function publicStatus(s: HostStatus): EndpointStatus {
  const out: Record<string, unknown> = { ...s };
  for (const k of PRIVATE_STATUS) delete out[k];
  return out as EndpointStatus;
}

/**
 * `E → app`, per isolate, and the ids found unregistered. `E → app` never changes after mint,
 * so a positive entry needs no expiry; a destroyed endpoint's entry routes to an object that
 * answers 404 because it has no identity. A negative entry expires after a minute, so that a
 * new mint in another isolate is reachable here soon after. Both maps are bounded: when full
 * they are cleared.
 */
class AppCache {
  static readonly MAX = 10_000;
  static readonly NEGATIVE_TTL_MS = 60_000;
  #apps = new Map<string, string>();
  #absent = new Map<string, number>();

  get(id: string): string | null | undefined {
    const app = this.#apps.get(id);
    if (app !== undefined) return app;
    const until = this.#absent.get(id);
    if (until === undefined) return undefined;
    if (until > Date.now()) return null;
    this.#absent.delete(id);
    return undefined;
  }

  set(id: string, app: string | null): void {
    if (app === null) {
      if (this.#absent.size >= AppCache.MAX) this.#absent.clear();
      this.#absent.set(id, Date.now() + AppCache.NEGATIVE_TTL_MS);
      return;
    }
    if (this.#apps.size >= AppCache.MAX) this.#apps.clear();
    this.#absent.delete(id);
    this.#apps.set(id, app);
  }

  delete(id: string): void {
    this.#apps.delete(id);
    this.#absent.delete(id);
  }
}

/** Failed admin token checks since the last `admin_auth` warning; see `AdminAuth`. */
class AdminAuth {
  static readonly LOG_EVERY_MS = 60_000;
  #suppressed = 0;
  #lastWarn = 0;

  /** Checks `Authorization: Bearer <expected>`; null if allowed, else the response. */
  async check(req: Request, expected: string | undefined, path: string): Promise<Response | null> {
    if (!expected) {
      log("error", "config_error", { missing: "ADMIN_TOKEN" });
      return new Response("admin disabled", { status: 503 });
    }
    const m = /^bearer\s+(.+)$/i.exec(req.headers.get("authorization")?.trim() ?? "");
    const ok = await tokenEquals(m?.[1]?.trim() ?? "", expected);
    if (ok) {
      log("info", "admin_auth", { ok, path });
      return null;
    }
    // One warning a minute at most, with the count: failures are cheap to send.
    this.#suppressed++;
    const now = Date.now();
    if (now - this.#lastWarn >= AdminAuth.LOG_EVERY_MS) {
      log("warn", "admin_auth", { ok, path, failures: this.#suppressed });
      this.#suppressed = 0;
      this.#lastWarn = now;
    }
    return new Response("unauthorized", { status: 401 });
  }

  /** Whether the request carries the token, without logging or a response. */
  async has(req: Request, expected: string | undefined): Promise<boolean> {
    if (!expected) return false;
    const m = /^bearer\s+(.+)$/i.exec(req.headers.get("authorization")?.trim() ?? "");
    return m?.[1] !== undefined && (await tokenEquals(m[1].trim(), expected));
  }
}

/** Compares SHA-256 digests in constant time, so that neither content nor length leaks. */
async function tokenEquals(given: string, expected: string): Promise<boolean> {
  const enc = new TextEncoder();
  const [a, b] = await Promise.all([
    crypto.subtle.digest("SHA-256", enc.encode(given)),
    crypto.subtle.digest("SHA-256", enc.encode(expected)),
  ]);
  return crypto.subtle.timingSafeEqual(a, b);
}

/** A request the router could not complete because a binding or an RPC failed. */
class Unavailable extends Error {}

/**
 * The Worker's request handler.
 *
 * - `GET <E>.<zone>/relay` with a WebSocket upgrade goes to the object named `E`.
 * - `GET <E>.<zone>/ping` and `/generate_204` are answered here: iroh's net_report sends
 *   them to a peer's home relay.
 * - `POST admin.<zone>/mint` (bearer token) generates a key and mints it in the object named
 *   by its public key. The key never leaves Cloudflare. With `apps`, the body may name the
 *   app and a label: `{"app": "chat", "label": "..."}`.
 * - `DELETE admin.<zone>/endpoints/<E>` (bearer token) destroys `E`.
 * - `GET admin.<zone>/status/<E>` returns `E`'s status: the node's and the app's fields, and
 *   with the bearer token also the host's (`HostStatus`).
 * - With `registry`: `GET admin.<zone>/` and `/endpoints.json` list the minted endpoints.
 *
 * Anything else is 404 without touching an object. A binding that is missing or an RPC that
 * fails is a 503. With `registry`, a relay, status or destroy request for a valid id that is
 * not cached in the isolate asks the registry first.
 */
export function irohRouter(opts: RouterOptions): IrohRouter {
  const apps = opts.apps ? Object.entries(opts.apps) : [];
  if (apps.length > 0 && !opts.registry) throw new Error("irohRouter: apps requires registry");
  if (apps.length === 0 && !opts.binding) throw new Error("irohRouter: give binding, or apps and registry");
  const defaultApp = opts.defaultApp ?? apps[0]?.[0];
  if (apps.length > 0 && !apps.some(([name]) => name === defaultApp)) {
    throw new Error(`irohRouter: defaultApp ${JSON.stringify(defaultApp)} is not in apps`);
  }
  const cache = new AppCache();
  const auth = new AdminAuth();

  const handle = async (req: Request, env: unknown): Promise<Response | null> => {
    instance(stringVar(env, "LOG_LEVEL"));
    const zone = stringVar(env, "ZONE_SUFFIX");
    if (!zone) {
      log("error", "config_error", { missing: "ZONE_SUFFIX" });
      return new Response("misconfigured", { status: 500 });
    }
    const url = new URL(req.url);
    const label = labelFor(url.host, zone);
    if (label === null) return null;
    try {
      return await route(req, env, url, zone, label);
    } catch (e) {
      // A missing binding, or a registry or object call that failed (overloaded, reset,
      // threw): logged, and a 503 rather than the runtime's opaque exception page.
      log("error", "route_failed", {
        path: url.pathname,
        error: String(e),
        ...(e instanceof Unavailable ? {} : rpcFlags(e)),
      });
      return new Response("unavailable", { status: 503 });
    }
  };

  const ns = (env: unknown, binding: string): Namespace => {
    const n = namespaceVar<IrohDurableObject>(env, binding);
    if (!n) {
      log("error", "config_error", { missing: binding });
      throw new Unavailable(`binding ${binding} is not a Durable Object namespace`);
    }
    return n;
  };
  const registry = (env: unknown): Registry => {
    const b = opts.registry ?? "";
    const n = namespaceVar<IrohRegistry>(env, b);
    if (!n) {
      log("error", "config_error", { missing: b });
      throw new Unavailable(`binding ${b} is not a Durable Object namespace`);
    }
    return n.get(n.idFromName(REGISTRY_NAME));
  };

  /** `E`'s app and the namespace of its class, or null if `E` is not routable. */
  const resolve = async (env: unknown, id: string): Promise<{ app: string; ns: Namespace } | null> => {
    if (opts.binding !== undefined && apps.length === 0) return { app: "", ns: ns(env, opts.binding) };
    let app = cache.get(id);
    if (app === undefined) {
      app = await registry(env).lookup(id);
      cache.set(id, app);
    }
    if (app === null) return null;
    const r = opts.apps?.[app];
    if (!r) {
      log("warn", "route_unknown_app", { endpoint: id, app });
      return null;
    }
    return { app, ns: ns(env, r.binding) };
  };

  const route = async (req: Request, env: unknown, url: URL, zone: string, label: string): Promise<Response> => {
    if (label === "admin") return admin(req, env, url, zone);
    if (!isEndpointLabel(label)) return new Response("not found", { status: 404 });
    switch (url.pathname) {
      case "/relay": {
        if (req.method !== "GET") return new Response("method not allowed", { status: 405 });
        const upgrade = req.headers.get("upgrade") ?? "";
        if (!upgrade.split(",").some((t) => t.trim().toLowerCase() === "websocket")) {
          return new Response("upgrade required", { status: 426 });
        }
        const c = await resolve(env, label);
        if (!c) return new Response("not found", { status: 404 });
        return c.ns.get(c.ns.idFromName(label)).fetch(req);
      }
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

  const admin = async (req: Request, env: unknown, url: URL, zone: string): Promise<Response> => {
    const token = stringVar(env, "ADMIN_TOKEN");
    const multi = apps.length > 0;
    if (url.pathname === "/mint") {
      if (req.method !== "POST") return new Response("method not allowed", { status: 405 });
      const denied = await auth.check(req, token, url.pathname);
      if (denied) return denied;
      return multi ? mintMulti(req, env, url, zone) : mintSingle(env, url, zone);
    }
    if (multi && (url.pathname === "/" || url.pathname === "/endpoints.json")) {
      if (req.method !== "GET") return new Response("method not allowed", { status: 405 });
      const beforeParam = url.searchParams.get("before");
      const before = beforeParam === null ? undefined : Number(beforeParam);
      if (before !== undefined && !Number.isSafeInteger(before)) return new Response("bad before", { status: 400 });
      const page = await registry(env).list(before === undefined ? {} : { before });
      const nextHref = page.next === null ? null : `${url.pathname}?before=${page.next}`;
      if (url.pathname === "/endpoints.json") {
        const headers: Record<string, string> = {};
        if (nextHref) headers.Link = `<${nextHref}>; rel="next"`;
        return Response.json(page.entries.map(wireEntry), { headers });
      }
      const hint = (app: string, id: string) => opts.apps?.[app]?.joinHint?.(id) ?? id;
      return new Response(listPage(page.entries, hint, nextHref), {
        headers: { "content-type": "text/html; charset=utf-8" },
      });
    }
    const destroy = /^\/endpoints\/([^/]+)$/.exec(url.pathname)?.[1];
    if (destroy !== undefined && req.method === "DELETE" && isEndpointLabel(destroy)) {
      const denied = await auth.check(req, token, "/endpoints");
      if (denied) return denied;
      const c = await resolve(env, destroy);
      if (!c) return new Response("not found", { status: 404 });
      const res = await c.ns.get(c.ns.idFromName(destroy)).irohDestroy();
      if (!multi) return res ? Response.json(res) : new Response("not found", { status: 404 });
      // As the Rust router: after the object, the registry. A retry after a failed
      // registry update finds the object unminted (404) and removes the entry again.
      cache.delete(destroy);
      let removed = false;
      try {
        removed = await registry(env).unregister(destroy);
      } catch (e) {
        log("warn", "registry_remove", { endpoint: destroy, ok: false, error: String(e) });
      }
      if (!removed && res) return new Response("destroyed; registry update failed, retry", { status: 502 });
      return res ? Response.json(res) : new Response("not found", { status: 404 });
    }
    const status = /^\/status\/([^/]+)$/.exec(url.pathname)?.[1];
    if (status !== undefined && req.method === "GET" && isEndpointLabel(status)) {
      const c = await resolve(env, status);
      const s = c ? await c.ns.get(c.ns.idFromName(status)).irohStatus() : null;
      if (!s) return new Response("not found", { status: 404 });
      return Response.json((await auth.has(req, token)) ? s : publicStatus(s));
    }
    return new Response("not found", { status: 404 });
  };

  const mintSingle = async (env: unknown, url: URL, zone: string): Promise<Response> => {
    const { secret, id, relayUrl } = newKey(url, zone);
    const n = ns(env, opts.binding ?? "");
    try {
      const minted = await n.get(n.idFromName(id)).irohMint(secret, relayUrl, pkarrRelay(env));
      return Response.json({ id: minted.id, relay_url: minted.relayUrl, published: minted.published });
    } catch (e) {
      log("error", "mint_failed", { endpoint: id, error: String(e) });
      return new Response("mint failed", { status: 500 });
    }
  };

  /**
   * Mint in a deployment with several apps. The registry entry is written before the object's
   * identity, because the router cannot reach `E` without it; if the mint then fails, the
   * object is destroyed (in case it stored the identity before the failure) and the entry
   * removed. The id is returned only when both succeeded.
   */
  const mintMulti = async (req: Request, env: unknown, url: URL, zone: string): Promise<Response> => {
    const body = mintBody(await req.text());
    if ("error" in body) return new Response(body.error, { status: 400 });
    const app = body.app ?? defaultApp;
    const route = app === undefined ? undefined : opts.apps?.[app];
    if (app === undefined || !route) {
      const names = apps.map(([n]) => JSON.stringify(n)).join(", ");
      return new Response(`unknown app; this deployment has [${names}]`, { status: 400 });
    }
    const { secret, id, relayUrl } = newKey(url, zone);
    const reg = registry(env);
    if (!(await reg.register({ id, app, label: body.label, mintedAt: Date.now() }))) {
      return new Response("registry refused the entry", { status: 500 });
    }
    log("info", "registry_register", { endpoint: id, from: "mint", ok: true });
    const n = ns(env, route.binding);
    const stub = n.get(n.idFromName(id));
    try {
      const minted = await stub.irohMint(secret, relayUrl, pkarrRelay(env));
      cache.set(id, app);
      return Response.json({ id: minted.id, app, relay_url: minted.relayUrl, published: minted.published });
    } catch (e) {
      log("error", "mint_failed", { endpoint: id, app, error: String(e) });
      await stub.irohDestroy().catch(() => null);
      await reg.unregister(id).catch(() => false);
      return new Response("mint failed", { status: 500 });
    }
  };

  return {
    handle,
    fetch: async (req, env) => (await handle(req, env)) ?? new Response("not found", { status: 404 }),
  };
}

/** A registry entry as `/endpoints.json` writes it, with the Rust host's field names. */
function wireEntry(e: RegistryEntry): { id: string; app: string; label: string; minted_at: number } {
  return { id: e.id, app: e.app, label: e.label, minted_at: e.mintedAt };
}

function pkarrRelay(env: unknown): string {
  return stringVar(env, "PKARR_RELAY") ?? DEFAULT_PKARR_RELAY;
}

/** The flags Workers RPC sets on an error from a Durable Object call, for the log. */
function rpcFlags(e: unknown): Record<string, boolean> {
  if (typeof e !== "object" || e === null) return {};
  const out: Record<string, boolean> = {};
  for (const k of ["remote", "retryable", "overloaded"]) {
    const v: unknown = Reflect.get(e, k);
    if (typeof v === "boolean") out[k] = v;
  }
  return out;
}

/** Generates a key in the isolate and the relay URL for it. */
function newKey(url: URL, zone: string): { secret: Uint8Array; id: string; relayUrl: string } {
  // Generated in the isolate (crypto.getRandomValues), passed to the object over RPC inside
  // Cloudflare, and never returned.
  const secret = generateSecretKey();
  const key = publicKeyOf(secret);
  const id = key.toZ32();
  key.free();
  const relayUrl = `${url.protocol}//${id}.${zone}${url.port ? `:${url.port}` : ""}/`;
  return { secret, id, relayUrl };
}
