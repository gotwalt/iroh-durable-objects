// The chat room in Miniflare with two present members, across a forced hibernation.
//
//   npm run build && cargo build -p iroh-do-chat && npx vitest run test/room.test.mjs
//
// Members are the stock `iroh-do-chat` without a UI: `--listen-for` joins and prints every
// event as the room sent it, `--say` joins, says one line and exits. Each uses the room as its
// home relay, so it holds a relay socket to the object while it runs (design note 008).
//
// 1. ana and bo listen; bo's join reaches ana as a delivery.
// 2. Once both requests' connections have closed (t_idle) and the endpoint is dropped
//    (t_linger), the object is Warm with both members present. It is evicted with
//    `unsafeEvictDurableObject(..., { webSockets: "hibernate" })`, which workerd refuses while
//    timers or fetches are pending, so a prompt eviction shows nothing ran.
// 3. cy says a line: the woken object dials ana and bo back over their hibernated relay
//    sockets and delivers it. cy says again within t_idle: the deliveries reuse the dialed
//    connections, whose handlers stayed pending.
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { sleep, startWorker } from "iroh-do/testing";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { json, runChat } from "./clients.mjs";

const dist = join(fileURLToPath(import.meta.url), "..", "..", "dist");
const port = Number(process.env.PORT ?? 8832);
const SCRIPT = "iroh-ts";
const classes = { ENDPOINTS: "Echo", CHAT: "Chat", REGISTRY: "Registry" };

// Tests in a file run in order; each step builds on the one before.
describe("chat room across a forced hibernation", () => {
  const state = mkdtempSync(join(tmpdir(), "iroh-do-room-test-"));
  const members = [];
  let w;
  let E;
  let ana;
  let bo;
  let anaReady;
  let warm;
  let afterEviction;

  const member = (nick, extra) => {
    const c = runChat(port, ["chat", E, "--nick", nick, "--state-dir", state, ...extra]);
    members.push(c);
    return c;
  };
  /** Accepts a delivered event (no `hist`) that `pred` accepts. */
  const live = (pred) => (l) => {
    const ev = json(l);
    return ev !== null && ev.hist === undefined && pred(ev);
  };
  const chatStats = async () => {
    const ns = await w.mf.getDurableObjectNamespace("CHAT", SCRIPT);
    return ns.get(ns.idFromName(E)).chatStats();
  };

  beforeAll(async () => {
    w = startWorker({ dist, script: SCRIPT, classes, port });
    await w.mf.ready;
    E = (await w.mint({ app: "chat", label: "room test" })).id;
  });

  afterAll(async () => {
    for (const m of members) m.stop();
    await w?.dispose();
    rmSync(state, { recursive: true, force: true });
  });

  test("two listeners: bo's join is delivered to ana", async () => {
    ana = member("ana", ["--listen-for", "120"]);
    anaReady = await ana.waitFor((l) => json(l)?.t === "join" && json(l)?.nick === "ana", 60_000);
    expect(json(anaReady).hist).toBe(true);
    bo = member("bo", ["--listen-for", "120"]);
    await bo.waitFor((l) => json(l)?.t === "join" && json(l)?.nick === "bo", 60_000);
    await ana.waitFor(
      live((ev) => ev.t === "join" && ev.nick === "bo"),
      20_000,
    );
  }, 120_000);

  test("Warm with both members present, then a prompt forced eviction", async () => {
    for (let i = 0; i < 40; i++) {
      warm = await w.status(E);
      if (warm.state === "Warm" && warm.tasks === 0) break;
      await sleep(1_000);
    }
    expect(warm).toMatchObject({ state: "Warm", tasks: 0, members: 2, present: 2 });
    expect(warm.sockets).toHaveLength(2);

    const t0 = Date.now();
    await w.mf.unsafeEvictDurableObject(SCRIPT, "Chat", { name: E, webSockets: "hibernate" });
    expect(Date.now() - t0).toBeLessThan(5_000);

    afterEviction = await w.status(E);
    expect(afterEviction.wakes).toBe(warm.wakes + 1);
    expect([...afterEviction.sockets].sort()).toEqual([...warm.sockets].sort());
    expect(afterEviction.present).toBe(2);
    const fresh = await chatStats();
    expect(fresh).toMatchObject({ deliveriesOk: 0, deliveryDials: 0 });
  }, 60_000);

  test("deliveries dial the members back, then reuse the dialed connections", async () => {
    const text1 = 'after the eviction: "quoted" é';
    const cy1 = member("cy", ["--say", text1]);
    const said = json(await cy1.waitFor((l) => json(l)?.t === "msg", 60_000));
    expect(said).toMatchObject({ text: text1, nick: "cy" });
    await cy1.exited;
    const a1 = json(await ana.waitFor(live((ev) => ev.t === "msg" && ev.text === text1), 20_000));
    const b1 = json(await bo.waitFor(live((ev) => ev.t === "msg" && ev.text === text1), 20_000));
    expect(a1.seq).toBe(b1.seq);
    expect(a1.nick).toBe("cy");
    const st1 = await chatStats();
    expect(st1.deliveryDials).toBeGreaterThanOrEqual(2);
    expect(st1.deliveriesFailed).toBe(0);

    const text2 = "second line, within t_idle";
    const cy2 = member("cy", ["--say", text2]);
    await cy2.waitFor((l) => json(l)?.t === "msg", 60_000);
    await cy2.exited;
    await ana.waitFor(live((ev) => ev.t === "msg" && ev.text === text2), 20_000);
    await bo.waitFor(live((ev) => ev.t === "msg" && ev.text === text2), 20_000);
    const st2 = await chatStats();
    expect(st2.deliveryReuses).toBeGreaterThanOrEqual(st1.deliveryReuses + 2);
    expect(st2.deliveriesFailed).toBe(0);
    const via = w.events("room_delivery").map((l) => /via: '(\w+)'/.exec(l)?.[1]);
    expect(via).toContain("dial");
    expect(via).toContain("open");
  }, 120_000);

  test("every member saw the same events, chained by prev", async () => {
    const events = (c) => c.lines.map(json).filter((ev) => ev && ev.seq !== undefined);
    const chain = events(ana).filter((ev) => ev.seq > json(anaReady).seq);
    expect(chain.length).toBeGreaterThan(0);
    for (let i = 1; i < chain.length; i++) expect(chain[i].prev).toBe(chain[i - 1].seq);
    expect(await w.status(E)).toMatchObject({ members: 3, present: 2, records: 5 });
  });
});
