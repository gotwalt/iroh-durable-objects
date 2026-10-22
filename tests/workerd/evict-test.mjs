// Deterministic eviction test for the iroh-do Worker on Miniflare (workerd).
//
//   node tests/workerd/evict-test.mjs
//
// Uses the bundle in build/worker/, the peer at target/debug/iroh-do-chat
// (override with IROH_DO_DEMO) and, for case E, target/debug/iroh-do-echo-client
// (override with IROH_DO_ECHO). Miniflare is pinned in package.json to the
// version wrangler 4.130.0 bundles. Instead of waiting ~10 s for real
// hibernation, the test forces it with
// `Miniflare#unsafeEvictDurableObject(..., { webSockets: "hibernate" })`, and
// asserts on the object's persisted `wakes` and `endpoint_builds` counters,
// read from its SQLite file and from the peer's /status and notify/1 output.
//
// Case C is the regression test for docs/verification/hibernation-references.md:
// once the Endpoint has run and the object is back in Warm (peer closed its
// connection, T_linger elapsed, Endpoint closed and dropped), forced eviction
// must succeed at once. workerd's evict waits ~30 s for the object to have no
// active references (pending timers, fetches, in-flight events) and then
// fails, so a refusal means something still runs in Warm.
//
// EVICT_DIAG=1 wraps setTimeout/setInterval/fetch in the bundle and prints the
// outstanding ones, with wasm stacks, when an eviction is refused.
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { DatabaseSync } from "node:sqlite";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { Miniflare } from "miniflare";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "..", "..");
const bundle = join(repo, "build", "worker");
const demoBin = process.env.IROH_DO_DEMO ?? join(repo, "target", "debug", "iroh-do-chat");
const echoBin = process.env.IROH_DO_ECHO ?? join(repo, "target", "debug", "iroh-do-echo-client");
const port = Number(process.env.PORT ?? 8817);
const ADMIN_TOKEN = "evict-test-token";
const SCRIPT = "iroh-do";
const CLASS = "endpoint";
const T_SETTLE_MS = 12_500; // T_idle (10 s) + T_linger (2 s): long enough to reach Warm

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;
const check = (name, ok, detail = "") => {
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? `: ${detail}` : ""}`);
  if (!ok) failures++;
};

// Test-only diagnostics (EVICT_DIAG=1): track outstanding timers/fetches in the
// isolate and serve them at /__timers through the router's fetch handler.
const DIAG = process.env.EVICT_DIAG === "1";
const DIAG_PRELUDE = `
globalThis.__diag = { timers: new Map(), fetches: new Map(), seq: 0 };
{
  const D = globalThis.__diag, g = globalThis;
  const oST = g.setTimeout, oCT = g.clearTimeout, oSI = g.setInterval, oCI = g.clearInterval, oF = g.fetch;
  const stack = () => (new Error().stack || "").split("\\n").slice(2, 30).map((x) => x.trim()).join(" | ");
  g.setTimeout = function (cb, ms, ...a) {
    let id;
    id = oST.call(g, (...x) => { D.timers.delete(id); return cb(...x); }, ms, ...a);
    D.timers.set(id, { kind: "timeout", ms, at: Date.now(), stack: stack() });
    return id;
  };
  g.clearTimeout = (id) => { D.timers.delete(id); return oCT.call(g, id); };
  g.setInterval = function (cb, ms, ...a) { const id = oSI.call(g, cb, ms, ...a); D.timers.set(id, { kind: "interval", ms, at: Date.now(), stack: stack() }); return id; };
  g.clearInterval = (id) => { D.timers.delete(id); return oCI.call(g, id); };
  g.fetch = function (...a) {
    const k = ++D.seq;
    D.fetches.set(k, { url: String(a[0]?.url ?? a[0]), at: Date.now(), stack: stack() });
    const p = oF.apply(g, a);
    p.finally(() => D.fetches.delete(k)).catch(() => {});
    return p;
  };
}
`;
function instrument(js) {
  // worker-build 0.8.7's minified router entry; if it changes, diagnostics are skipped.
  const needle = /(\w+)\.prototype\.fetch=function\(e\)\{return (\w+)\.call\(this,e,this\.env,this\.ctx\)\}/;
  if (!needle.test(js)) {
    console.log("info EVICT_DIAG: router entry not found in index.js; diagnostics disabled");
    return js;
  }
  const hook = (_, cls, h) => `${cls}.prototype.fetch=function(e){if(new URL(e.url).pathname==='/__timers'){const D=globalThis.__diag;` +
    "return new Response(JSON.stringify({now:Date.now(),timers:[...D.timers.values()],fetches:[...D.fetches.values()]}))}" +
    `return ${h}.call(this,e,this.env,this.ctx)}`;
  return DIAG_PRELUDE + js.replace(needle, hook);
}
const indexJs = readFileSync(join(bundle, "index.js"), "utf8");

const persistDir = mkdtempSync(join(tmpdir(), "iroh-do-evict-"));
const text = (value) => ({ type: "text", value });
const mf = new Miniflare({
  host: "127.0.0.1",
  resourcePersistencePath: persistDir,
  port,
  workers: [
    {
      // Miniflare 5 config schema (mirrors what wrangler 4.130.0 generates).
      config: {
        name: SCRIPT,
        type: "worker",
        compatibilityDate: "2026-09-15",
        manifest: {
          mainModule: "index.js",
          modules: {
            "index.js": { type: "esm", contents: DIAG ? instrument(indexJs) : indexJs },
            "index_bg.wasm": { type: "wasm", contents: new Uint8Array(readFileSync(join(bundle, "index_bg.wasm"))) },
          },
        },
        env: {
          ENDPOINTS: { type: "durable-object", worker: SCRIPT, exportName: CLASS }, REGISTRY: { type: "durable-object", worker: SCRIPT, exportName: "registry" },
          ZONE_SUFFIX: text("localhost"),
          ADMIN_TOKEN: text(ADMIN_TOKEN),
          PKARR_RELAY: text(""), // no publishing from tests; the peer dials the relay URL directly
          BUNDLE_SHA256: text("evict-test"),
        },
        exports: { [CLASS]: { type: "durable-object", storage: "sqlite" }, registry: { type: "durable-object", storage: "sqlite" } },
      },
    },
  ],
});

/** Run `iroh-do-chat <args…> E` (driven over stdin); resolve lines matching a predicate. */
function startDemo(E, ...args) {
  const child = spawn(demoBin, ["--local", "--local-port", String(port), ...args, E], {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, RUST_LOG: process.env.RUST_LOG ?? "warn" },
  });
  const lines = [];
  const waiters = [];
  let buf = "";
  const onData = (d) => {
    buf += d.toString();
    let i;
    while ((i = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, i);
      buf = buf.slice(i + 1);
      lines.push(line);
      if (process.env.VERBOSE) console.log(`  | ${line}`);
      for (const w of [...waiters]) if (w.pred(line)) { waiters.splice(waiters.indexOf(w), 1); w.resolve(line); }
    }
  };
  child.stdout.on("data", onData);
  child.stderr.on("data", onData);
  return {
    child,
    send: (l) => child.stdin.write(l + "\n"),
    wait(pred, timeoutMs = 20_000) {
      const hit = lines.find(pred);
      if (hit) { lines.splice(lines.indexOf(hit), 1); return Promise.resolve(hit); }
      return new Promise((resolve, reject) => {
        const w = { pred, resolve };
        waiters.push(w);
        setTimeout(() => { const k = waiters.indexOf(w); if (k >= 0) { waiters.splice(k, 1); reject(new Error("timeout")); } }, timeoutMs);
      });
    },
  };
}
function z32decode(s) {
  const alphabet = "ybndrfg8ejkmcpqxot1uwisza345h769";
  let bits = 0, value = 0;
  const out = [];
  for (const ch of s) {
    value = (value << 5) | alphabet.indexOf(ch);
    bits += 5;
    if (bits >= 8) {
      out.push((value >>> (bits - 8)) & 0xff);
      bits -= 8;
    }
  }
  return out.slice(0, 32);
}
const wakesOf = (line) => Number(/wakes=(\d+)/.exec(line)?.[1] ?? NaN);
/** EVICT_DIAG: print the isolate's outstanding timers/fetches with their wasm frames. */
async function dumpOutstanding(when) {
  const r = await mf.dispatchFetch(`http://127.0.0.1:${port}/__timers`);
  if (!r.ok) return console.log(`info diag (${when}): /__timers -> ${r.status}`);
  const d = await r.json();
  console.log(`info diag (${when}): ${d.timers.length} timer(s), ${d.fetches.length} fetch(es) outstanding`);
  const frames = (st) => st.split(" | ").filter((f) => f.includes("wasm."))
    .map((f) => f.split("wasm.")[1].replace(/::h[0-9a-f]{16}.*/, "")).slice(0, 6).join(" <- ");
  for (const t of d.timers) console.log(`  ${t.kind} ${t.ms} ms (set ${d.now - t.at} ms ago): ${frames(t.stack)}`);
  for (const f of d.fetches) console.log(`  fetch ${f.url} (${d.now - f.at} ms): ${frames(f.stack)}`);
}

