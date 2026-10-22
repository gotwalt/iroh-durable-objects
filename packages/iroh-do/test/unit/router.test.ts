// The router and the registry in workerd: requests through the test Worker's router, and a
// second router over a counting fake registry for the per-isolate cache (A-M4).
import { exports } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { irohRouter } from "../../src/router.js";
import { newId } from "./ids.js";

const auth = { authorization: "Bearer test-token" };
const VALID = newId();
const get = (path: string, init?: RequestInit) => exports.default.fetch(`http://admin.localhost${path}`, init);

describe("admin surface", () => {
  it("mint needs the token, compared whatever its length", async () => {
    for (const authorization of ["", "Bearer x", "Bearer test-token-and-more", "Basic test-token"]) {
      const r = await get("/mint", { method: "POST", headers: { authorization } });
      expect(r.status).toBe(401);
    }
  });

  it("mint by app, list with snake_case fields and paging links", async () => {
    const r = await get("/mint", { method: "POST", headers: auth, body: JSON.stringify({ label: "<b>x</b>" }) });
    expect(r.status).toBe(200);
    const minted = (await r.json()) as Record<string, unknown>;
    expect(Object.keys(minted).sort()).toEqual(["app", "id", "published", "relay_url"]);
    expect(minted.app).toBe("test");
    const list = (await (await get("/endpoints.json")).json()) as Record<string, unknown>[];
    const entry = list.find((e) => e.id === minted.id);
    expect(entry).toMatchObject({ app: "test", label: "<b>x</b>" });
    expect(typeof entry?.minted_at).toBe("number");
    const page = await (await get("/")).text();
    expect(page).toContain(`<code>join ${String(minted.id)}</code>`);
    expect(page).toContain("&lt;b&gt;x&lt;/b&gt;");
  });

  it("unknown app, long label and control characters are 400", async () => {
    const post = (body: unknown) => get("/mint", { method: "POST", headers: auth, body: JSON.stringify(body) });
    const unknown = await post({ app: "nope" });
    expect([unknown.status, await unknown.text()]).toEqual([
      400,
      'unknown app; this deployment has ["test", "broken"]',
    ]);
    expect((await post({ label: "é".repeat(65) })).status).toBe(400);
    expect((await post({ label: "a\u0085b" })).status).toBe(400);
  });

  it("status without the token omits the host's fields", async () => {
    const r = await get("/mint", { method: "POST", headers: auth });
    const { id } = (await r.json()) as { id: string };
    const open = (await (await get(`/status/${id}`)).json()) as Record<string, unknown>;
    const full = (await (await get(`/status/${id}`, { headers: auth })).json()) as Record<string, unknown>;
    for (const k of ["sockets", "tasks", "pkarr", "generation", "isolate_nodes"]) {
      expect(open).not.toHaveProperty(k);
      expect(full).toHaveProperty(k);
    }
    expect(open).toMatchObject({ id, app: "test", state: "Cold" });
  });

  it("a valid id that was never minted is 404", async () => {
    expect((await get(`/status/${VALID}`)).status).toBe(404);
    const relay = await exports.default.fetch(`http://${VALID}.localhost/relay`, { headers: { upgrade: "websocket" } });
    expect(relay.status).toBe(404);
  });
});

/** A registry namespace whose one object answers `lookup` from `apps` and counts the calls. */
function fakeRegistry(apps: Record<string, string>, fail = false) {
  const calls: string[] = [];
  const object = {
    lookup: async (id: string) => {
      calls.push(id);
      if (fail) throw new Error("registry overloaded (simulated)");
      return apps[id] ?? null;
    },
  };
  const ns = { idFromName: (n: string) => n, get: () => object };
  return { ns, calls };
}

function fakeEndpoints() {
  const ns = {
    idFromName: (n: string) => n,
    get: () => ({ irohStatus: async () => ({ id: VALID, app: "test", state: "Warm", tasks: 0 }) }),
  };
  return ns;
}

describe("class resolution", () => {
  const req = (id: string) => new Request(`http://admin.localhost/status/${id}`);

  it("caches E → app per router, and unregistered ids for a while", async () => {
    const reg = fakeRegistry({ [VALID]: "test" });
    const env = { ZONE_SUFFIX: "localhost", REG: reg.ns, EP: fakeEndpoints() };
    const router = irohRouter({ apps: { test: { binding: "EP" } }, registry: "REG" });
    expect((await router.fetch(req(VALID), env)).status).toBe(200);
    expect((await router.fetch(req(VALID), env)).status).toBe(200);
    expect(reg.calls).toEqual([VALID]);
    const other = newId();
    expect((await router.fetch(req(other), env)).status).toBe(404);
    expect((await router.fetch(req(other), env)).status).toBe(404);
    expect(reg.calls).toEqual([VALID, other]);
  });

  it("a failing registry is a 503, not an exception", async () => {
    const reg = fakeRegistry({}, true);
    const env = { ZONE_SUFFIX: "localhost", REG: reg.ns, EP: fakeEndpoints() };
    const router = irohRouter({ apps: { test: { binding: "EP" } }, registry: "REG" });
    expect((await router.fetch(req(VALID), env)).status).toBe(503);
  });

  it("a missing binding is a 503", async () => {
    const router = irohRouter({ binding: "NOPE" });
    expect((await router.fetch(req(VALID), { ZONE_SUFFIX: "localhost" })).status).toBe(503);
  });

  it("handle() leaves hosts outside the zone to the caller", async () => {
    const router = irohRouter({ binding: "EP" });
    expect(await router.handle(new Request("http://example.com/"), { ZONE_SUFFIX: "localhost" })).toBeNull();
    expect((await router.fetch(new Request("http://example.com/"), { ZONE_SUFFIX: "localhost" })).status).toBe(404);
  });

  it("defaultApp must name an app", () => {
    expect(() => irohRouter({ apps: { a: { binding: "A" } }, registry: "R", defaultApp: "b" })).toThrow(/defaultApp/);
  });
});
