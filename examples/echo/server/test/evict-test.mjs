// Forced hibernation of the echo app on Miniflare (workerd), and the eviction marker.
//
//   npm run build && cargo build -p iroh-do-test-peer && node test/evict-test.mjs
//
// Loads dist/ (manifest.json), mints an endpoint over Workers RPC, and drives it with the
// stock test peer. As tests/workerd/evict-test.mjs does, hibernation is forced with
// `Miniflare#unsafeEvictDurableObject(..., { webSockets: "hibernate" })`; workerd refuses
// the eviction while the object has pending timers, fetches or events, so a prompt success
// shows nothing runs in Warm. Finally an eviction with the endpoint bound is simulated by
// restarting workerd on the same storage, which must log `evicted_active`.
//
// T_SILENT_MS is shortened to 20 s to keep the run short; t_idle is the default 10 s.
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterMs, check, fail, finish, sleep, startPeer, startWorker } from "iroh-do/testing";

const dist = join(fileURLToPath(import.meta.url), "..", "..", "dist");
const port = Number(process.env.PORT ?? 8818);
const SCRIPT = "iroh-ts";
const CLASS = "Echo";
const T_SILENT_MS = 20_000;
const opts = { dist, script: SCRIPT, className: CLASS, port, vars: { T_SILENT_MS } };

let w = startWorker(opts);
let peer;
try {
  await w.mf.ready;
  const minted = await w.mint();
  const E = minted.id;
  check("mint over RPC", E?.length === 52 && minted.published === false, JSON.stringify(minted));
  const evict = async () => {
    const t0 = Date.now();
    try {
      await w.mf.unsafeEvictDurableObject(SCRIPT, CLASS, { name: E, webSockets: "hibernate" });
      return { ok: true, ms: Date.now() - t0 };
    } catch (e) {
      return { ok: false, ms: Date.now() - t0, error: e.message };
    }
  };

  // A. A socket hibernated mid-handshake: the woken object rebuilds the session (with its
  //    challenge) from the attachment and denies a bad signature.
  const up = await w.mf.dispatchFetch(`http://${E}.localhost:${port}/relay`, {
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
  peer = startPeer(E, port);
  check("peer ready", (await peer.next()).startsWith("ready"));
  peer.send("echo one");
  const one = await peer.next();
  check("echo before eviction", one.startsWith("echo: one"), one);
  const s1 = await w.status(E);
  check("one relay socket", s1.sockets.length === 1, JSON.stringify(s1.sockets));
  await sleep(4_000); // t_linger (2 s) + teardown margin: Active -> Warm
  const s2 = await w.status(E);
  check("Warm with no node tasks after the endpoint ran", s2.state === "Warm" && s2.tasks === 0,
    `state=${s2.state} tasks=${s2.tasks} endpoint_builds=${s2.endpoint_builds}`);
  const e1 = await evict();
  check("forced eviction in Warm succeeds promptly", e1.ok && e1.ms < 5_000, `${e1.ms} ms ${e1.error ?? ""}`);
  const s3 = await w.status(E);
  check("woken object rebuilt the node from the attachment (wakes +1, same socket id)",
    s3.wakes === s2.wakes + 1 && JSON.stringify(s3.sockets) === JSON.stringify(s2.sockets) && s3.state === "Warm",
    `wakes ${s2.wakes} -> ${s3.wakes}, sockets ${JSON.stringify(s2.sockets)} -> ${JSON.stringify(s3.sockets)}, state ${s3.state}`);
  peer.send("echo two");
  const two = await peer.next();
  check("echo after eviction over the hibernated socket", two.startsWith("echo: two"), two);
  const s4 = await w.status(E);
  check("same socket, new endpoint build", JSON.stringify(s4.sockets) === JSON.stringify(s2.sockets) && s4.endpoint_builds === s2.endpoint_builds + 1,
    `sockets ${JSON.stringify(s4.sockets)}, endpoint_builds ${s2.endpoint_builds} -> ${s4.endpoint_builds}`);

  // C. Idle close without a guard API: a connection with no streams is closed by the
  //    object after t_idle (10 s) with reason "idle".
  peer.send("idle");
  const idle = await peer.next();
  check("idle connection closed by the object with reason idle after ~10 s",
    /idle/.test(idle) && afterMs(idle) >= 9_500 && afterMs(idle) < 13_000, idle);

  // D. A stream the peer opened and never finished counts as open: the object waits
  //    t_silent (20 s here) and closes with reason "silent".
  peer.send("silent");
  const silent = await peer.next();
  check(`connection with an open stream closed with reason silent after ~${T_SILENT_MS / 1000} s`,
    /silent/.test(silent) && afterMs(silent) >= T_SILENT_MS - 500 && afterMs(silent) < T_SILENT_MS + 3_000, silent);

  await sleep(4_000);
  const s5 = await w.status(E);
  check("Warm again, no tasks, no open streams", s5.state === "Warm" && s5.tasks === 0 && s5.open_streams === 0,
    `state=${s5.state} tasks=${s5.tasks} open_streams=${s5.open_streams}`);
  const e2 = await evict();
  check("forced eviction in Warm after idle and silent closes", e2.ok && e2.ms < 5_000, `${e2.ms} ms ${e2.error ?? ""}`);
  peer.send("echo three");
  const three = await peer.next();
  check("echo after the second eviction", three.startsWith("echo: three"), three);
  check("no evicted_active after hibernations in Warm", w.events("evicted_active").length === 0,
    w.events("evicted_active").join(" | "));

  // E. Eviction with the endpoint bound. workerd will not evict an object with pending
  //    timers, so the eviction is simulated by restarting workerd on the same storage while
  //    a connection is open. The next construction finds the bind marker.
  peer.send("idle");
  await sleep(1_000);
  const s6 = await w.status(E);
  check("Active with a connection open", s6.state === "Active" && s6.connections === 1, `state=${s6.state} connections=${s6.connections}`);
  peer.stop();
  peer = null;
  await w.dispose(true);
  const restarted = startWorker({ ...opts, persist: w.persistDir });
  w = restarted;
  await w.mf.ready;
  const s7 = await w.status(E);
  const ev = w.events("evicted_active");
  check("construction after an eviction while Active logs evicted_active", ev.length === 1 && /since_bind_ms/.test(ev[0]), ev.join(" | "));
  check("woken object is Cold (the peer is gone)", s7?.state === "Cold", `state=${s7?.state}`);
  console.log(`info ${ev[0]}`);
} catch (e) {
  fail(e);
} finally {
  peer?.stop();
  await w.dispose();
}
finish();