let ctl;
try {
  await mf.ready;
  const base = `http://127.0.0.1:${port}`;
  const ns = await mf.getDurableObjectNamespace("ENDPOINTS");

  // An upgrade to an unminted (valid z32) name must not write any storage.
  const unminted = "yr9ge793ppirfkkm4edbyu56mbnctp4i9xsf3468xsda9o8cuo6o";
  const probe = await mf.dispatchFetch(`http://${unminted}.localhost:${port}/relay`, {
    headers: { Upgrade: "websocket", "Sec-WebSocket-Protocol": "iroh-relay-v2, iroh-relay-v1" },
  });
  check("unminted upgrade rejected", probe.status === 404, `status ${probe.status}`);
  // workerd creates the object's .sqlite file on first access, even a read, so
  // assert that it has no tables. The file is read directly because Miniflare's
  // SQL introspection uses Durable Object RPC, which the worker-build shim does
  // not support.
  const unmintedId = ns.idFromName(unminted).toString();
  const file = readdirSync(persistDir, { recursive: true }).find((f) => f.endsWith(`${unmintedId}.sqlite`));
  let tables = [];
  if (file) {
    const db = new DatabaseSync(join(persistDir, file), { readOnly: true });
    tables = db.prepare("SELECT name FROM sqlite_master WHERE type = 'table'").all().map((r) => r.name)
      .filter((n) => !n.startsWith("_cf_") && !n.startsWith("sqlite_"));
    db.close();
  }
  check("unminted probe wrote no storage (no tables)", tables.length === 0, `file=${file ?? "none"} tables=${JSON.stringify(tables)}`);

  // Mint.
  const res = await mf.dispatchFetch(`http://admin.localhost:${port}/mint`, {
    method: "POST",
    headers: { Authorization: `Bearer ${ADMIN_TOKEN}` },
  });
  const minted = await res.json();
  const E = minted.id;
  check("mint", res.status === 200 && E?.length === 52, JSON.stringify(minted));
  check("relay_url", minted.relay_url === `http://${E}.localhost:${port}/`, minted.relay_url);
  const dbFile = () => {
    const id = ns.idFromName(E).toString();
    const f = readdirSync(persistDir, { recursive: true }).find((x) => x.endsWith(`${id}.sqlite`));
    return f && join(persistDir, f);
  };
  // Read the DO's persisted counters straight from its SQLite file.
  const counter = (name) => {
    const db = new DatabaseSync(dbFile(), { readOnly: true });
    try {
      return db.prepare("SELECT value FROM counters WHERE name = ?").get(name)?.value ?? 0;
    } finally {
      db.close();
    }
  };
  const evict = () => mf.unsafeEvictDurableObject(SCRIPT, CLASS, { name: E, webSockets: "hibernate" });

  // A. Forced hibernation of a socket accepted by the DO (handshake pending):
  //    socket, tags and attachment (with the challenge) must survive.
  const wakes0 = counter("wakes");
  const up = await mf.dispatchFetch(`http://${E}.localhost:${port}/relay`, {
    headers: { Upgrade: "websocket", "Sec-WebSocket-Protocol": "iroh-relay-v2, iroh-relay-v1" },
  });
  check("upgrade 101 + subprotocol", up.status === 101 && up.headers.get("sec-websocket-protocol") === "iroh-relay-v2");
  const ws = up.webSocket;
  const frames = [];
  let closed = null;
  ws.addEventListener("message", (e) => frames.push(new Uint8Array(e.data)));
  ws.addEventListener("close", (e) => (closed = e.code));
  ws.accept();
  await sleep(300);
  check("ServerChallenge first (type 0, 16 B)", frames[0]?.[0] === 0 && frames[0]?.length === 17, `${frames[0]?.length} B`);
  await evict();
  check("forced eviction succeeded (no Endpoint ever bound)", true);
  // A ClientAuth with a valid key but a wrong signature: only a session restored
  // from the attachment (awaiting auth, with its challenge) answers ServerDeniesAuth.
  const key = z32decode(E);
  const auth = new Uint8Array([1, ...key, 64, ...new Uint8Array(64)]);
  ws.send(auth);
  await sleep(500);
  check("constructor re-ran on the hibernated socket (wakes +1)", counter("wakes") === wakes0 + 1,
    `wakes ${wakes0} -> ${counter("wakes")}`);
  check("restored session denied the bad signature (ServerDeniesAuth, 1008)",
    frames.some((f) => f[0] === 3) && closed === 1008, `frames=${frames.map((f) => f[0])} close=${closed}`);

  // B. Stock peer, ctl/1 session. While the ctl stream is open the object
  //    stays Active (the idle close only applies with no open streams, design
  //    note 005), so no eviction is attempted here.
  ctl = startDemo(E, "ctl");
  ctl.send("hello");
  const echo = await ctl.wait((l) => l.includes("echo: hello"));
  check("stock client ctl/1 with log suffix", /echo: hello \(#\d+, total \d+, yours \d+\)/.test(echo), echo.trim());
  ctl.send("/status");
  const st = await ctl.wait((l) => l.includes('"state"'));
  check("/status reports Active with a bound Endpoint", st.includes('"state":"Active"'), st.trim());
  ctl.send("/quit");
  ctl.child.stdin.end();
  await new Promise((r) => (ctl.child.exitCode !== null ? r() : ctl.child.once("exit", r)));
  ctl = null;
  await sleep(4_000); // B's Endpoint lingers T_linger (2 s); C must build its own

  // C. Regression (see the header): the Endpoint ran, the peer closed its
  //    connection but kept its relay socket, and the object went Active → Warm.
  //    Forced eviction must succeed at once, and the dial-back alarm must wake
  //    the evicted object and reach the peer over the hibernated socket.
  const CALLBACK_S = 10;
  const builds0 = counter("endpoint_builds");
  ctl = startDemo(E, "listen", "--callback", String(CALLBACK_S), "--once", "--timeout", "45");
  await ctl.wait((l) => l.includes("ctl connection closed"));
  check("listen: Endpoint ran for this session", counter("endpoint_builds") === builds0 + 1,
    `endpoint_builds ${builds0} -> ${counter("endpoint_builds")}`);
  await sleep(4_000); // T_linger (2 s) + teardown margin → Warm
  if (DIAG) await dumpOutstanding("Warm");
  const wakesBefore = counter("wakes");
  const t0 = Date.now();
  let evictErr = null;
  try {
    await evict();
  } catch (e) {
    evictErr = e;
    if (DIAG) await dumpOutstanding("eviction refused");
  }
  const took = Date.now() - t0;
  check("forced eviction in Warm after the Endpoint ran", evictErr === null && took < 5_000,
    evictErr ? `${evictErr.message} (after ${took} ms)` : `${took} ms`);
  const notice = await ctl.wait((l) => l.includes("notify/1 from"), (CALLBACK_S + 20) * 1000);
  check("dial-back after forced eviction reaches the peer over the hibernated socket", notice.includes(E), notice.trim());
  check("dial-back came from a re-instantiated DO (wakes +1)", wakesOf(notice) === wakesBefore + 1,
    `wakes ${wakesBefore} -> ${wakesOf(notice)}`);

  // D. The room (design note 008) on workerd's SQLite: two chat clients whose home relay is
  //    the room. bo listens; ana joins and says a line; the room dials bo to deliver it. The
  //    object then returns to Warm and can be evicted at once.
  ctl?.child.kill();
  ctl = null;
  const stateDir = mkdtempSync(join(tmpdir(), "iroh-do-room-"));
  const bo = startDemo(E, "chat", "--nick", "bo", "--state-dir", join(stateDir, "bo"), "--listen-for", "25");
  await sleep(4_000); // bo joins
  const ana = startDemo(E, "chat", "--nick", "ana", "--state-dir", join(stateDir, "ana"), "--say", "hello bo");
  const said = await ana.wait((l) => l.includes('"t":"ok"') || l.includes('"t":"msg"') || l.includes('"t":"err"'), 20_000);
  check("room: ana's /say acknowledged", !said.includes('"t":"err"'), said.trim());
  const got = await bo.wait((l) => l.includes('"text":"hello bo"'), 20_000);
  check("room: delivered to bo", got.includes('"nick":"ana"') && !got.includes('"hist":true'), got.trim());
  await new Promise((r) => (bo.child.exitCode !== null ? r() : bo.child.once("exit", r)));
  await sleep(T_SETTLE_MS);
  const t1 = Date.now();
  let roomEvictErr = null;
  try {
    await evict();
  } catch (e) {
    // Both clients have exited, so the object may already have left memory on its own.
    if (!/not currently running/.test(e?.message)) roomEvictErr = e;
  }
  check("room: evictable after deliveries (or already unloaded)", roomEvictErr === null && Date.now() - t1 < 5_000,
    roomEvictErr ? roomEvictErr.message : `${Date.now() - t1} ms`);
  rmSync(stateDir, { recursive: true, force: true });

  // E. An endpoint minted with the echo app (design note 011) runs iroh's echo example
  //    handler, unchanged, for a stock peer; once the peer is gone the object holds nothing
  //    that keeps it in memory, and the next round trip runs on a new object.
  const em = await mf.dispatchFetch(`http://admin.localhost:${port}/mint`, {
    method: "POST",
    headers: { Authorization: `Bearer ${ADMIN_TOKEN}` },
    body: JSON.stringify({ app: "echo" }),
  });
  const echoMinted = await em.json();
  check("echo: mint", em.status === 200 && echoMinted.app === "echo", JSON.stringify(echoMinted));
  const EE = echoMinted.id;
  const echoOnce = (text) => new Promise((resolve) => {
    const child = spawn(echoBin, [EE, text, "--local", String(port)], { stdio: ["ignore", "pipe", "pipe"] });
    let out = "";
    child.stdout.on("data", (d) => (out += d));
    child.stderr.on("data", (d) => process.env.VERBOSE && process.stdout.write(`  | ${d}`));
    child.on("exit", (code) => resolve({ code, out: out.trim() }));
  });
  const first = await echoOnce("echo before eviction");
  check("echo: round trip", first.code === 0 && first.out === "echo before eviction", JSON.stringify(first));
  await sleep(4_000); // T_linger (2 s) + teardown margin
  const t2 = Date.now();
  let echoEvictErr = null;
  try {
    await mf.unsafeEvictDurableObject(SCRIPT, CLASS, { name: EE, webSockets: "hibernate" });
  } catch (e) {
    // The peer has exited, so the object may already have left memory on its own.
    if (!/not currently running/.test(e?.message)) echoEvictErr = e;
  }
  check("echo: evictable after the handler ran", echoEvictErr === null && Date.now() - t2 < 5_000,
    echoEvictErr ? echoEvictErr.message : `${Date.now() - t2} ms`);
  const second = await echoOnce("echo after eviction");
  check("echo: round trip on a new object", second.code === 0 && second.out === "echo after eviction", JSON.stringify(second));
} catch (e) {
  console.log(`FAIL exception: ${e?.stack ?? e}`);
  failures++;
} finally {
  ctl?.child.stdin.end();
  ctl?.child.kill();
  await mf.dispose();
  rmSync(persistDir, { recursive: true, force: true });
}
console.log(failures ? `${failures} FAILURE(S)` : "ALL PASS");
process.exit(failures ? 1 : 0);
