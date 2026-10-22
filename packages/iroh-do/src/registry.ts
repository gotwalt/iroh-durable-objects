// The endpoint list (design note 009) and the record of which app each endpoint runs.
//
// Cloudflare lists Durable Objects only by internal id, so the router records every minted
// EndpointId in one registry object, named `registry`. With several apps in one deployment
// the record also routes: the router reads `E → app` from it to find the class whose object
// holds `E` (design note 013, "Consequences"). Ported from `crates/iroh-do/src/registry.rs`
// and `glue/registry.rs`, with the same table.
import { DurableObject } from "cloudflare:workers";
import { stringVar } from "./env.js";
import { log } from "./log.js";
import { GlueEndpointId, instance } from "./wasm.js";

/** Name of the one registry object. Not valid z-base-32, so it cannot be an EndpointId. */
export const REGISTRY_NAME = "registry";
/** Longest label and app name, in characters. */
export const MAX_LABEL = 64;
/** Most endpoints one page of the list holds, newest first. */
export const LIST_LIMIT = 500;

const SCHEMA = [
  `CREATE TABLE IF NOT EXISTS endpoints(
     id TEXT PRIMARY KEY,
     app TEXT NOT NULL,
     label TEXT NOT NULL,
     minted_at INTEGER NOT NULL)`,
  "CREATE INDEX IF NOT EXISTS endpoints_minted ON endpoints(minted_at)",
];
// A second registration of the same id keeps the first.
const REGISTER = `INSERT INTO endpoints(id, app, label, minted_at) VALUES (?, ?, ?, ?)
 ON CONFLICT(id) DO NOTHING`;
const UNREGISTER = "DELETE FROM endpoints WHERE id = ?";
const LOOKUP = "SELECT app FROM endpoints WHERE id = ?";
const LIST = "SELECT id, app, label, minted_at FROM endpoints WHERE minted_at < ? ORDER BY minted_at DESC LIMIT ?";

/** One registered endpoint. `/endpoints.json` writes `mintedAt` as `minted_at`. */
export interface RegistryEntry {
  /** EndpointId, z-base-32. */
  id: string;
  /** Name of the app the endpoint runs. */
  app: string;
  /** Label given at mint; empty if none. */
  label: string;
  /** Mint time in milliseconds since the UNIX epoch. */
  mintedAt: number;
}

/** One page of the list (`IrohRegistry.list`). */
export interface RegistryPage {
  entries: RegistryEntry[];
  /** Pass as `before` for the next, older page; null on the last page. */
  next: number | null;
}

/** Whether `id` is a valid EndpointId in z-base-32. */
export function isEndpointId(id: unknown): id is string {
  if (typeof id !== "string" || id.length !== 52) return false;
  try {
    // PublicKey::from_z32, never FromStr (design note 004).
    GlueEndpointId.fromZ32(id).free();
    return true;
  } catch {
    return false;
  }
}

/** Why `label` is not acceptable, or null. Rust's `char::is_control` is the Cc category. */
export function labelError(label: string): string | null {
  if ([...label].length > MAX_LABEL) return `label longer than ${MAX_LABEL} characters`;
  if (/\p{Cc}/u.test(label)) return "label contains control characters";
  return null;
}

/**
 * The registry object. One instance, named `registry`, bound as `REGISTRY` by convention. The
 * router calls it over Workers RPC.
 */
export class IrohRegistry<Env = unknown> extends DurableObject<Env> {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    instance(stringVar(env, "LOG_LEVEL"));
    for (const s of SCHEMA) ctx.storage.sql.exec(s);
  }

  /** Records `e`. Returns false, and writes nothing, if any field is malformed. */
  async register(e: RegistryEntry): Promise<boolean> {
    const ok =
      isEndpointId(e.id) &&
      typeof e.app === "string" &&
      e.app !== "" &&
      [...e.app].length <= MAX_LABEL &&
      typeof e.label === "string" &&
      labelError(e.label) === null &&
      Number.isSafeInteger(e.mintedAt) &&
      e.mintedAt >= 0;
    if (!ok) return false;
    this.ctx.storage.sql.exec(REGISTER, e.id, e.app, e.label, e.mintedAt);
    log("info", "registry_entry", { endpoint: e.id, app: e.app });
    return true;
  }

  /** Removes `id`. Removing an absent id succeeds. */
  async unregister(id: string): Promise<boolean> {
    if (!isEndpointId(id)) return false;
    this.ctx.storage.sql.exec(UNREGISTER, id);
    log("info", "registry_remove", { endpoint: id });
    return true;
  }

  /** The app `id` was minted with, or null if it is not registered. */
  async lookup(id: string): Promise<string | null> {
    const app: unknown = this.ctx.storage.sql.exec(LOOKUP, id).toArray()[0]?.app;
    return typeof app === "string" ? app : null;
  }

  /**
   * The registered endpoints minted before `before` (default: all), newest first, at most
   * `limit` (default and maximum `LIST_LIMIT`).
   */
  async list(opts: { before?: number; limit?: number } = {}): Promise<RegistryPage> {
    const limit = Math.max(1, Math.min(LIST_LIMIT, Math.floor(opts.limit ?? LIST_LIMIT)));
    const before = opts.before ?? Number.MAX_SAFE_INTEGER;
    const rows = this.ctx.storage.sql.exec(LIST, before, limit + 1).toArray();
    const entries: RegistryEntry[] = [];
    for (const r of rows.slice(0, limit)) {
      if (typeof r.id !== "string" || typeof r.app !== "string" || typeof r.label !== "string") continue;
      entries.push({ id: r.id, app: r.app, label: r.label, mintedAt: Number(r.minted_at) });
    }
    const last = entries[entries.length - 1];
    return { entries, next: rows.length > limit && last ? last.mintedAt : null };
  }
}

