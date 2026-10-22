// The deployment's router and registry in Miniflare: mint by app, the endpoint list, class
// resolution through the registry, destroy, and stale cache entries. The chat endpoint is
// reached with the stock `iroh-do-chat ping` (iroh-do/echo/1, served by class Chat), the
// echo endpoint with the stock test peer (iroh-example/echo/0, class Echo): each answers
// only if the router picked its class.
import { fileURLToPath } from "node:url";
import { startPeer, startWorker, type TestWorker } from "iroh-do-testing";
import { cargoBin, nativeWrapper } from "iroh-do-testing/repo";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { runChat } from "./clients.js";

const dist = fileURLToPath(new URL("../dist", import.meta.url));
const SCRIPT = "iroh-ts";
const classes = { ENDPOINTS: "Echo", CHAT: "Chat", REGISTRY: "Registry" };

type Entry = { id: string; app: string; label: string; minted_at: number };

describe("router and registry", () => {
  let w: TestWorker;
  let chat: { id: string; app?: string };
  let echo: { id: string; app?: string };

  beforeAll(async () => {
    w = await startWorker({ dist, script: SCRIPT, classes });
  });
  afterAll(async () => {
    await w?.dispose();
  });

  const post = (body: string, auth = true) => w.admin("/mint", { method: "POST", auth, body });
  const list = async () => (await (await w.admin("/endpoints.json")).json()) as Entry[];

  it("mints by app: chat by default, echo with a label", async () => {
    chat = await w.mint();
    expect(chat.app).toBe("chat");
    echo = await w.mint({ app: "echo", label: "<b>echo</b> & co" });
    expect(echo.app).toBe("echo");
    const bad = await post(JSON.stringify({ app: "nope" }));
    expect([bad.status, await bad.text()]).toEqual([400, 'unknown app; this deployment has ["chat", "echo"]']);
    expect((await post("", false)).status).toBe(401);
  });

  it("lists both, newest first, and renders join hints with labels escaped", async () => {
    const l = await list();
    expect(l.map((e) => [e.id, e.app, e.label])).toEqual([
      [echo.id, "echo", "<b>echo</b> & co"],
      [chat.id, "chat", ""],
    ]);
    const page = await (await w.admin("/")).text();
    expect(page).toContain(`<code>iroh-do-chat ${chat.id}</code>`);
    expect(page).toContain(`<code>iroh-do-echo-client ${echo.id}</code>`);
    expect(page).toContain("&lt;b&gt;echo&lt;/b&gt; &amp; co");
    expect((await w.admin("/endpoints.json", { method: "POST" })).status).toBe(405);
  });

  it("status reaches each endpoint's class", async () => {
    expect(await w.status(chat.id)).toMatchObject({ app: "chat", members: 0, present: 0 });
    const se = await w.status(echo.id);
    expect(se?.app).toBe("echo");
    expect(se).not.toHaveProperty("members");
  });

  it("stock peers reach each class through the relay path", async () => {
    const ping = runChat(w.port, ["ping", chat.id]);
    expect(await ping.take((l) => l.startsWith("ping 1/1"), 60_000)).toMatch(/echo 32 B ok/);
    await ping.exited;
    const peer = await startPeer(cargoBin("iroh-do-test-peer"), echo.id, w.port, nativeWrapper);
    try {
      expect(await peer.run("echo routed")).toMatch(/^echo: routed/);
    } finally {
      peer.stop();
    }
  });

  it("a stale cache entry reaches an object without identity, which answers 404", async () => {
    // Destroyed behind this isolate's back, as another isolate's destroy would leave it.
    const direct = await (await w.object<{ irohDestroy(): Promise<unknown> }>("CHAT", chat.id)).irohDestroy();
    expect(direct).toEqual({ id: chat.id, destroyed: true });
    expect((await w.admin(`/status/${chat.id}`)).status).toBe(404);
    const relay = await w.fetch(chat.id, "/relay", {
      headers: { Upgrade: "websocket", "Sec-WebSocket-Protocol": "iroh-relay-v2" },
    });
    expect(relay.status).toBe(404);
  });

  it("destroy through the admin surface removes the entry; again is 404", async () => {
    const del = await w.admin(`/endpoints/${echo.id}`, { method: "DELETE", auth: true });
    expect([del.status, await del.json()]).toEqual([200, { id: echo.id, destroyed: true }]);
    expect((await list()).some((e) => e.id === echo.id)).toBe(false);
    expect((await w.admin(`/status/${echo.id}`)).status).toBe(404);
    expect((await w.admin(`/endpoints/${echo.id}`, { method: "DELETE", auth: true })).status).toBe(404);
    // The chat entry whose object is gone: 404, and the entry goes too.
    expect((await w.admin(`/endpoints/${chat.id}`, { method: "DELETE", auth: true })).status).toBe(404);
    expect(await list()).toEqual([]);
  });
});
