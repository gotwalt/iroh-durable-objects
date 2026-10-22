// The Worker's `fetch`: routes relay connections to the object named by the host label, and
// serves the admin surface over Workers RPC to the objects.
import { log } from "./log";
import type { IrohDurableObject } from "./object";
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
  const a = new TextEncoder().encode(m?.[1].trim() ?? "");
  const b = new TextEncoder().encode(expected);
  const ok = a.length === b.length && crypto.subtle.timingSafeEqual(a, b);
  log(ok ? "info" : "warn", "admin_auth", { ok, path });
  return ok ? null : new Response("unauthorized", { status: 401 });
}

/** Options of `irohRouter`. */
export interface RouterOptions {
  /** The Durable Object namespace binding of the endpoint class. */
  binding: string;
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

/**
 * The Worker's `fetch` handler.
 *
 * - `GET <E>.<zone>/relay` with a WebSocket upgrade goes to the object named `E`.
 * - `GET <E>.<zone>/ping` and `/generate_204` are answered here: iroh's net_report sends
 *   them to a peer's home relay.
 * - `POST admin.<zone>/mint` (bearer token) generates a key and mints it in the object named
 *   by its public key. The key never leaves Cloudflare.
 * - `DELETE admin.<zone>/endpoints/<E>` (bearer token) destroys `E`.
 * - `GET admin.<zone>/status/<E>` returns `E`'s status.
 *
 * Anything else is 404 without touching an object.
 */
export function irohRouter(opts: RouterOptions) {
  return async (req: Request, env: RouterEnv): Promise<Response> => {
    instance(env.LOG_LEVEL);
    const zone = env.ZONE_SUFFIX;
    if (!zone) {
      log("error", "config_error", { missing: "ZONE_SUFFIX" });
      return new Response("misconfigured", { status: 500 });
    }
    const ns = (env as Record<string, unknown>)[opts.binding] as DurableObjectNamespace<IrohDurableObject>;
    const url = new URL(req.url);
    const label = labelFor(url.host, zone);
    if (label === null) return new Response("not found", { status: 404 });

    if (label === "admin") {
      if (url.pathname === "/mint") {
        if (req.method !== "POST") return new Response("method not allowed", { status: 405 });
        const denied = adminAuth(req, env.ADMIN_TOKEN, url.pathname);
        if (denied) return denied;
        // Generated in the isolate (crypto.getRandomValues), passed to the object over RPC
        // inside Cloudflare, and never returned.
        const secret = generateSecretKey();
        const id = publicKeyOf(secret).toZ32();
        const relayUrl = `${url.protocol}//${id}.${zone}${url.port ? `:${url.port}` : ""}/`;
        const pkarrRelay = env.PKARR_RELAY ?? DEFAULT_PKARR_RELAY;
        const minted = await ns.get(ns.idFromName(id)).mint(secret, relayUrl, pkarrRelay);
        return Response.json(minted);
      }
      const destroy = /^\/endpoints\/([^/]+)$/.exec(url.pathname);
      if (destroy && req.method === "DELETE" && parseEndpointLabel(destroy[1])) {
        const denied = adminAuth(req, env.ADMIN_TOKEN, "/endpoints");
        if (denied) return denied;
        const res = await ns.get(ns.idFromName(destroy[1])).destroy();
        return res ? Response.json(res) : new Response("not found", { status: 404 });
      }
      const status = /^\/status\/([^/]+)$/.exec(url.pathname);
      if (status && req.method === "GET" && parseEndpointLabel(status[1])) {
        const s = await ns.get(ns.idFromName(status[1])).status();
        return s ? Response.json(s) : new Response("not found", { status: 404 });
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
