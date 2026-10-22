// Forced-hibernation test of the TypeScript host on Miniflare (workerd).
//
//   npm run bundle && node test/evict-test.mjs
//
// Loads the bundle wrangler wrote to dist/ (`wrangler deploy --dry-run --outdir dist`),
// mints an echo endpoint over Workers RPC, and drives it with the stock peer in peer/
// (target/debug/iroh-do-ts-spike-peer, override with PEER). As tests/workerd/evict-test.mjs
// does, hibernation is forced with
// `Miniflare#unsafeEvictDurableObject(..., { webSockets: "hibernate" })`; workerd refuses
// the eviction (after ~30 s) while the object has pending timers, fetches or events, so a
// prompt success shows nothing runs in Warm.
//
// T_SILENT_MS is shortened to 20 s to keep the run short; T_IDLE is the default 10 s.
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { Miniflare } from "miniflare";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
const repo = join(root, "..", "..");
const dist = join(root, "dist");
const peerBin = process.env.PEER ?? join(process.env.CARGO_TARGET_DIR ?? join(repo, "target"), "debug", "iroh-do-ts-spike-peer");
const port = Number(process.env.PORT ?? 8818);
const SCRIPT = "iroh-do-ts";
const CLASS = "Echo";
const T_SILENT_MS = 20_000;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;
const check = (name, ok, detail = "") => {
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? `: ${detail}` : ""}`);
  if (!ok) failures++;
};

const wasmName = readdirSync(dist).find((f) => f.endsWith(".wasm"));
const persistDir = mkdtempSync(join(tmpdir(), "iroh-do-ts-evict-"));
const text = (value) => ({ type: "text", value });
const mf = new Miniflare({
  host: "127.0.0.1",
  port,
  resourcePersistencePath: persistDir,
  workers: [{
    config: {
      name: SCRIPT,
      type: "worker",
      compatibilityDate: "2026-09-15",
      manifest: {
        mainModule: "worker.js",
        modules: {
          "worker.js": { type: "esm", contents: readFileSync(join(dist, "worker.js"), "utf8") },
          [wasmName]: { type: "wasm", contents: new Uint8Array(readFileSync(join(dist, wasmName))) },
        },
      },
      env: {
        ENDPOINTS: { type: "durable-object", worker: SCRIPT, exportName: CLASS },
        ZONE_SUFFIX: text("localhost"),
        BUNDLE_SHA256: text("evict-test"),
        LOG_LEVEL: text(process.env.LOG_LEVEL ?? "info,iroh=warn,noq=warn,netwatch=error,iroh_do=info"),
        T_SILENT_MS: text(String(T_SILENT_MS)),
      },
      exports: { [CLASS]: { type: "durable-object", storage: "sqlite" } },
    },
  }],
});

function startPeer(E) {
  const child = spawn(join(repo, "scripts", "macos-sign-run.sh"), [peerBin, E, "--local", String(port)], { stdio: ["pipe", "pipe", "pipe"] });
  const lines = [];
  const waiters = [];
  let buf = "";
  child.stdout.on("data", (d) => {
    buf += d.toString();
    let i;
    while ((i = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, i);
      buf = buf.slice(i + 1);
      if (process.env.VERBOSE) console.log(`  | ${line}`);
      const w = waiters.shift();
      if (w) w(line); else lines.push(line);
    }
  });
  child.stderr.on("data", (d) => process.env.VERBOSE && process.stdout.write(`  ! ${d}`));
  return {
    child,
    send: (l) => child.stdin.write(l + "\n"),
    next: (timeoutMs = 90_000) => lines.length
      ? Promise.resolve(lines.shift())
      : new Promise((resolve, reject) => {
          waiters.push(resolve);
          setTimeout(() => reject(new Error("peer timeout")), timeoutMs);
        }),
  };
}

let peer;
try {
  await mf.ready;
  const base = `http://admin.localhost:${port}`;
  const status = async (E) => (await mf.dispatchFetch(`${base}/status/${E}`)).json();

  // Mint over Workers RPC (router -> stub.mint).
  const res = await mf.dispatchFetch(`${base}/mint`, { method: "POST" });
  const minted = await res.json();
  const E = minted.id;
  check("mint over RPC", res.status === 200 && E?.length === 52, JSON.stringify(minted));
  const evict = async () => {
    const t0 = Date.now();
    try {
      await mf.unsafeEvictDurableObject(SCRIPT, CLASS, { name: E, webSockets: "hibernate" });
      return { ok: true, ms: Date.now() - t0 };
    } catch (e) {
      return { ok: false, ms: Date.now() - t0, error: e.message };
    }
  };

  // A. A socket accepted by the TS class, hibernated mid-handshake: the woken object must
  //    rebuild the session (with its challenge) from the attachment and deny a bad signature.
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
  check("ServerChallenge first (type 0, 17 B)", frames[0]?.[0] === 0 && frames[0]?.length === 17, `${frames[0]?.length} B`);
  const wakes0 = (await status(E)).wakes;
  const e0 = await evict();
  check("evict with a pending handshake", e0.ok, `${e0.ms} ms ${e0.error ?? ""}`);
  const z = "ybndrfg8ejkmcpqxot1uwisza345h769";
  let bits = 0, value = 0; const idBytes = [];
  for (const ch of E) { value = (value << 5) | z.indexOf(ch); bits += 5; if (bits >= 8) { idBytes.push((value >>> (bits - 8)) & 0xff); bits -= 8; } }
  ws.send(new Uint8Array([1, ...idBytes.slice(0, 32), 64, ...new Uint8Array(64)]));
  await sleep(500);
  check("restored session denied the bad signature (ServerDeniesAuth, 1008)",
    frames.some((f) => f[0] === 3) && closed === 1008, `frames=${frames.map((f) => f[0])} close=${closed}`);

  // B. Stock peer: one relay socket across an echo, a forced hibernation and a second echo.
  peer = startPeer(E);
  check("peer ready", (await peer.next()) === "ready");
  peer.send("echo one");
  const one = await peer.next();
  check("echo before eviction", one.startsWith("echo: one"), one);
  const s1 = await status(E);
  check("one relay socket", s1.sockets.length === 1, JSON.stringify(s1.sockets));
  await sleep(4_000); // t_linger (2 s) + teardown margin: Active -> Warm
  const s2 = await status(E);
  check("Warm with no node tasks after the endpoint ran", s2.state === "Warm" && s2.tasks === 0,
    `state=${s2.state} tasks=${s2.tasks} endpoint_builds=${s2.endpoint_builds}`);
  const e1 = await evict();
  check("forced eviction in Warm succeeds promptly", e1.ok && e1.ms < 5_000, `${e1.ms} ms ${e1.error ?? ""}`);
  const s3 = await status(E);
  check("woken object rebuilt the node from the attachment (wakes +1, same socket id)",
    s3.wakes === s2.wakes + 1 && JSON.stringify(s3.sockets) === JSON.stringify(s2.sockets) && s3.state === "Warm",
    `wakes ${s2.wakes} -> ${s3.wakes}, sockets ${JSON.stringify(s2.sockets)} -> ${JSON.stringify(s3.sockets)}, state ${s3.state}`);
  peer.send("echo two");
  const two = await peer.next();
  check("echo after eviction over the hibernated socket", two.startsWith("echo: two"), two);
  const s4 = await status(E);
  check("same socket, new endpoint build", JSON.stringify(s4.sockets) === JSON.stringify(s2.sockets) && s4.endpoint_builds === s2.endpoint_builds + 1,
    `sockets ${JSON.stringify(s4.sockets)}, endpoint_builds ${s2.endpoint_builds} -> ${s4.endpoint_builds}`);

  // C. Idle close without a guard API: a connection with no streams is closed by the
  //    object after t_idle (10 s) with reason "idle".
  peer.send("idle");
  const idle = await peer.next();
  const idleMs = Number(/after_ms=(\d+)/.exec(idle)?.[1]);
  check("idle connection closed by the object with reason idle after ~10 s",
    /idle/.test(idle) && idleMs >= 9_500 && idleMs < 13_000, idle);

  // D. A stream the peer opened and never finished counts as open: the object waits
  //    t_silent (20 s here) and closes with reason "silent".
  peer.send("silent");
  const silent = await peer.next();
  const silentMs = Number(/after_ms=(\d+)/.exec(silent)?.[1]);
  check(`connection with an open stream closed with reason silent after ~${T_SILENT_MS / 1000} s`,
    /silent/.test(silent) && silentMs >= T_SILENT_MS - 500 && silentMs < T_SILENT_MS + 3_000, silent);

  await sleep(4_000);
  const s5 = await status(E);
  check("Warm again, no tasks", s5.state === "Warm" && s5.tasks === 0, `state=${s5.state} tasks=${s5.tasks}`);
  const e2 = await evict();
  check("forced eviction in Warm after idle and silent closes", e2.ok && e2.ms < 5_000, `${e2.ms} ms ${e2.error ?? ""}`);
  peer.send("echo three");
  const three = await peer.next();
  check("echo after the second eviction", three.startsWith("echo: three"), three);
  const s6 = await status(E);
  console.log(`info final status: ${JSON.stringify(s6)}`);
} catch (e) {
  console.log(`FAIL exception: ${e?.stack ?? e}`);
  failures++;
} finally {
  peer?.child.stdin.end();
  peer?.child.kill();
  await mf.dispose();
  rmSync(persistDir, { recursive: true, force: true });
}
console.log(failures ? `${failures} FAILURE(S)` : "ALL PASS");
process.exit(failures ? 1 : 0);
