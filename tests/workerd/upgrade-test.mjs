// Upgrade test for the iroh-do Worker on Miniflare (workerd): an object minted by an older
// bundle keeps its key and its room after the Worker is redeployed (design note 015).
//
//   UPGRADE_FROM=<dir> node tests/workerd/upgrade-test.mjs
//
// UPGRADE_FROM holds the older bundle (index.js, index_bg.wasm), built with
// `just build-worker` at the older commit; the new bundle is build/worker/. Uses the peer at
// target/debug/iroh-do-chat (override with IROH_DO_DEMO).
//
// 1. The older bundle mints a chat room and runs a scripted session, which writes room events
//    and schedules alarm work in a `schedule` table without `attempt`.
// 2. The new bundle, on the same storage, adds `attempt` with the rows kept, and the same
//    endpoint id passes the scripted session again, alarm dial-back included.
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { DatabaseSync } from "node:sqlite";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { Miniflare } from "miniflare";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "..", "..");
const oldBundle = process.env.UPGRADE_FROM;
if (!oldBundle) throw new Error("set UPGRADE_FROM to the older bundle's directory");
const newBundle = join(repo, "build", "worker");
const demoBin = process.env.IROH_DO_DEMO ?? join(repo, "target", "debug", "iroh-do-chat");
const port = Number(process.env.PORT ?? 8848);
const ADMIN_TOKEN = "upgrade-test-token";
let failures = 0;
const check = (name, ok, detail = "") => {
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? `: ${detail}` : ""}`);
  if (!ok) failures++;
};

const persistDir = mkdtempSync(join(tmpdir(), "iroh-do-upgrade-"));
const text = (value) => ({ type: "text", value });
const start = (bundle) => new Miniflare({
  host: "127.0.0.1", port, resourcePersistencePath: persistDir,
  workers: [{ config: {
    name: "iroh-do", type: "worker", compatibilityDate: "2026-09-15",
    manifest: { mainModule: "index.js", modules: {
      "index.js": { type: "esm", contents: readFileSync(join(bundle, "index.js"), "utf8") },
      "index_bg.wasm": { type: "wasm", contents: new Uint8Array(readFileSync(join(bundle, "index_bg.wasm"))) } } },
    env: {
      ENDPOINTS: { type: "durable-object", worker: "iroh-do", exportName: "endpoint" },
      REGISTRY: { type: "durable-object", worker: "iroh-do", exportName: "registry" },
      ZONE_SUFFIX: text("localhost"), ADMIN_TOKEN: text(ADMIN_TOKEN), PKARR_RELAY: text(""), BUNDLE_SHA256: text("upgrade-test") },
    exports: {
      endpoint: { type: "durable-object", storage: "sqlite" },
      registry: { type: "durable-object", storage: "sqlite" } } } }],
});
const lifecycle = (E) => new Promise((resolve) => {
  const child = spawn(demoBin, ["--local", "--local-port", String(port), "lifecycle", E, "--room"],
    { stdio: ["ignore", "pipe", "pipe"] });
  let out = "";
  child.stdout.on("data", (d) => { out += d; });
  child.stderr.on("data", (d) => { out += d; });
  child.on("close", (code) => resolve({ code, out }));
});
const sqliteFile = async (mf, name) => {
  const ns = await mf.getDurableObjectNamespace("ENDPOINTS");
  const id = ns.idFromName(name).toString();
  const f = readdirSync(persistDir, { recursive: true }).find((x) => x.endsWith(`${id}.sqlite`));
  return f && join(persistDir, f);
};
const inspect = (file) => {
  const db = new DatabaseSync(file, { readOnly: true });
  try {
    return {
      columns: db.prepare("SELECT name FROM pragma_table_info('schedule')").all().map((r) => r.name),
      secret: db.prepare("SELECT secret_hex FROM identity").get()?.secret_hex,
      events: db.prepare("SELECT count(*) AS n FROM log").get().n,
    };
  } finally {
    db.close();
  }
};

let mf = start(oldBundle);
try {
  await mf.ready;
  const r = await mf.dispatchFetch(`http://admin.localhost:${port}/mint`, {
    method: "POST", headers: { Authorization: `Bearer ${ADMIN_TOKEN}` }, body: JSON.stringify({ app: "chat" }) });
  const body = await r.text();
  if (r.status !== 200) throw new Error(`mint ${r.status}: ${body}`);
  const E = JSON.parse(body).id;
  const first = await lifecycle(E);
  check("older bundle: scripted session passes", first.code === 0 && first.out.includes("result: PASS"), first.out.slice(-300));
  const file = await sqliteFile(mf, E);
  await mf.dispose();
  const before = inspect(file);
  check("older bundle: schedule has no attempt", !before.columns.includes("attempt"), before.columns.join(","));

  mf = start(newBundle);
  await mf.ready;
  const second = await lifecycle(E);
  check("new bundle: same id passes the scripted session", second.code === 0 && second.out.includes("result: PASS"), second.out.slice(-300));
  await mf.dispose();
  mf = undefined;
  const after = inspect(file);
  check("new bundle: schedule gained attempt", after.columns.includes("attempt"), after.columns.join(","));
  check("key unchanged", after.secret === before.secret);
  check("room events kept and added to", after.events > before.events, `${before.events} -> ${after.events}`);
} finally {
  await mf?.dispose();
  rmSync(persistDir, { recursive: true, force: true });
}
console.log(failures ? `${failures} FAILED` : "ALL PASS");
process.exit(failures ? 1 : 0);
