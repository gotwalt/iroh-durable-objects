// The deployment's router and registry in Miniflare: mint by app, the endpoint list, class
// resolution through the registry and its per-isolate cache, and destroy.
//
//   npm run build && cargo build -p iroh-do-chat -p iroh-do-test-peer && node test/router-test.mjs
//
// The chat endpoint is reached with the stock `iroh-do-chat ping` (iroh-do/echo/1, served by
// class Chat), the echo endpoint with the stock test peer (iroh-example/echo/0, class Echo):
// each answers only if the router picked its class.
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { check, fail, finish, startPeer, startWorker } from "iroh-do/testing";
import { runChat } from "./clients.mjs";

const dist = join(fileURLToPath(import.meta.url), "..", "..", "dist");
const port = Number(process.env.PORT ?? 8830);
const SCRIPT = "iroh-ts";
const classes = { ENDPOINTS: "Echo", CHAT: "Chat", REGISTRY: "Registry" };
const opts = { dist, script: SCRIPT, classes, port };

let w = startWorker(opts);
let peer;
try {
  await w.mf.ready;
  const post = (body, headers = w.auth) => w.admin("/mint", { method: "POST", headers, body });

  // Mint.
  const chat = await w.mint();
  check("mint with no body: the first app, chat", chat.app === "chat" && chat.id?.length === 52 && chat.published === false,
    JSON.stringify(chat));
  const echo = await w.mint({ app: "echo", label: "<b>echo</b> & co" });
  check("mint {app: echo, label}", echo.app === "echo" && echo.id?.length === 52, JSON.stringify(echo));
  const bad = await post(JSON.stringify({ app: "nope" }));
  const badText = await bad.text();
  check("unknown app: 400 naming the apps", bad.status === 400 && badText === 'unknown app; this deployment has ["chat", "echo"]',
    `${bad.status} ${badText}`);
  const long = await post(JSON.stringify({ label: "é".repeat(65) }));
  check("label of 65 characters: 400", long.status === 400 && (await long.text()) === "label longer than 64 characters");
  const ctl = await post(JSON.stringify({ label: "a\u0085b" }));
  check("label with a control character: 400", ctl.status === 400 && (await ctl.text()) === "label contains control characters");
  const notJson = await post("label=x");
  check("body that is not JSON: 400", notJson.status === 400 && (await notJson.text()).startsWith('body must be JSON {"app":…, "label":…}:'));
  const noAuth = await post("", {});
  check("mint without the token: 401", noAuth.status === 401);

  // The list.
  const list = await (await w.admin("/endpoints.json")).json();
  check("endpoints.json: both, newest first, with app and label",
    list.length === 2 && list[0].id === echo.id && list[0].app === "echo" && list[0].label === "<b>echo</b> & co"
      && list[1].id === chat.id && list[1].app === "chat" && list[1].label === "",
    JSON.stringify(list));
  const page = await (await w.admin("/")).text();
  check("list page: join hints per app, label escaped",
    page.includes(`<code>iroh-do-chat ${chat.id}</code>`) && page.includes(`<code>iroh-do-echo-client ${echo.id}</code>`)
      && page.includes("&lt;b&gt;echo&lt;/b&gt; &amp; co") && !page.includes("<b>echo</b>"));
  const postList = await w.admin("/endpoints.json", { method: "POST" });
  check("POST /endpoints.json: 405", postList.status === 405);

  // Status through the router reaches each endpoint's class.
  const sc = await w.status(chat.id);
  const se = await w.status(echo.id);
  check("status of the chat endpoint comes from class Chat", sc?.app === "chat" && sc.records === 0 && sc.members === 0 && sc.present === 0,
    JSON.stringify({ app: sc?.app, records: sc?.records, members: sc?.members, present: sc?.present }));
  check("status of the echo endpoint comes from class Echo", se?.app === "echo" && se.records === undefined, `app=${se?.app}`);
  const unknown = "ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1u";
  check("status of a valid id that was never minted: 404", (await w.admin(`/status/${unknown}`)).status === 404);
  const up = await w.mf.dispatchFetch(`http://${unknown}.localhost:${port}/relay`, { headers: { Upgrade: "websocket" } });
  check("relay upgrade for a valid id that was never minted: 404", up.status === 404);

  // Stock peers through the relay path.
  const ping = runChat(port, ["ping", chat.id]);
  const pong = await ping.waitFor((l) => l.startsWith("ping 1/1"), 60_000);
  check("iroh-do-chat ping reaches class Chat (iroh-do/echo/1)", /echo 32 B ok/.test(pong), pong);
  await ping.exited;
  peer = startPeer(echo.id, port);
  check("test peer ready on the echo endpoint", (await peer.next()).startsWith("ready"));
  peer.send("echo routed");
  const routed = await peer.next();
  check("test peer echoes off class Echo (iroh-example/echo/0)", routed.startsWith("echo: routed"), routed);
  peer.stop();
  peer = null;

  // The cache. A new isolate (Miniflare restarted on the same storage) has an empty cache and
  // a new registry object, whose lookup count starts at 0.
  await w.dispose(true);
  w = startWorker({ ...opts, persist: w.persistDir });
  await w.mf.ready;
  const registry = async () => {
    const ns = await w.mf.getDurableObjectNamespace("REGISTRY", SCRIPT);
    return ns.get(ns.idFromName("registry"));
  };
  await w.status(chat.id);
  const l1 = await (await registry()).lookups();
  await w.status(chat.id);
  const p2 = runChat(port, ["ping", chat.id]);
  await p2.waitFor((l) => l.startsWith("ping 1/1"), 60_000);
  await p2.exited;
  const l2 = await (await registry()).lookups();
  check("first request for E in an isolate asks the registry, later ones use the cache", l1 === 1 && l2 === 1,
    `lookups after first status ${l1}, after second status and a relay session ${l2}`);
  await w.status(echo.id);
  const l3 = await (await registry()).lookups();
  check("another endpoint: one more lookup", l3 === 2, `lookups ${l3}`);

  // A stale cache entry. The object is destroyed behind the router's back, as another
  // isolate's destroy would leave this isolate's cache: the request reaches the object, which
  // has no identity and answers 404.
  const chatNs = await w.mf.getDurableObjectNamespace("CHAT", SCRIPT);
  const direct = await chatNs.get(chatNs.idFromName(chat.id)).destroy();
  check("destroy the chat object directly over RPC", direct?.destroyed === true, JSON.stringify(direct));
  const staleStatus = await w.admin(`/status/${chat.id}`);
  const staleRelay = await w.mf.dispatchFetch(`http://${chat.id}.localhost:${port}/relay`, {
    headers: { Upgrade: "websocket", "Sec-WebSocket-Protocol": "iroh-relay-v2" },
  });
  const l4 = await (await registry()).lookups();
  check("stale cache entry: status and relay reach the object, which answers 404", staleStatus.status === 404 && staleRelay.status === 404 && l4 === 2,
    `status ${staleStatus.status}, relay ${staleRelay.status}, lookups ${l4}`);

  // Destroy through the admin surface.
  const del = await w.admin(`/endpoints/${echo.id}`, { method: "DELETE", headers: w.auth });
  const delBody = await del.json().catch(() => null);
  check("DELETE /endpoints/<echo>: 200 {id, destroyed}", del.status === 200 && delBody?.id === echo.id && delBody.destroyed === true,
    `${del.status} ${JSON.stringify(delBody)}`);
  const after = await (await w.admin("/endpoints.json")).json();
  check("destroyed endpoint left the list", !after.some((e) => e.id === echo.id), JSON.stringify(after.map((e) => e.id)));
  check("status after destroy: 404", (await w.admin(`/status/${echo.id}`)).status === 404);
  const again = await w.admin(`/endpoints/${echo.id}`, { method: "DELETE", headers: w.auth });
  check("second destroy: 404", again.status === 404);
  // The chat entry is still listed (its object was destroyed directly); a destroy through the
  // router finds the object unminted, removes the entry and answers 404, as in the Rust router.
  const cleanup = await w.admin(`/endpoints/${chat.id}`, { method: "DELETE", headers: w.auth });
  const final = await (await w.admin("/endpoints.json")).json();
  check("destroy of an entry whose object is gone: 404, entry removed", cleanup.status === 404 && final.length === 0,
    `${cleanup.status}, ${final.length} left`);
} catch (e) {
  fail(e);
} finally {
  peer?.stop();
  await w.dispose();
}
finish();
