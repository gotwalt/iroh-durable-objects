// Reproduces the production-only CPU spin locally (docs/measurements/cost-profile.md, "CPU diagnosis").
//
//   CLOCK=real|frozen  FIX=0|1  node docs/measurements/clock-emulation.mjs > docs/measurements/raw/clock-<clock>-fix<FIX>.json
//
// Local workerd's clock is real time (V12); production's is not. workerd computes a
// timeout's deadline as `when = now() + msDelay` and, while a fired timer's callback
// runs, clamps the visible clock to that `when` (io-context.c++ setTimeoutImpl /
// getClockClamp). Time only catches up with real time on the next I/O delivery.
// CLOCK=frozen emulates that in a test-only in-memory copy of build/worker/index.js:
//   - Date.now / performance.now return an integer-ms value frozen per event;
//   - every DO/router entry (the bundle's per-call hook) re-syncs it to real time;
//   - a setTimeout callback sets it to max(clock, when).
// FIX=1 adds the proposed workaround: every setTimeout delay gets +1 ms.
// STACKS=1 samples the wasm frames of every 500th zero-delay setTimeout (reported as `stacks`).
// The probe reports workerd's process CPU time over a fixed peer workload and counts
// setTimeout(0) calls made by the bundle.
import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "..", "..");
const { Miniflare } = await import(join(repo, "harness/workerd/node_modules/miniflare/dist/src/index.js"));
const bundle = join(repo, "build", "worker");
const demoBin = process.env.IROH_DO_DEMO ?? join(repo, "target", "release", "iroh-do-demo");
const port = Number(process.env.PORT ?? 8837);
const CLOCK = process.env.CLOCK ?? "frozen";
const FIX = process.env.FIX === "1";
const ADMIN_TOKEN = "clock-emulation-token";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const log = (...a) => console.error(...a);

const PRELUDE = `
{
  const g = globalThis, P = g.performance, realNow = Date.now.bind(Date);
  const C = (g.__clk = { frozen: ${CLOCK === "frozen"}, fix: ${FIX}, v: Math.floor(realNow()), st: 0, st0: 0, st0fired: 0 });
  const now = () => (C.frozen ? C.v : realNow());
  g.__clkSync = () => { C.v = Math.max(C.v, Math.floor(realNow())); };
  if (C.frozen) {
    Date.now = now;
    Object.defineProperty(P, "now", { value: now, configurable: true, writable: true });
  }
  const oST = g.setTimeout;
  g.setTimeout = function (cb, ms, ...a) {
    let d = Math.max(0, Math.floor(Number(ms) || 0));
    C.st++; if (d === 0) C.st0++;
    if (d === 0 && ${process.env.STACKS === "1"} && C.st0 % 500 === 1) {
      const fr = (new Error().stack || "").split("\\n").filter((f) => f.includes("wasm")).slice(0, 8)
        .map((f) => f.trim().replace(/^at /, "").replace(/ \\(.*$/, "").replace(/::h[0-9a-f]{16}$/, "")).join(" <- ");
      (C.stacks ??= {})[fr] = (C.stacks[fr] ?? 0) + 1;
    }
    if (C.fix) d += 1;
    const when = now() + d, zero = d === 0;
    return oST.call(g, (...x) => { if (zero) C.st0fired++; C.v = Math.max(C.v, when); return cb(...a, ...x); }, d);
  };
}
`;
let js = readFileSync(join(bundle, "index.js"), "utf8");
const entry = "function Y(){m.criticalError&&(";
const route = "D.prototype.fetch=function(e){return ie.call(this,e,this.env,this.ctx)}";
if (!js.includes(entry) || !js.includes(route)) throw new Error("hook points not found in index.js (worker-build changed?)");
js = PRELUDE + js
  .replace(entry, "function Y(){globalThis.__clkSync();m.criticalError&&(")
  .replace(route, "D.prototype.fetch=function(e){if(new URL(e.url).pathname==='/__clk'){" +
    "return new Response(JSON.stringify(globalThis.__clk))}return ie.call(this,e,this.env,this.ctx)}");