/** Body of `POST admin.<zone>/mint`: the app and label, or an error message. */
export function mintBody(body: string): { app: string | null; label: string } | { error: string } {
  if (body.trim() === "") return { app: null, label: "" };
  let b: unknown;
  try {
    b = JSON.parse(body);
  } catch (e) {
    return { error: `body must be JSON {"app":…, "label":…}: ${e instanceof Error ? e.message : String(e)}` };
  }
  if (typeof b !== "object" || b === null || Array.isArray(b)) {
    return { error: 'body must be JSON {"app":…, "label":…}: not an object' };
  }
  const app: unknown = Reflect.get(b, "app");
  const label: unknown = Reflect.get(b, "label");
  if ((app != null && typeof app !== "string") || (label != null && typeof label !== "string")) {
    return { error: 'body must be JSON {"app":…, "label":…}: app and label must be strings' };
  }
  const l = (label ?? "").trim();
  const err = labelError(l);
  if (err) return { error: err };
  return { app: app ?? null, label: l };
}

const ESCAPES: Record<string, string> = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" };

/** Escapes text for HTML element content and quoted attribute values. */
export function htmlEscape(s: string): string {
  return s.replace(/[&<>"']/g, (c) => ESCAPES[c] ?? c);
}

/** Formats milliseconds since the UNIX epoch as `YYYY-MM-DD HH:MM UTC`. */
export function utc(ms: number): string {
  const d = new Date(ms);
  const p = (n: number, w = 2) => String(n).padStart(w, "0");
  return `${p(d.getUTCFullYear(), 4)}-${p(d.getUTCMonth() + 1)}-${p(d.getUTCDate())} ${p(d.getUTCHours())}:${p(d.getUTCMinutes())} UTC`;
}

/**
 * Renders one page of the list. `joinHint(app, id)` is the command shown for an endpoint;
 * `nextHref` links the next, older page.
 */
export function listPage(
  entries: RegistryEntry[],
  joinHint: (app: string, id: string) => string,
  nextHref: string | null = null,
): string {
  let out = PAGE_HEAD;
  if (entries.length === 0) {
    out += "<p>No endpoints yet.</p>\n";
  } else {
    out += "<table>\n<tr><th>Label</th><th>App</th><th>Endpoint</th><th>Minted</th><th>Join</th><th>Status</th></tr>\n";
    for (const e of entries) {
      const id = htmlEscape(e.id);
      const label = e.label === "" ? "<span class=dim>none</span>" : htmlEscape(e.label);
      const app = htmlEscape(e.app);
      const hint = htmlEscape(joinHint(e.app, e.id));
      out +=
        `<tr><td>${label}</td><td>${app}</td><td><code>${id}</code> <button data-copy="${id}">copy</button></td>` +
        `<td>${utc(e.mintedAt)}</td><td><code>${hint}</code></td>` +
        `<td><button data-status="${id}">show</button> <span class=status></span></td></tr>\n`;
    }
    out += "</table>\n";
  }
  if (nextHref !== null) out += `<p><a href="${htmlEscape(nextHref)}">Older endpoints</a></p>\n`;
  return out + PAGE_TAIL;
}

const PAGE_HEAD = `<!doctype html>
<html lang=en><head><meta charset=utf-8><meta name=viewport content="width=device-width">
<title>iroh-do endpoints</title>
<style>
body{font:15px/1.4 system-ui,sans-serif;margin:1.5em;color:#111;background:#fff}
@media (prefers-color-scheme:dark){body{color:#ddd;background:#111}}
table{border-collapse:collapse}td,th{padding:.3em .6em;text-align:left;vertical-align:top}
tr+tr{border-top:1px solid #8884}code{font-size:13px;word-break:break-all}.dim{opacity:.6}
</style></head><body>
<h1>iroh-do endpoints</h1>
<p>Each endpoint is an iroh endpoint hosted in a Cloudflare Durable Object, running one app.
Anyone who knows an endpoint's id can connect to it. Status wakes the endpoint's object.</p>
`;

const PAGE_TAIL = `<script>
document.addEventListener('click', async (ev) => {
  const b = ev.target;
  if (b.dataset.copy) { await navigator.clipboard.writeText(b.dataset.copy); b.textContent = 'copied'; }
  if (b.dataset.status) {
    const out = b.nextElementSibling;
    out.textContent = '…';
    try {
      const r = await fetch('/status/' + b.dataset.status);
      const s = await r.json();
      const core = ['id', 'app', 'state', 'wakes', 'endpoint_builds', 'connections', 'bundle', 'since_wake_ms'];
      const extra = Object.entries(s).filter(([k, v]) => !core.includes(k) && (v === null || typeof v !== 'object')).map(([k, v]) => \`\${k} \${v}\`);
      out.textContent = [s.state, \`\${s.wakes} wakes\`, ...extra].join(', ');
    } catch (e) { out.textContent = 'unavailable'; }
  }
});
</script>
</body></html>
`;
