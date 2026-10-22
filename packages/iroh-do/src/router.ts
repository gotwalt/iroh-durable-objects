// The Worker's `fetch`: routes relay connections to the object named by the host label, and
// serves the admin surface over Workers RPC to the objects.
import { log } from "./log";
import type { IrohDurableObject } from "./object";
import { type IrohRegistry, REGISTRY_NAME, listPage, mintBody } from "./registry";
import { EndpointId, generateSecretKey, instance, publicKeyOf } from "./wasm";

/** Default pkarr relay (iroh 1.4.0, `N0_DNS_PKARR_RELAY_PROD`); `PKARR_RELAY` overrides it. */
export const DEFAULT_PKARR_RELAY = "https://dns.iroh.link/pkarr";

const Z32 = /^[ybndrfg8ejkmcpqxot1uwisza345h769]{52}$/;

/** Parses an endpoint label: 52 characters of z-base-32 that decode to a valid key. */
function parseEndpointLabel(label: string): EndpointId | null {
  if (!Z32.test(label)) return null;
  try {
    // PublicKey::from_z32 in Rust, never FromStr (design note 004).
    return EndpointId.fromZ32(label);
  } catch {
    return null;
  }
}

/** The leftmost label of `host` if the rest is `zone`, lower-cased; port ignored. */
function labelFor(host: string, zone: string): string | null {
  const h = host.replace(/:\d+$/, "").replace(/\.$/, "").toLowerCase();
  const dot = h.indexOf(".");
  if (dot <= 0 || h.slice(dot + 1) !== zone.toLowerCase().replace(/\.$/, "")) return null;
  return h.slice(0, dot);
}

/** Checks `Authorization: Bearer <ADMIN_TOKEN>`; null if allowed, else the response. */
function adminAuth(req: Request, expected: string | undefined, path: string): Response | null {
  if (!expected) {
    log("error", "config_error", { missing: "ADMIN_TOKEN" });
    return new Response("admin disabled", { status: 503 });
  }
  const m = /^bearer\s+(.+)$/i.exec(req.headers.get("authorization")?.trim() ?? "");
  const a = new TextEncoder().encode(m?.[1]?.trim() ?? "");
  const b = new TextEncoder().encode(expected);
  const ok = a.length === b.length && crypto.subtle.timingSafeEqual(a, b);
  log(ok ? "info" : "warn", "admin_auth", { ok, path });
  return ok ? null : new Response("unauthorized", { status: 401 });
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
  /**
   * The deployment's apps by name, each with its class's binding. The first is the default
   * for a mint that names none. Requires `registry`.
   */
  apps?: Record<string, AppRoute>;
  /**
   * The binding of the registry's namespace (class `IrohRegistry`). Mint records `E → app`
   * there; relay, status and destroy requests find `E`'s class through it.
   */
  registry?: string;
}

/** The bindings the router reads. */
export interface RouterEnv {
  /** The zone the endpoints live under: `<E>.<ZONE_SUFFIX>`, `admin.<ZONE_SUFFIX>`. */
  ZONE_SUFFIX?: string;
  /** Bearer token for mint and destroy. Unset or empty disables them. */
  ADMIN_TOKEN?: string;
  /** pkarr relay base URL for new endpoints; "" disables publishing. */
  PKARR_RELAY?: string;
  /** tracing directives for the wasm side's log (design note 010). */
  LOG_LEVEL?: string;
}

type Namespace = DurableObjectNamespace<IrohDurableObject>;

/**
 * `E → app`, per isolate. The mapping never changes after mint, so an entry needs no expiry.
 * A destroyed endpoint's entry routes to an object that answers 404 because it has no
 * identity. Bounded: an attacker can make the router look up any number of valid ids, but
 * only registered ones are cached.
 */
const appCache = new Map<string, string>();
const APP_CACHE_MAX = 10_000;

function cacheApp(id: string, app: string): void {
  if (appCache.size >= APP_CACHE_MAX) appCache.clear();
  appCache.set(id, app);
}

