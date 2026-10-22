// Endpoint list test (design note 009) for the iroh-do Worker on Miniflare (workerd).
//
//   node tests/workerd/registry-test.mjs
//
// 1. Mints, with and without a label, are recorded in the registry; the list is newest first.
// 2. The page escapes labels (a label carrying HTML renders as text).
// 3. /status/<E> returns the object's status; for an unminted E it returns 404 and the object
//    has no tables afterwards. Minting still requires the token.
// 4. Backfill: an endpoint missing from the registry, as one minted before the registry
//    existed, registers itself on its next wake.
import { mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { DatabaseSync } from "node:sqlite";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { Miniflare } from "miniflare";

const here = dirname(fileURLToPath(import.meta.url));
const bundle = join(here, "..", "..", "build", "worker");
const port = Number(process.env.PORT ?? 8847);
const ADMIN_TOKEN = "registry-test-token";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;
const check = (name, ok, detail = "") => {
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? `: ${detail}` : ""}`);
  if (!ok) failures++;
};

const persistDir = mkdtempSync(join(tmpdir(), "iroh-do-registry-"));
const text = (value) => ({ type: "text", value });
const start = () => new Miniflare({
  host: "127.0.0.1", port, resourcePersistencePath: persistDir,
  workers: [{ config: {
    name: "iroh-do", type: "worker", compatibilityDate: "2026-09-15",
    manifest: { mainModule: "index.js", modules: {
      "index.js": { type: "esm", contents: readFileSync(join(bundle, "index.js"), "utf8") },
      "index_bg.wasm": { type: "wasm", contents: new Uint8Array(readFileSync(join(bundle, "index_bg.wasm"))) } } },
    env: {
      ENDPOINTS: { type: "durable-object", worker: "iroh-do", exportName: "endpoint" },
      REGISTRY: { type: "durable-object", worker: "iroh-do", exportName: "registry" },
      ZONE_SUFFIX: text("localhost"), ADMIN_TOKEN: text(ADMIN_TOKEN), PKARR_RELAY: text(""), BUNDLE_SHA256: text("registry-test") },
    exports: {
      endpoint: { type: "durable-object", storage: "sqlite" },
      registry: { type: "durable-object", storage: "sqlite" } } } }],
});
const admin = (path, init) => mf.dispatchFetch(`http://admin.localhost:${port}${path}`, init);
const mint = async (body) => {
  const r = await admin("/mint", { method: "POST", headers: { Authorization: `Bearer ${ADMIN_TOKEN}` }, body });
  const t = await r.text();
  if (r.status !== 200) throw new Error(`mint ${r.status}: ${t}`);
  return JSON.parse(t).id;
};
const list = async () => (await (await admin("/endpoints.json")).json());
const sqliteFile = async (cls, name) => {
  const ns = await mf.getDurableObjectNamespace(cls === "registry" ? "REGISTRY" : "ENDPOINTS");
  const id = ns.idFromName(name).toString();
  const f = readdirSync(persistDir, { recursive: true }).find((x) => x.endsWith(`${id}.sqlite`));
  return f && join(persistDir, f);
};
const tablesOf = (file) => {
  if (!file) return [];
  const db = new DatabaseSync(file, { readOnly: true });
  try {
    return db.prepare("SELECT name FROM sqlite_master WHERE type = 'table'").all().map((r) => r.name)
      .filter((n) => !n.startsWith("_cf_") && !n.startsWith("sqlite_"));
  } finally {
    db.close();
  }
};

let mf = start();
try {
  await mf.ready;
  const evil = `<script>alert("x")</script><img src=x onerror=alert(1)>`;
  const a = await mint(JSON.stringify({ label: evil }));
  await sleep(5);
  const b = await mint("");
  let l = await list();
  check("mints are registered, newest first", l.length === 2 && l[0].id === b && l[1].id === a, JSON.stringify(l));
  check("label stored as given", l[1].label === evil && l[0].label === "");
  const tooLong = await admin("/mint", { method: "POST", headers: { Authorization: `Bearer ${ADMIN_TOKEN}` },
    body: JSON.stringify({ label: "x".repeat(65) }) });
  check("label over 64 characters refused", tooLong.status === 400, String(tooLong.status));
  const noToken = await admin("/mint", { method: "POST", body: "{}" });
  check("mint without token refused", noToken.status === 401, String(noToken.status));

  const page = await admin("/");
  const html = await page.text();
  check("page served as HTML", page.status === 200 && page.headers.get("content-type")?.startsWith("text/html"));
  check("page lists both endpoints with the chat command", html.includes(`iroh-do-chat ${a}`) && html.includes(`iroh-do-chat ${b}`));
  check("label is escaped", !html.includes(evil) && !html.includes("<img") &&
    html.includes("&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;&lt;img src=x onerror=alert(1)&gt;"));
  check("page has only its own script", html.split("<script>").length === 2);

  const st = await admin(`/status/${a}`);
  const sj = st.status === 200 ? await st.json() : { status: st.status, body: await st.text() };
  check("/status/<E> returns the object's status", sj?.id === a && "present" in sj && "members" in sj, JSON.stringify(sj));
  const unminted = "yr9ge793ppirfkkm4edbyu56mbnctp4i9xsf3468xsda9o8cuo6o";
  const su = await admin(`/status/${unminted}`);
  check("/status/<unminted> is 404", su.status === 404, String(su.status));
  const ut = tablesOf(await sqliteFile("endpoint", unminted));
  check("unminted status request wrote no storage", ut.length === 0, JSON.stringify(ut));
  check("/status/<not z32> is 404", (await admin("/status/registry")).status === 404);

  // Backfill: forget `b` in the registry and in its own flag, as for an endpoint minted
  // before the registry existed.
  const regFile = await sqliteFile("registry", "registry");
  const bFile = await sqliteFile("endpoint", b);
  await mf.dispose();
  const reg = new DatabaseSync(regFile);
  reg.prepare("DELETE FROM endpoints WHERE id = ?").run(b);
  reg.close();
  const bdb = new DatabaseSync(bFile);
  bdb.prepare("DELETE FROM counters WHERE name = 'registered'").run();
  bdb.close();
  mf = start();
  await mf.ready;
  l = await list();
  check("forgotten endpoint is not listed", !l.some((e) => e.id === b), JSON.stringify(l.map((e) => e.id)));
  const up = await mf.dispatchFetch(`http://${b}.localhost:${port}/relay`, {
    headers: { Upgrade: "websocket", "Sec-WebSocket-Protocol": "iroh-relay-v2, iroh-relay-v1" },
  });
  up.webSocket?.accept();
  up.webSocket?.close(1000, "done");
  await sleep(500);
  l = await list();
  const back = l.find((e) => e.id === b);
  check("backfill: the endpoint registered itself on its next wake", !!back && back.minted_at > 0, JSON.stringify(back));
} catch (e) {
  console.log(`FAIL exception: ${e?.stack ?? e}`);
  failures++;
} finally {
  await mf?.dispose().catch(() => {});
  rmSync(persistDir, { recursive: true, force: true });
}
console.log(failures ? `${failures} FAILURE(S)` : "ALL PASS");
process.exit(failures ? 1 : 0);
