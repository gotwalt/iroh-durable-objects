// The endpoint list (design note 009) and the record of which app each endpoint runs.
//
// Cloudflare lists Durable Objects only by internal id, so the router records every minted
// EndpointId in one registry object, named `registry`. With several apps in one deployment
// the record also routes: the router reads `E → app` from it to find the class whose object
// holds `E` (design note 013, "Consequences"). Ported from `crates/iroh-do/src/registry.rs`
// and `glue/registry.rs`, with the same table.
import { DurableObject } from "cloudflare:workers";
import { log } from "./log";
import { EndpointId, instance } from "./wasm";

/** Name of the one registry object. Not valid z-base-32, so it cannot be an EndpointId. */
export const REGISTRY_NAME = "registry";
/** Longest label, in characters. */
export const MAX_LABEL = 64;
/** Most endpoints the list returns, newest first. */
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
const LIST = "SELECT id, app, label, minted_at FROM endpoints ORDER BY minted_at DESC LIMIT ?";

/** One registered endpoint, as stored and as `/endpoints.json` returns it. */
export interface RegistryEntry {
  /** EndpointId, z-base-32. */
  id: string;
  /** Name of the app the endpoint runs. */
  app: string;
  /** Label given at mint; empty if none. */
  label: string;
  /** Mint time in milliseconds since the UNIX epoch. */
  minted_at: number;
}

function isEndpointId(id: string): boolean {
  if (id.length !== 52) return false;
  try {
    // PublicKey::from_z32, never FromStr (design note 004).
    EndpointId.fromZ32(id);
    return true;
  } catch {
    return false;
  }
}

/**
 * The registry object. One instance, named `registry`, bound as `REGISTRY` by convention. The
 * router calls it over Workers RPC.
 */
export class IrohRegistry extends DurableObject {
  /** `lookup` calls since this construction, for tests of the router's cache. */
  #lookups = 0;

  constructor(ctx: DurableObjectState, env: unknown) {
    super(ctx, env as never);
    instance((env as Record<string, unknown>).LOG_LEVEL as string | undefined);
    for (const s of SCHEMA) ctx.storage.sql.exec(s);
  }

  /** Records `e`. Returns false, and writes nothing, if `e` is malformed. */
  async register(e: RegistryEntry): Promise<boolean> {
    if (!isEndpointId(e.id) || typeof e.app !== "string" || [...e.label].length > MAX_LABEL) return false;
    this.ctx.storage.sql.exec(REGISTER, e.id, e.app, e.label, e.minted_at);
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
    this.#lookups++;
    return (this.ctx.storage.sql.exec(LOOKUP, id).toArray()[0]?.app as string | undefined) ?? null;
  }

  /** The newest registered endpoints, at most `LIST_LIMIT`. */
  async list(): Promise<RegistryEntry[]> {
    return this.ctx.storage.sql.exec(LIST, LIST_LIMIT).toArray().map((r) => ({
      id: r.id as string,
      app: r.app as string,
      label: r.label as string,
      minted_at: r.minted_at as number,
    }));
  }

  /** `lookup` calls since this construction. */
  async lookups(): Promise<number> {
    return this.#lookups;
  }
}

/** Body of `POST admin.<zone>/mint`: the app and label, or an error message. */
export function mintBody(body: string): { app: string | null; label: string } | { error: string } {
  if (body.trim() === "") return { app: null, label: "" };
  let b: unknown;
  try {
    b = JSON.parse(body);
  } catch (e) {
    return { error: `body must be JSON {"app":…, "label":…}: ${(e as Error).message}` };
  }
  if (typeof b !== "object" || b === null || Array.isArray(b)) {
    return { error: 'body must be JSON {"app":…, "label":…}: not an object' };
  }
  const { app, label } = b as { app?: unknown; label?: unknown };
  if ((app !== undefined && app !== null && typeof app !== "string") || (label !== undefined && label !== null && typeof label !== "string")) {
    return { error: 'body must be JSON {"app":…, "label":…}: app and label must be strings' };
  }
  const l = ((label as string | null | undefined) ?? "").trim();
  if ([...l].length > MAX_LABEL) return { error: `label longer than ${MAX_LABEL} characters` };
  // Rust's char::is_control: the Unicode Cc category.
  if (/[\u0000-\u001f\u007f-\u009f]/.test(l)) return { error: "label contains control characters" };
  return { app: (app as string | null | undefined) ?? null, label: l };
}

/** Escapes text for HTML element content and quoted attribute values. */
export function htmlEscape(s: string): string {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
}

/** Formats milliseconds since the UNIX epoch as `YYYY-MM-DD HH:MM UTC`. */
export function utc(ms: number): string {
  const d = new Date(ms);
  const p = (n: number, w = 2) => String(n).padStart(w, "0");
  return `${p(d.getUTCFullYear(), 4)}-${p(d.getUTCMonth() + 1)}-${p(d.getUTCDate())} ${p(d.getUTCHours())}:${p(d.getUTCMinutes())} UTC`;
}

/** Renders the list page. `joinHint(app, id)` is the command shown for an endpoint. */
export function listPage(entries: RegistryEntry[], joinHint: (app: string, id: string) => string): string {
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
      out += `<tr><td>${label}</td><td>${app}</td><td><code>${id}</code> <button data-copy="${id}">copy</button></td>`
        + `<td>${utc(e.minted_at)}</td><td><code>${hint}</code></td>`
        + `<td><button data-status="${id}">show</button> <span class=status></span></td></tr>\n`;
    }
    out += "</table>\n";
  }
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
