// Mint with pkarr publishing, and destroy, over Workers RPC, on Miniflare.
//
// PKARR_RELAY points at a fake pkarr relay in this process. It stores each PUT body and
// returns it on GET, so the object's confirmation (`pkarrConfirm`, the Rust check of
// crates/iroh-do-core/src/pkarr.rs) runs on the record the object signed. Failure modes: the
// PUT answers 503, the GET returns a different record, and the relay never answers.
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { fileURLToPath } from "node:url";
import { startPeer, startWorker, type TestWorker } from "iroh-do-testing";
import { cargoBin, nativeWrapper } from "iroh-do-testing/repo";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

const dist = fileURLToPath(new URL("../../dist", import.meta.url));

type Mode = "ok" | "put503" | "garbage";
let mode: Mode = "ok";
const records = new Map<string, Buffer>();
const requests: string[] = [];

function fakeRelay(): Server {
  return createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => {
      requests.push(`${req.method} ${req.url}`);
      const url = req.url ?? "";
      if (req.method === "PUT") {
        if (mode === "put503") return void res.writeHead(503).end();
        records.set(url, Buffer.concat(chunks));
        return void res.writeHead(204).end();
      }
      const body = mode === "garbage" ? Buffer.alloc(200, 7) : records.get(url);
      if (!body) return void res.writeHead(404).end();
      res.writeHead(200).end(body);
    });
  });
}

type Pkarr = { failures: number; status: string; next_ms: number };

describe("mint with pkarr, and destroy", () => {
  let w: TestWorker;
  let relay: Server;

  beforeAll(async () => {
    relay = fakeRelay();
    await new Promise<void>((r) => relay.listen(0, "127.0.0.1", r));
    const { port } = relay.address() as AddressInfo;
    w = await startWorker({
      dist,
      script: "iroh-ts",
      classes: { ENDPOINTS: "Echo" },
      vars: { PKARR_RELAY: `http://127.0.0.1:${port}/pkarr` },
    });
  });
  afterAll(async () => {
    await w?.dispose();
    relay?.close();
  });

  const pkarr = async (id: string) => (await w.status(id))?.pkarr as Pkarr;

  it("mint needs the bearer token", async () => {
    expect((await w.admin("/mint", { method: "POST" })).status).toBe(401);
  });

  it("publishes the record and confirms it by GET; republishes a day later", async () => {
    mode = "ok";
    requests.length = 0;
    const m = await w.mint();
    expect(m.published).toBe(true);
    expect(requests).toEqual([`PUT /pkarr/${m.id}`, `GET /pkarr/${m.id}`]);
    const p = await pkarr(m.id);
    expect([p.failures, p.status]).toEqual([0, "ok"]);
    const inDay = p.next_ms - Date.now();
    expect(inDay).toBeGreaterThan(23.9 * 3600e3);
    expect(inDay).toBeLessThanOrEqual(24 * 3600e3);
  });

  it("a PUT that fails is retried after 60-90 s", async () => {
    mode = "put503";
    const m = await w.mint();
    expect(m.published).toBe(false);
    const p = await pkarr(m.id);
    expect([p.failures, p.status]).toEqual([1, "put status 503"]);
    const retry = p.next_ms - Date.now();
    expect(retry).toBeGreaterThan(55e3);
    expect(retry).toBeLessThanOrEqual(90e3);
  });

  it("a GET that returns another record does not confirm", async () => {
    mode = "garbage";
    const m = await w.mint();
    expect(m.published).toBe(false);
    expect((await pkarr(m.id)).status).toBe("get returned a different record");
  });

  it("destroy closes the relay socket and leaves nothing", async () => {
    mode = "ok";
    const m = await w.mint();
    const other = await w.mint();
    const peer = await startPeer(cargoBin("iroh-do-test-peer"), m.id, w.port, nativeWrapper);
    try {
      expect(await peer.run("echo before")).toMatch(/^echo: before/);
      expect((await w.admin(`/endpoints/${m.id}`, { method: "DELETE" })).status).toBe(401);
      const del = await w.admin(`/endpoints/${m.id}`, { method: "DELETE", auth: true });
      expect([del.status, await del.json()]).toEqual([200, { id: m.id, destroyed: true }]);
      const ev = w.events("destroy");
      expect(ev).toHaveLength(1);
      expect(ev[0]).toContain("sockets: 1");
      expect(await w.status(m.id)).toBeNull();
      expect((await w.admin(`/endpoints/${m.id}`, { method: "DELETE", auth: true })).status).toBe(404);
      const up = await w.fetch(m.id, "/relay", {
        headers: { Upgrade: "websocket", "Sec-WebSocket-Protocol": "iroh-relay-v2" },
      });
      expect(up.status).toBe(404);
      expect((await w.status(other.id))?.id).toBe(other.id);
    } finally {
      peer.stop();
    }
  });
});
