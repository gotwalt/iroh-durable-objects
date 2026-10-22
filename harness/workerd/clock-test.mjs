// Regression test for the timer spin under Cloudflare's production clock
// (docs/measurements/cost-profile.md, "CPU diagnosis"; fix in crates/iroh-do/src/glue/timers.rs).
//
//   node harness/workerd/clock-test.mjs
//
// Local workerd's clock is real time; production's is not. In production a timer callback
// sees exactly its scheduled time `now + ms`, and the clock catches up with real time only on
// the next I/O. The test runs build/worker/ in Miniflare from an in-memory copy (nothing on
// disk changes) with a prelude that emulates this:
//   - Date.now / performance.now return an integer-ms value frozen per event;
//   - every DO and router entry re-syncs it to real time;
//   - a setTimeout callback sets it to max(clock, when).
// It counts timers that fire with a zero delay and workerd's process CPU over a fixed peer
// workload (a ping, then a quiet ctl session, each followed by teardown).
//
// Two runs: the bundle as built, which must stay at the real-clock baseline, and a control
// run in which the prelude makes `globalThis.setTimeout` read-only so the bundle cannot install
// its fix, which must spin. The control proves the test detects the bug; SKIP_CONTROL=1 skips it.
//
// Uses the peer at target/debug/iroh-do-chat (override with IROH_DO_DEMO).
import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { Miniflare } from "miniflare";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "..", "..");
const bundle = join(repo, "build", "worker");
const demoBin = process.env.IROH_DO_DEMO ?? join(repo, "target", "debug", "iroh-do-chat");
const port = Number(process.env.PORT ?? 8837);
const ADMIN_TOKEN = "clock-test-token";
// Limits for the fixed run. The real-clock baseline of this workload is about 0.1 s of
// workerd CPU and no zero-delay timer firings; the unfixed bundle fires tens of thousands.
const MAX_ZERO_FIRED = 50;
const MAX_CPU_S = 1.0;
const MIN_ZERO_FIRED_CONTROL = 1000;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;
const check = (name, ok, detail = "") => {
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? `: ${detail}` : ""}`);
  if (!ok) failures++;
};

const prelude = (readOnlySetTimeout) => `
{
  const g = globalThis, P = g.performance, realNow = Date.now.bind(Date);
  const C = (g.__clk = { v: Math.floor(realNow()), zero: 0, zeroFired: 0 });
  const now = () => C.v;
  g.__clkSync = () => { C.v = Math.max(C.v, Math.floor(realNow())); };
  Date.now = now;
  Object.defineProperty(P, "now", { value: now, configurable: true, writable: true });
  const oST = g.setTimeout;
  const st = function (cb, ms, ...a) {
    const d = Math.max(0, Math.floor(Number(ms) || 0));
    if (d === 0) C.zero++;
    const when = now() + d;
    return oST.call(g, (...x) => { if (d === 0) C.zeroFired++; C.v = Math.max(C.v, when); return cb(...a, ...x); }, d);
  };
  Object.defineProperty(g, "setTimeout", { value: st, configurable: true, writable: ${!readOnlySetTimeout} });
}
`;

function instrument(js, readOnlySetTimeout) {
  // worker-build's minified entry points; the names change between builds.
  const entry = /function (\w+)\(\)\{(\w+)\.criticalError&&\(/;
  const route = /(\w+)\.prototype\.fetch=function\(e\)\{return (\w+)\.call\(this,e,this\.env,this\.ctx\)\}/;
  if (!entry.test(js) || !route.test(js)) throw new Error("hook points not found in index.js (worker-build changed?)");
  return prelude(readOnlySetTimeout) + js
    .replace(entry, (_, f, m) => `function ${f}(){globalThis.__clkSync();${m}.criticalError&&(`)
    .replace(route, (_, cls, h) => `${cls}.prototype.fetch=function(e){if(new URL(e.url).pathname==='/__clk')` +
      `return new Response(JSON.stringify(globalThis.__clk));return ${h}.call(this,e,this.env,this.ctx)}`);
}

// CPU seconds of every workerd process.
function workerdCpu() {
  const out = execFileSync("ps", ["-A", "-o", "time=,command="], { encoding: "utf8" });
  let s = 0;
  for (const line of out.split("\n")) {
    if (!/workerd/.test(line) || /ps -A/.test(line)) continue;
    const parts = line.trim().split(/\s+/)[0].replace("-", ":").split(":").map(Number);
    s += parts.reduce((acc, p) => acc * 60 + p, 0);
  }
  return s;
}

function run(E, args, input = []) {
  return new Promise((resolve) => {
    const child = spawn(demoBin, ["--local", "--local-port", String(port), ...args, E], {
      stdio: ["pipe", "pipe", "pipe"], env: { ...process.env, RUST_LOG: "warn" },
    });
    let out = "";
    child.stdout.on("data", (d) => (out += d));
    child.stderr.on("data", (d) => (out += d));
    (async () => { for (const [delay, line] of input) { await sleep(delay); child.stdin.write(line + "\n"); } child.stdin.end(); })();
    child.on("exit", (code) => resolve({ code, out }));
  });
}

async function measure(readOnlySetTimeout) {
  const persist = mkdtempSync(join(tmpdir(), "iroh-do-clock-"));
  const text = (value) => ({ type: "text", value });
  const js = instrument(readFileSync(join(bundle, "index.js"), "utf8"), readOnlySetTimeout);
  const mf = new Miniflare({
    host: "127.0.0.1", port, resourcePersistencePath: persist,
    workers: [{ config: {
      name: "iroh-do", type: "worker", compatibilityDate: "2026-09-15",
      manifest: { mainModule: "index.js", modules: {
        "index.js": { type: "esm", contents: js },
        "index_bg.wasm": { type: "wasm", contents: new Uint8Array(readFileSync(join(bundle, "index_bg.wasm"))) } } },
      env: { ENDPOINTS: { type: "durable-object", worker: "iroh-do", exportName: "endpoint" }, REGISTRY: { type: "durable-object", worker: "iroh-do", exportName: "registry" },
        ZONE_SUFFIX: text("localhost"), ADMIN_TOKEN: text(ADMIN_TOKEN), PKARR_RELAY: text(""), BUNDLE_SHA256: text("clock-test") },
      exports: { endpoint: { type: "durable-object", storage: "sqlite" }, registry: { type: "durable-object", storage: "sqlite" } } } }],
  });
  const clk = async () => (await (await mf.dispatchFetch(`http://127.0.0.1:${port}/__clk`)).json());
  try {
    await mf.ready;
    const r = await mf.dispatchFetch(`http://admin.localhost:${port}/mint`, { method: "POST", headers: { Authorization: `Bearer ${ADMIN_TOKEN}` } });
    const E = (await r.json()).id;
    await sleep(1000);
    const c0 = workerdCpu(), k0 = await clk();
    const ping = await run(E, ["ping"]);
    await sleep(14_000); // close, draining, t_linger, teardown
    const ctl = await run(E, ["ctl"], [[500, "hello"], [8_000, "/quit"]]);
    await sleep(14_000);
    const c1 = workerdCpu(), k1 = await clk();
    return { codes: [ping.code, ctl.code], cpu_s: +(c1 - c0).toFixed(2), zero: k1.zero - k0.zero, zero_fired: k1.zeroFired - k0.zeroFired };
  } finally {
    await mf.dispose();
    rmSync(persist, { recursive: true, force: true });
  }
}

try {
  const fixed = await measure(false);
  console.log(`info with the fix: ${JSON.stringify(fixed)}`);
  check("peers succeeded", fixed.codes.every((c) => c === 0), `exit codes ${fixed.codes}`);
  check(`zero-delay timer firings <= ${MAX_ZERO_FIRED}`, fixed.zero_fired <= MAX_ZERO_FIRED, String(fixed.zero_fired));
  check(`workerd CPU <= ${MAX_CPU_S} s`, fixed.cpu_s <= MAX_CPU_S, `${fixed.cpu_s} s`);
  if (process.env.SKIP_CONTROL !== "1") {
    const control = await measure(true);
    console.log(`info control, fix disabled: ${JSON.stringify(control)}`);
    check(`control spins (>= ${MIN_ZERO_FIRED_CONTROL} zero-delay firings)`, control.zero_fired >= MIN_ZERO_FIRED_CONTROL, String(control.zero_fired));
  }
} catch (e) {
  console.log(`FAIL exception: ${e?.stack ?? e}`);
  failures++;
}
console.log(failures ? `${failures} FAILURE(S)` : "ALL PASS");
process.exit(failures ? 1 : 0);