/** How a request finds the class of `E`'s object. */
interface Classes {
  /** `E`'s app and the namespace of its class, or null if `E` is not routable. */
  resolve(id: string): Promise<{ app: string; ns: Namespace } | null>;
}

/**
 * The Worker's `fetch` handler.
 *
 * - `GET <E>.<zone>/relay` with a WebSocket upgrade goes to the object named `E`.
 * - `GET <E>.<zone>/ping` and `/generate_204` are answered here: iroh's net_report sends
 *   them to a peer's home relay.
 * - `POST admin.<zone>/mint` (bearer token) generates a key and mints it in the object named
 *   by its public key. The key never leaves Cloudflare. With `apps`, the body may name the
 *   app and a label: `{"app": "chat", "label": "..."}`.
 * - `DELETE admin.<zone>/endpoints/<E>` (bearer token) destroys `E`.
 * - `GET admin.<zone>/status/<E>` returns `E`'s status.
 * - With `registry`: `GET admin.<zone>/` and `/endpoints.json` list the minted endpoints.
 *
 * Anything else is 404 without touching an object. With `registry`, a relay, status or
 * destroy request for a valid id that is not cached in the isolate asks the registry first.
 */
export function irohRouter(opts: RouterOptions) {
  const apps = opts.apps ? Object.entries(opts.apps) : [];
  if (apps.length > 0 && !opts.registry) throw new Error("irohRouter: apps requires registry");
  if (apps.length === 0 && !opts.binding) throw new Error("irohRouter: give binding, or apps and registry");
  const multi = apps.length > 0;

  return async (req: Request, env: RouterEnv): Promise<Response> => {
    instance(env.LOG_LEVEL);
    const zone = env.ZONE_SUFFIX;
    if (!zone) {
      log("error", "config_error", { missing: "ZONE_SUFFIX" });
      return new Response("misconfigured", { status: 500 });
    }
    const vars = env as Record<string, unknown>;
    const nsOf = (binding: string) => vars[binding] as Namespace;
    const registry = () => {
      const ns = vars[opts.registry!] as DurableObjectNamespace<IrohRegistry>;
      return ns.get(ns.idFromName(REGISTRY_NAME));
    };
    const classes: Classes = multi
      ? {
        resolve: async (id) => {
          let app = appCache.get(id) ?? null;
          if (app === null) {
            app = await registry().lookup(id);
            if (app !== null) cacheApp(id, app);
          }
          const route = app === null ? undefined : opts.apps![app];
          if (app !== null && !route) log("warn", "route_unknown_app", { endpoint: id, app });
          return route ? { app: app!, ns: nsOf(route.binding) } : null;
        },
      }
      : { resolve: async () => ({ app: "", ns: nsOf(opts.binding!) }) };

    const url = new URL(req.url);
    const label = labelFor(url.host, zone);
    if (label === null) return new Response("not found", { status: 404 });

    if (label === "admin") {
      if (url.pathname === "/mint") {
        if (req.method !== "POST") return new Response("method not allowed", { status: 405 });
        const denied = adminAuth(req, env.ADMIN_TOKEN, url.pathname);
        if (denied) return denied;
        return multi ? mintMulti(req, env, url, zone, apps, nsOf, registry) : mintSingle(env, url, zone, nsOf(opts.binding!));
      }
      if (multi && req.method === "GET" && url.pathname === "/") {
        const hint = (app: string, id: string) => opts.apps![app]?.joinHint?.(id) ?? id;
        return new Response(listPage(await registry().list(), hint), {
          headers: { "content-type": "text/html; charset=utf-8" },
        });
      }
      if (multi && req.method === "GET" && url.pathname === "/endpoints.json") {
        return Response.json(await registry().list());
      }
      const destroy = /^\/endpoints\/([^/]+)$/.exec(url.pathname)?.[1];
      if (destroy !== undefined && req.method === "DELETE" && parseEndpointLabel(destroy)) {
        const denied = adminAuth(req, env.ADMIN_TOKEN, "/endpoints");
        if (denied) return denied;
        const id = destroy;
        const c = await classes.resolve(id);
        if (!c) return new Response("not found", { status: 404 });
        const res = await c.ns.get(c.ns.idFromName(id)).destroy();
        if (!multi) return res ? Response.json(res) : new Response("not found", { status: 404 });
        // As the Rust router: after the object, the registry. A retry after a failed
        // registry update finds the object unminted (404) and removes the entry again.
        appCache.delete(id);
        let removed = false;
        try {
          removed = await registry().unregister(id);
        } catch (e) {
          log("warn", "registry_remove", { endpoint: id, ok: false, error: String(e) });
        }
        if (!removed && res) return new Response("destroyed; registry update failed, retry", { status: 502 });
        return res ? Response.json(res) : new Response("not found", { status: 404 });
      }
      const status = /^\/status\/([^/]+)$/.exec(url.pathname)?.[1];
      if (status !== undefined && req.method === "GET" && parseEndpointLabel(status)) {
        const c = await classes.resolve(status);
        const s = c ? await c.ns.get(c.ns.idFromName(status)).status() : null;
        return s ? Response.json(s) : new Response("not found", { status: 404 });
      }
      if (multi && (url.pathname === "/" || url.pathname === "/endpoints.json")) {
        return new Response("method not allowed", { status: 405 });
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
        const c = await classes.resolve(label);
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
}

/** Generates a key in the isolate and the relay URL for it. */
function newKey(url: URL, zone: string): { secret: Uint8Array; id: string; relayUrl: string } {
  // Generated in the isolate (crypto.getRandomValues), passed to the object over RPC inside
  // Cloudflare, and never returned.
  const secret = generateSecretKey();
  const id = publicKeyOf(secret).toZ32();
  const relayUrl = `${url.protocol}//${id}.${zone}${url.port ? `:${url.port}` : ""}/`;
  return { secret, id, relayUrl };
}

async function mintSingle(env: RouterEnv, url: URL, zone: string, ns: Namespace): Promise<Response> {
  const { secret, id, relayUrl } = newKey(url, zone);
  const pkarrRelay = env.PKARR_RELAY ?? DEFAULT_PKARR_RELAY;
  return Response.json(await ns.get(ns.idFromName(id)).mint(secret, relayUrl, pkarrRelay));
}

/**
 * Mint in a deployment with several apps. The registry entry is written before the object's
 * identity, because the router cannot reach `E` without it; if the mint then fails, the entry
 * is removed again. The id is returned only when both succeeded.
 */
async function mintMulti(
  req: Request,
  env: RouterEnv,
  url: URL,
  zone: string,
  apps: [string, AppRoute][],
  nsOf: (binding: string) => Namespace,
  registry: () => DurableObjectStub<IrohRegistry>,
): Promise<Response> {
  const body = mintBody(await req.text());
  if ("error" in body) return new Response(body.error, { status: 400 });
  // No app named: the deployment's first.
  const entry = body.app === null ? apps[0] : apps.find(([name]) => name === body.app);
  if (!entry) {
    const names = apps.map(([n]) => JSON.stringify(n)).join(", ");
    return new Response(`unknown app; this deployment has [${names}]`, { status: 400 });
  }
  const [app, route] = entry;
  const { secret, id, relayUrl } = newKey(url, zone);
  const pkarrRelay = env.PKARR_RELAY ?? DEFAULT_PKARR_RELAY;
  const reg = registry();
  if (!(await reg.register({ id, app, label: body.label, minted_at: Date.now() }))) {
    return new Response("registry refused the entry", { status: 500 });
  }
  log("info", "registry_register", { endpoint: id, from: "mint", ok: true });
  const ns = nsOf(route.binding);
  try {
    const minted = await ns.get(ns.idFromName(id)).mint(secret, relayUrl, pkarrRelay);
    cacheApp(id, app);
    return Response.json({ id: minted.id, app, relay_url: minted.relay_url, published: minted.published });
  } catch (e) {
    log("error", "mint_failed", { endpoint: id, app, error: String(e) });
    await reg.unregister(id).catch(() => false);
    return new Response("mint failed", { status: 500 });
  }
}
