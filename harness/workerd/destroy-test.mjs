// Room destruction test (design note 009) for the iroh-do Worker on Miniflare (workerd).
//
//   node harness/workerd/destroy-test.mjs
//
// 1. DELETE /endpoints/<E> needs the admin token.
// 2. Destroying a minted endpoint closes its relay sockets (1000, "destroyed"), removes it
//    from the registry and deletes all of its storage.
// 3. Afterwards the object behaves as if never minted, also after a restart: relay upgrades
//    and /status get 404, and requests write no storage.
// 4. Destroying it again, or destroying an id that was never minted, is 404 and writes nothing.
import { randomBytes } from "node:crypto";
import { mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { request } from "node:http";
import { tmpdir } from "node:os";
import { DatabaseSync } from "node:sqlite";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { Miniflare } from "miniflare";

const here = dirname(fileURLToPath(import.meta.url));
const bundle = join(here, "..", "..", "build", "worker");
const port = Number(process.env.PORT ?? 8848);
const ADMIN_TOKEN = "destroy-test-token";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;
const check = (name, ok, detail = "") => {
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? `: ${detail}` : ""}`);
  if (!ok) failures++;
};

const persistDir = mkdtempSync(join(tmpdir(), "iroh-do-destroy-"));
const text = (value) => ({ type: "text", value });
const start = () => new Miniflare({
  host: "127.0.0.1", port, resourcePersistencePath: persistDir,
  workers: [{ config: {
    name: "iroh-do-chat", type: "worker", compatibilityDate: "2026-09-15",
    manifest: { mainModule: "index.js", modules: {
      "index.js": { type: "esm", contents: readFileSync(join(bundle, "index.js"), "utf8") },
      "index_bg.wasm": { type: "wasm", contents: new Uint8Array(readFileSync(join(bundle, "index_bg.wasm"))) } } },
    env: {
      ROOMS: { type: "durable-object", worker: "iroh-do-chat", exportName: "room" },
      REGISTRY: { type: "durable-object", worker: "iroh-do-chat", exportName: "registry" },
      ZONE_SUFFIX: text("localhost"), ADMIN_TOKEN: text(ADMIN_TOKEN), PKARR_RELAY: text(""), BUNDLE_SHA256: text("destroy-test") },
    exports: {
      room: { type: "durable-object", storage: "sqlite" },
      registry: { type: "durable-object", storage: "sqlite" } } } }],
});
const admin = (path, init) => mf.dispatchFetch(`http://admin.localhost:${port}${path}`, init);
const auth = { Authorization: `Bearer ${ADMIN_TOKEN}` };
const mint = async () => {
  const r = await admin("/mint", { method: "POST", headers: auth, body: JSON.stringify({ label: "doomed" }) });
  const t = await r.text();
  if (r.status !== 200) throw new Error(`mint ${r.status}: ${t}`);
  return JSON.parse(t).id;
};
const destroy = (id, headers = auth) => admin(`/endpoints/${id}`, { method: "DELETE", headers });
const relay = (id) => mf.dispatchFetch(`http://${id}.localhost:${port}/relay`, {
  headers: { Upgrade: "websocket", "Sec-WebSocket-Protocol": "iroh-relay-v2, iroh-relay-v1" },
});
const list = async () => (await (await admin("/endpoints.json")).json());
// A relay socket over the real network (not Miniflare's in-process loopback), so a close sent
// by the object from another request's handler is observed as a client would observe it.
// Resolves `closed` with {code, reason} when a close frame arrives.
const netSocket = (id) => new Promise((resolve, reject) => {
  const req = request({
    host: "127.0.0.1", port, path: "/relay",
    headers: {
      Host: `${id}.localhost:${port}`, Connection: "Upgrade", Upgrade: "websocket",
      "Sec-WebSocket-Version": "13", "Sec-WebSocket-Key": randomBytes(16).toString("base64"),
      "Sec-WebSocket-Protocol": "iroh-relay-v2, iroh-relay-v1",
    },
  });
  req.on("upgrade", (_res, sock) => {
    let buf = Buffer.alloc(0);
    const closed = new Promise((done) => {
      sock.on("data", (d) => {
        buf = Buffer.concat([buf, d]);
        while (buf.length >= 2) {
          const op = buf[0] & 0x0f;
          let len = buf[1] & 0x7f, off = 2;
          if (len === 126) { len = buf.readUInt16BE(2); off = 4; }
          if (buf.length < off + len) break;
          const payload = buf.subarray(off, off + len);
          buf = buf.subarray(off + len);
          if (op === 8) done({ code: payload.readUInt16BE(0), reason: payload.subarray(2).toString() });
        }
      });
      sock.on("close", () => done(null));
    });
    resolve({ sock, closed });
  });
  req.on("response", (res) => reject(new Error(`upgrade answered ${res.statusCode}`)));
  req.on("error", reject);
  req.end();
});
const sqliteFile = async (name) => {
  const ns = await mf.getDurableObjectNamespace("ROOMS");
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
  const e = await mint();
  const before = tablesOf(await sqliteFile(e));
  check("minted endpoint has storage", before.includes("identity"), JSON.stringify(before));

  // An open relay socket, as a member's client holds.
  const net = await netSocket(e);
  check("relay upgrade before destroy", true);
  const timeout = new Promise((r) => setTimeout(() => r("timeout"), 3000));

  check("destroy without token refused", (await destroy(e, {})).status === 401);
  check("destroy with a wrong token refused",
    (await destroy(e, { Authorization: "Bearer nope" })).status === 401);
  check("destroy still listed after refused attempts", (await list()).some((x) => x.id === e));

  const r = await destroy(e);
  const body = r.status === 200 ? await r.json() : await r.text();
  check("destroy returns {id, destroyed: true}", r.status === 200 && body.id === e && body.destroyed === true,
    `${r.status} ${JSON.stringify(body)}`);
  const closed = await Promise.race([net.closed, timeout]);
  net.sock.destroy();
  check("open relay socket closed with 1000 \"destroyed\"",
    closed?.code === 1000 && closed?.reason === "destroyed", JSON.stringify(closed));
  check("registry no longer lists it", !(await list()).some((x) => x.id === e));

  const again = await relay(e);
  again.webSocket?.accept();
  check("relay upgrade after destroy is 404", again.status === 404, String(again.status));
  check("/status after destroy is 404", (await admin(`/status/${e}`)).status === 404);
  const after = tablesOf(await sqliteFile(e));
  check("storage is empty after destroy and later requests", after.length === 0, JSON.stringify(after));
  check("second destroy is 404", (await destroy(e)).status === 404);

  // A restart must not resurrect anything: the constructor writes only for minted objects.
  await mf.dispose();
  mf = start();
  await mf.ready;
  const cold = await relay(e);
  cold.webSocket?.accept();
  check("relay upgrade after restart is 404", cold.status === 404, String(cold.status));
  const afterRestart = tablesOf(await sqliteFile(e));
  check("storage still empty after restart", afterRestart.length === 0, JSON.stringify(afterRestart));

  const never = "yr9ge793ppirfkkm4edbyu56mbnctp4i9xsf3468xsda9o8cuo6o";
  check("destroying a never-minted id is 404", (await destroy(never)).status === 404);
  const nt = tablesOf(await sqliteFile(never));
  check("which writes no storage", nt.length === 0, JSON.stringify(nt));
  check("destroying a malformed id is 404", (await destroy("registry")).status === 404);
} catch (err) {
  console.log(`FAIL exception: ${err?.stack ?? err}`);
  failures++;
} finally {
  await mf?.dispose().catch(() => {});
  rmSync(persistDir, { recursive: true, force: true });
}
console.log(failures ? `${failures} FAILURE(S)` : "ALL PASS");
process.exit(failures ? 1 : 0);
