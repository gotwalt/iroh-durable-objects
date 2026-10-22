// Mint with pkarr publishing, and destroy, over Workers RPC, on Miniflare.
//
//   npm run build && cargo build -p iroh-do-test-peer && node test/admin-test.mjs
//
// PKARR_RELAY points at a fake pkarr relay in this process. It stores each PUT body and
// returns it on GET, so the object's confirmation (`pkarrConfirm`, the Rust check of
// crates/iroh-do-core/src/pkarr.rs) runs on the record the object signed. Two failure modes:
// the PUT answers 503, or the GET returns a different record.
import { createServer } from "node:http";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { check, fail, finish, sleep, startPeer, startWorker } from "iroh-do/testing";

const dist = join(fileURLToPath(import.meta.url), "..", "..", "dist");
const port = Number(process.env.PORT ?? 8821);
const relayPort = port + 100;

let mode = "ok";
const records = new Map();
const requests = [];
const relay = createServer((req, res) => {
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    requests.push(`${req.method} ${req.url}`);
    if (req.method === "PUT") {
      if (mode === "put503") return res.writeHead(503).end();
      records.set(req.url, Buffer.concat(chunks));
      return res.writeHead(204).end();
    }
    const body = mode === "garbage" ? Buffer.alloc(200, 7) : records.get(req.url);
    if (!body) return res.writeHead(404).end();
    res.writeHead(200, { "content-type": "application/pkarr.org/relays#payload" }).end(body);
  });
});
await new Promise((r) => relay.listen(relayPort, "127.0.0.1", r));

const w = startWorker({
  dist, script: "iroh-ts", className: "Echo", port,
  vars: { PKARR_RELAY: `http://127.0.0.1:${relayPort}/pkarr` },
});
let peer;
try {
  await w.mf.ready;

  const noToken = await w.admin("/mint", { method: "POST" });
  check("mint without the bearer token: 401", noToken.status === 401, `${noToken.status}`);

  // Publish and confirm.
  const m1 = await w.mint();
  check("mint publishes the pkarr record and confirms it by GET", m1.published === true, JSON.stringify(m1));
  check("PUT then GET at <relay>/<z32>", requests.join(",") === `PUT /pkarr/${m1.id},GET /pkarr/${m1.id}`, requests.join(","));
  const s1 = await w.status(m1.id);
  const inDay = s1.pkarr.next_ms - Date.now();
  check("republish scheduled a day later", s1.pkarr.failures === 0 && s1.pkarr.status === "ok" && inDay > 23.9 * 3600e3 && inDay <= 24 * 3600e3,
    JSON.stringify(s1.pkarr));

  // The PUT fails: retried after the linear backoff (60 s, plus up to 30 s jitter).
  mode = "put503";
  const m2 = await w.mint();
  const s2 = await w.status(m2.id);
  const retry = s2.pkarr.next_ms - Date.now();
  check("PUT 503: not published, retry in 60-90 s", m2.published === false && s2.pkarr.failures === 1 && s2.pkarr.status === "put status 503" && retry > 55e3 && retry <= 90e3,
    `${JSON.stringify(s2.pkarr)} retry_in_ms=${retry}`);

  // The GET returns something else: a stale PUT also answers 204.
  mode = "garbage";
  const m3 = await w.mint();
  const s3 = await w.status(m3.id);
  check("GET returns another record: not published", m3.published === false && s3.pkarr.status === "get returned a different record",
    JSON.stringify(s3.pkarr));
  mode = "ok";

  // Destroy over RPC, with a peer holding a relay socket.
  peer = startPeer(m1.id, port);
  check("peer ready", (await peer.next()).startsWith("ready"));
  peer.send("echo before");
  const e1 = await peer.next();
  check("echo before destroy", e1.startsWith("echo: before"), e1);
  const del0 = await w.admin(`/endpoints/${m1.id}`, { method: "DELETE" });
  check("destroy without the bearer token: 401", del0.status === 401, `${del0.status}`);
  const del1 = await w.admin(`/endpoints/${m1.id}`, { method: "DELETE", headers: w.auth });
  const body1 = await del1.json();
  check("destroy: 200 {id, destroyed}", del1.status === 200 && body1.id === m1.id && body1.destroyed === true, JSON.stringify(body1));
  const ev = w.events("destroy");
  check("destroy closed the relay socket", ev.length === 1 && /sockets: 1/.test(ev[0]), ev.join(" | "));
  check("status after destroy: 404", (await w.status(m1.id)) === null);
  const del2 = await w.admin(`/endpoints/${m1.id}`, { method: "DELETE", headers: w.auth });
  check("second destroy: 404", del2.status === 404, `${del2.status}`);
  const up = await w.mf.dispatchFetch(`http://${m1.id}.localhost:${port}/relay`, {
    headers: { Upgrade: "websocket", "Sec-WebSocket-Protocol": "iroh-relay-v2" },
  });
  check("relay upgrade after destroy: 404", up.status === 404, `${up.status}`);
  const other = await w.status(m2.id);
  check("other endpoints unaffected", other?.id === m2.id, `${other?.id}`);
  await sleep(100);
} catch (e) {
  fail(e);
} finally {
  peer?.stop();
  await w.dispose();
  relay.close();
}
finish();
