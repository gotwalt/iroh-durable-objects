// The base class in workerd: the alarm's schedule table under failures and traps (A-B1),
// staleness after another object's trap (A-B2), and mint and build failures (A-M1).
import { env, exports } from "cloudflare:workers";
import { runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { generateSecretKey, publicKeyOf } from "../../src/wasm.js";
import type { TestEndpoint } from "./worker.js";

const auth = { authorization: "Bearer test-token" };

async function mint(app = "test"): Promise<string> {
  const r = await exports.default.fetch("http://admin.localhost/mint", {
    method: "POST",
    headers: auth,
    body: JSON.stringify({ app }),
  });
  expect(r.status, await r.clone().text()).toBe(200);
  const { id } = (await r.json()) as { id: string };
  return id;
}

function stub(id: string): DurableObjectStub<TestEndpoint> {
  return env.ENDPOINTS.get(env.ENDPOINTS.idFromName(id));
}

/** The schedule rows of `s`: key → due time. */
async function schedule(s: DurableObjectStub<TestEndpoint>): Promise<Map<string, number>> {
  return runInDurableObject(s, (_o, state) => {
    const rows = state.storage.sql.exec("SELECT key, due_ms FROM iroh_schedule").toArray();
    return new Map(rows.map((r) => [String(r.key), Number(r.due_ms)]));
  });
}

async function alarmAt(s: DurableObjectStub<TestEndpoint>): Promise<number | null> {
  return runInDurableObject(s, (_o, state) => state.storage.getAlarm());
}

/**
 * Stores app work due in the past, in order, and arms the alarm an hour out, so that it runs
 * only when the test calls `runDurableObjectAlarm`, which runs it at once.
 */
async function seed(s: DurableObjectStub<TestEndpoint>, keys: string[]): Promise<void> {
  await runInDurableObject(s, async (_o, state) => {
    const t = Date.now() - 1000;
    keys.forEach((k, i) =>
      state.storage.sql.exec("INSERT INTO iroh_schedule (key, due_ms, payload) VALUES (?, ?, '')", `app:${k}`, t + i),
    );
    await state.storage.setAlarm(Date.now() + 3_600_000);
  });
}

describe("alarm", () => {
  it("deletes a row only after its work ran", async () => {
    const s = stub(await mint());
    await seed(s, ["ok"]);
    expect(await runDurableObjectAlarm(s)).toBe(true);
    expect(await s.runs()).toEqual({ ok: 1 });
    expect((await schedule(s)).has("app:ok")).toBe(false);
  });

  it("retries a row whose work threw, 30 s on", async () => {
    const s = stub(await mint());
    await seed(s, ["throw"]);
    const t0 = Date.now();
    await runDurableObjectAlarm(s);
    const due = (await schedule(s)).get("app:throw");
    expect(due).toBeGreaterThanOrEqual(t0 + 29_000);
    expect(await alarmAt(s)).toBe(due);
  });

  it("A-B1: a trap during app work keeps the row, retries it, and re-arms for the rows left", async () => {
    const s = stub(await mint());
    await seed(s, ["trap", "zz-ok"]);
    const t0 = Date.now();
    await runDurableObjectAlarm(s);
    // The alarm stopped at the trap: the row after it did not run on the trapped node. The
    // rearm armed the alarm for it, already due, so the runtime runs it on the rebuilt node
    // by itself; the test only waits for that.
    for (let i = 0; i < 100 && !("zz-ok" in (await s.runs())); i++) await new Promise((r) => setTimeout(r, 20));
    expect(await s.runs()).toEqual({ trap: 1, "zz-ok": 1 });
    // The trapped key is still there, moved 30 s on, and the alarm is armed for it.
    const rows = await schedule(s);
    expect([...rows.keys()]).toEqual(["app:trap"]);
    expect(rows.get("app:trap")).toBeGreaterThanOrEqual(t0 + 29_000);
    expect(await alarmAt(s)).toBe(rows.get("app:trap"));
    expect((await s.irohStatus())?.rebuilds).toBe(1);
  });

  it("deletes the alarm once no row is left", async () => {
    const s = stub(await mint());
    await seed(s, ["ok"]);
    await runDurableObjectAlarm(s);
    expect(await alarmAt(s)).toBeNull();
  });
});

describe("staleness after a wasm reset", () => {
  it("A-B2: an RPC method reading this.endpoint after another object's trap gets the rebuilt node", async () => {
    const a = stub(await mint());
    const b = stub(await mint());
    const before = await b.probe();
    expect(before.lost).toEqual([]);
    // A traps; the instance shared by both objects is replaced.
    expect(await a.trapInNode()).toBe("NodeTrappedError");
    // B's own method, which the base class does not wrap, reads the endpoint.
    const after = await b.probe();
    expect(after.id).toBe(before.id);
    expect(after.lost).toHaveLength(1);
    expect(after.lost[0]).toContain("wasm instance was reset");
    expect((await b.irohStatus())?.rebuilds).toBe(1);
  });
});

describe("mint and build", () => {
  it("schedule() before mint throws NotMintedError", async () => {
    const s = env.ENDPOINTS.get(env.ENDPOINTS.idFromName("ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1u"));
    expect(await s.scheduleUnminted()).toBe("NotMintedError");
  });

  it("A-M1: a failed node build stores nothing, and the router cleans up", async () => {
    const r = await exports.default.fetch("http://admin.localhost/mint", {
      method: "POST",
      headers: auth,
      body: JSON.stringify({ app: "broken" }),
    });
    expect(r.status).toBe(500);
    const list = (await (await exports.default.fetch("http://admin.localhost/endpoints.json")).json()) as {
      app: string;
    }[];
    expect(list.filter((e) => e.app === "broken")).toEqual([]);
  });

  it("A-M1: the identity, the counters and the app's tables roll back together", async () => {
    const secret = generateSecretKey();
    const key = publicKeyOf(secret);
    const id = key.toZ32();
    key.free();
    const s = env.BROKEN.get(env.BROKEN.idFromName(id));
    const error = await s.irohMint(secret, `http://${id}.localhost/`, "").then(
      () => null,
      (e: unknown) => e,
    );
    expect(String(error)).toMatch(/node build failed/);
    // A later event finds no identity: 404, not a node that never builds.
    const r = await s.fetch(new Request(`http://${id}.localhost/relay`, { headers: { upgrade: "websocket" } }));
    expect(r.status).toBe(404);
    const tables = await runInDurableObject(s, (_o, state) =>
      state.storage.sql
        .exec("SELECT name FROM sqlite_master WHERE type = 'table'")
        .toArray()
        .map((t) => String(t.name)),
    );
    expect(tables.filter((t) => t.startsWith("iroh_") || t === "app_table")).toEqual([]);
  });
});