const text = (value) => ({ type: "text", value });
const mf = new Miniflare({
  host: "127.0.0.1", port,
  resourcePersistencePath: mkdtempSync(join(tmpdir(), "iroh-do-clock-")),
  workers: [{ config: {
    name: "iroh-do", type: "worker", compatibilityDate: "2026-09-15",
    manifest: { mainModule: "index.js", modules: {
      "index.js": { type: "esm", contents: js },
      "index_bg.wasm": { type: "wasm", contents: new Uint8Array(readFileSync(join(bundle, "index_bg.wasm"))) } } },
    env: { ENDPOINTS: { type: "durable-object", worker: "iroh-do", exportName: "IrohEndpoint" },
      ZONE_SUFFIX: text("localhost"), ADMIN_TOKEN: text(ADMIN_TOKEN), PKARR_RELAY: text(""), BUNDLE_SHA256: text("clock-emulation") },
    exports: { IrohEndpoint: { type: "durable-object", storage: "sqlite" } } } }],
});

// CPU seconds of every workerd process (Miniflare spawns one).
function workerdCpu() {
  const out = execFileSync("ps", ["-A", "-o", "time=,command="], { encoding: "utf8" });
  let s = 0;
  for (const line of out.split("\n")) {
    if (!/workerd/.test(line) || /ps -A/.test(line)) continue;
    const t = line.trim().split(/\s+/)[0]; // [[dd-]hh:]mm:ss.cc
    const parts = t.replace("-", ":").split(":").map(Number);
    s += parts.reduce((acc, p) => acc * 60 + p, 0);
  }
  return s;
}
function run(E, args, input = []) {
  return new Promise((resolve) => {
    const child = spawn(demoBin, ["--local", "--local-port", String(port), ...args, E], { stdio: ["pipe", "pipe", "pipe"] });
    let out = "";
    child.stdout.on("data", (d) => (out += d));
    child.stderr.on("data", (d) => (out += d));
    (async () => { for (const [delay, line] of input) { await sleep(delay); child.stdin.write(line + "\n"); } child.stdin.end(); })();
    child.on("exit", (code) => resolve({ code, out }));
  });
}
const clk = async () => (await (await mf.dispatchFetch(`http://127.0.0.1:${port}/__clk`)).json());

const phases = [];
async function phase(name, fn) {
  const c0 = workerdCpu(), k0 = await clk(), t0 = Date.now();
  const res = await fn();
  const c1 = workerdCpu(), k1 = await clk(), t1 = Date.now();
  const p = { name, wall_s: (t1 - t0) / 1000, workerd_cpu_s: +(c1 - c0).toFixed(2),
    setTimeout_calls: k1.st - k0.st, setTimeout0_calls: k1.st0 - k0.st0, setTimeout0_fired: k1.st0fired - k0.st0fired, ...res };
  phases.push(p);
  log(JSON.stringify(p));
}
try {
  await mf.ready;
  const r = await mf.dispatchFetch(`http://admin.localhost:${port}/mint`, { method: "POST", headers: { Authorization: `Bearer ${ADMIN_TOKEN}` } });
  const E = (await r.json()).id;
  await sleep(2000);
  await phase("3 pings, each followed by 15 s idle (close, T_linger, teardown)", async () => {
    const ok = [];
    for (let i = 0; i < 3; i++) { ok.push((await run(E, ["ping"])).code); await sleep(15_000); }
    return { exit_codes: ok };
  });
  await phase("chat: 20 lines at 0.3 s, then EOF and 15 s idle", async () => {
    const input = [[1000, "first"]];
    for (let i = 0; i < 19; i++) input.push([300, `line ${i}`]);
    const { code } = await run(E, ["chat"], input);
    await sleep(15_000);
    return { exit_code: code };
  });
  await phase("quiet chat: 1 line, hold 40 s (keep-alives only), EOF, 15 s idle", async () => {
    const { code } = await run(E, ["chat"], [[1000, "hello"], [40_000, "bye"]]);
    await sleep(15_000);
    return { exit_code: code };
  });
  await phase("bench --mb 4, then 15 s idle", async () => {
    const { code, out } = await run(E, ["bench", "--mb", "4"]);
    await sleep(15_000);
    return { exit_code: code, result: out.trim().split("\n").filter((l) => l.startsWith("echoed")).join("") };
  });
  const stacks = (await clk()).stacks;
  console.log(JSON.stringify({ clock: CLOCK, fix: FIX, stacks, bundle: readFileSync(join(bundle, "BUNDLE_SHA256"), "utf8").trim(), phases }, null, 1));
} finally {
  await mf.dispose();
}
