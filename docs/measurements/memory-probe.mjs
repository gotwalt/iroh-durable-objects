// Memory probe for the iroh-do bundle on Miniflare/workerd (docs/measurements/cost-profile.md).
//
//   node docs/measurements/memory-probe.mjs [peers|bench] > docs/measurements/raw/memory-<mode>.json
//
// Loads build/worker/ unchanged on disk; the in-memory copy of index.js gets a
// test-only `/__mem` route on the router that returns the wasm linear memory size
// (`exports.memory.buffer.byteLength`). Router and Durable Object share one isolate
// and one wasm instance in workerd, so this is the DO's linear memory.
// Peers are stock `iroh-do-demo` processes (target/release) dialing --local.
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "..", "..");
const { Miniflare } = await import(join(repo, "harness/workerd/node_modules/miniflare/dist/src/index.js"));
const bundle = join(repo, "build", "worker");
const demoBin = process.env.IROH_DO_DEMO ?? join(repo, "target", "release", "iroh-do-demo");
const port = Number(process.env.PORT ?? 8827);
const mode = process.argv[2] ?? "peers";
const ADMIN_TOKEN = "memory-probe-token";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const log = (...a) => console.error(...a);

const needle = "D.prototype.fetch=function(e){return ie.call(this,e,this.env,this.ctx)}";
let js = readFileSync(join(bundle, "index.js"), "utf8");
if (!js.includes(needle)) throw new Error("router entry not found in index.js (worker-build changed?)");
js = js.replace(needle,
  "D.prototype.fetch=function(e){if(new URL(e.url).pathname==='/__mem'){" +
  "return new Response(JSON.stringify({wasm_bytes:i.memory.buffer.byteLength}))}" +
  "return ie.call(this,e,this.env,this.ctx)}");

const text = (value) => ({ type: "text", value });
const mf = new Miniflare({
  host: "127.0.0.1",
  port,
  resourcePersistencePath: mkdtempSync(join(tmpdir(), "iroh-do-mem-")),
  workers: [{
    config: {
      name: "iroh-do", type: "worker", compatibilityDate: "2026-09-15",
      manifest: { mainModule: "index.js", modules: {
        "index.js": { type: "esm", contents: js },
        "index_bg.wasm": { type: "wasm", contents: new Uint8Array(readFileSync(join(bundle, "index_bg.wasm"))) },
      } },
      env: {
        ENDPOINTS: { type: "durable-object", worker: "iroh-do", exportName: "IrohEndpoint" },
        ZONE_SUFFIX: text("localhost"), ADMIN_TOKEN: text(ADMIN_TOKEN), PKARR_RELAY: text(""),
        BUNDLE_SHA256: text("memory-probe"),
      },
      exports: { IrohEndpoint: { type: "durable-object", storage: "sqlite" } },
    },
  }],
});

const samples = [];
async function mem(label, extra = {}) {
  const r = await mf.dispatchFetch(`http://127.0.0.1:${port}/__mem`);
  const { wasm_bytes } = await r.json();
  const s = { label, t_ms: Date.now(), wasm_bytes, wasm_mib: +(wasm_bytes / 2 ** 20).toFixed(2), ...extra };
  samples.push(s);
  log(`${label.padEnd(44)} wasm ${s.wasm_mib} MiB`);
  return s;
}

function demo(E, ...args) {
  const child = spawn(demoBin, ["--local", "--local-port", String(port), ...args, E], { stdio: ["pipe", "pipe", "pipe"] });
  let out = "";
  child.stdout.on("data", (d) => (out += d));
  child.stderr.on("data", (d) => (out += d));
  const done = new Promise((r) => child.on("exit", (code) => r(code)));
  return { child, send: (l) => child.stdin.write(l + "\n"), end: () => child.stdin.end(), out: () => out, done };
}
async function waitFor(p, pred, ms = 30_000) {
  const t0 = Date.now();
  while (Date.now() - t0 < ms) { if (pred(p.out())) return true; await sleep(100); }
  return false;
}

const peers = [];
try {
  await mf.ready;
  await mem("isolate up, module instantiated (router only)");
  const res = await mf.dispatchFetch(`http://admin.localhost:${port}/mint`, {
    method: "POST", headers: { Authorization: `Bearer ${ADMIN_TOKEN}` } });
  const E = (await res.json()).id;
  await mem("(a) after mint: DO constructed, no Endpoint");

  if (mode === "bench") {
    const b = demo(E, "bench", "--mb", "16");
    const code = await b.done;
    await mem("(e) after bench --mb 16 (1 connection)", { exit: code, out: b.out().trim().split("\n").slice(-1)[0] });
    await sleep(14_000);
    await mem("(e) +14 s: connection idle-closed, Warm/Cold");
  } else {
    const open = async (n) => {
      const batch = [];
      while (peers.length < n) {
        const p = demo(E, "chat");
        peers.push(p); batch.push(p);
        p.send(`hello from peer ${peers.length}`);
      }
      const ok = await Promise.all(batch.map((p) => waitFor(p, (o) => o.includes("<< echo: hello"), 60_000)));
      return ok.filter(Boolean).length;
    };
    let ok = await open(1);
    await sleep(1000);
    await mem("(b) Active, 1 chat connection", { connected: ok });
    ok = await open(10);
    await sleep(1000);
    await mem("(c) Active, 10 chat connections", { connected_new: ok });
    // Keep the 10 alive past t_silent while we add 40 more.
    for (const p of peers) p.send("keepalive");
    ok = await open(50);
    await sleep(1000);
    await mem("(c) Active, 50 chat connections", { connected_new: ok });
    for (const p of peers) p.send("/status");
    await sleep(2000);
    await mem("(c) 50 connections after one more line each");
    for (const p of peers) p.end();
    await Promise.all(peers.map((p) => p.done));
    await sleep(14_000); // t_idle/linger: Endpoint closed and dropped
    await mem("(d) all peers gone +14 s: Endpoint torn down");
    await mf.unsafeEvictDurableObject("iroh-do", "IrohEndpoint", { name: E, webSockets: "hibernate" }).catch((e) => log("evict:", e.message));
    await mem("(d) after forced eviction (wasm memory never shrinks)");
    // Second wave on the same isolate: does the high-water mark get reused?
    peers.length = 0;
    ok = await open(10);
    await sleep(1000);
    await mem("(d') new Active period, 10 connections, same isolate", { connected_new: ok });
    for (const p of peers) p.end();
    await Promise.all(peers.map((p) => p.done));
  }
  console.log(JSON.stringify({ mode, bundle: readFileSync(join(bundle, "BUNDLE_SHA256"), "utf8").trim(), samples }, null, 1));
} finally {
  for (const p of peers) p.child.kill();
  await mf.dispose();
}
