// The chat room in Miniflare with two present members, across a forced hibernation.
//
// Members are the stock `iroh-do-chat` without a UI: `--listen-for` joins and prints every
// event as the room sent it, `--say` joins, says one line and exits. Each uses the room as its
// home relay, so it holds a relay socket to the object while it runs (design note 008).
//
// 1. ana and bo listen; bo's join reaches ana as a delivery.
// 2. Once the object is Warm with both members present, it is evicted with
//    `unsafeEvictDurableObject(..., { webSockets: "hibernate" })`, which workerd refuses while
//    timers or fetches are pending, so a prompt eviction shows nothing ran.
// 3. cy says a line: the woken object dials ana and bo back over their hibernated relay
//    sockets. cy says again within t_idle: the deliveries reuse the dialed connections. The
//    room's `room_delivery` log events tell dials from reuses.
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { type LineProcess, poll, startWorker, type TestWorker } from "iroh-do-testing";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { json, runChat } from "./clients.js";

const dist = fileURLToPath(new URL("../dist", import.meta.url));
const SCRIPT = "iroh-ts";
const classes = { ENDPOINTS: "Echo", CHAT: "Chat", REGISTRY: "Registry" };

describe("chat room across a forced hibernation", () => {
  const state = mkdtempSync(join(tmpdir(), "iroh-do-room-test-"));
  const members: LineProcess[] = [];
  let w: TestWorker;
  let E: string;
  let ana: LineProcess;
  let bo: LineProcess;
  let anaJoinSeq: number;

  beforeAll(async () => {
    w = await startWorker({ dist, script: SCRIPT, classes });
    E = (await w.mint({ app: "chat", label: "room test" })).id;
  });
  afterAll(async () => {
    for (const m of members) m.stop();
    await w?.dispose();
    rmSync(state, { recursive: true, force: true });
  });

  const member = (nick: string, extra: string[]) => {
    const c = runChat(w.port, ["chat", E, "--nick", nick, "--state-dir", state, ...extra]);
    members.push(c);
    return c;
  };
  /** A delivered event (no `hist`) that `pred` accepts. */
  const live = (pred: (ev: Record<string, unknown>) => boolean) => (l: string) => {
    const ev = json(l);
    return ev !== null && ev.hist === undefined && pred(ev);
  };
  const status = async () => {
    const s = await w.status(E);
    if (!s) throw new Error("no status");
    return s;
  };
  /** `room_delivery` events by `via` and outcome. */
  const deliveries = () => {
    const lines = w.events("room_delivery");
    const count = (re: RegExp) => lines.filter((l) => re.test(l)).length;
    return {
      dial: count(/"via":"dial"|via: 'dial'/),
      open: count(/"via":"open"|via: 'open'/),
      failed: count(/"ok":false|ok: false/),
    };
  };

  it("two listeners: bo's join is delivered to ana", async () => {
    ana = member("ana", ["--listen-for", "120"]);
    const anaReady = json(await ana.take((l) => json(l)?.t === "join" && json(l)?.nick === "ana", 60_000));
    expect(anaReady?.hist).toBe(true);
    anaJoinSeq = Number(anaReady?.seq);
    bo = member("bo", ["--listen-for", "120"]);
    await bo.take((l) => json(l)?.t === "join" && json(l)?.nick === "bo", 60_000);
    await ana.take(
      live((ev) => ev.t === "join" && ev.nick === "bo"),
      20_000,
    );
  });

  it("Warm with both members present, then a prompt forced eviction", async () => {
    const warm = await poll(status, (s) => s.state === "Warm" && s.tasks === 0, { timeoutMs: 60_000, what: "Warm" });
    expect(warm).toMatchObject({ members: 2, present: 2 });
    expect(warm.sockets).toHaveLength(2);
    const t0 = Date.now();
    await w.mf.unsafeEvictDurableObject(SCRIPT, "Chat", { name: E, webSockets: "hibernate" });
    expect(Date.now() - t0).toBeLessThan(5_000);
    const woken = await status();
    expect(woken.wakes).toBe(Number(warm.wakes) + 1);
    expect([...(woken.sockets as string[])].sort()).toEqual([...(warm.sockets as string[])].sort());
    expect(woken.present).toBe(2);
  });

  let before: ReturnType<typeof deliveries>;
  let d1: ReturnType<typeof deliveries>;

  it("deliveries dial the members back", async () => {
    before = deliveries();
    const text1 = 'after the eviction: "quoted" é';
    const cy1 = member("cy", ["--say", text1]);
    expect(json(await cy1.take((l) => json(l)?.t === "msg", 60_000))).toMatchObject({ text: text1, nick: "cy" });
    await cy1.exited;
    const a1 = json(
      await ana.take(
        live((ev) => ev.t === "msg" && ev.text === text1),
        20_000,
      ),
    );
    const b1 = json(
      await bo.take(
        live((ev) => ev.t === "msg" && ev.text === text1),
        20_000,
      ),
    );
    expect(a1?.seq).toBe(b1?.seq);
    d1 = await poll(deliveries, (d) => d.dial >= before.dial + 2, { what: "two dial-back deliveries" });
    expect(d1.failed).toBe(before.failed);
  });

  it("the next deliveries, within t_idle, reuse the dialed connections", async () => {
    const text2 = "second line, within t_idle";
    const cy2 = member("cy", ["--say", text2]);
    await cy2.take((l) => json(l)?.t === "msg", 60_000);
    await cy2.exited;
    await ana.take(
      live((ev) => ev.t === "msg" && ev.text === text2),
      20_000,
    );
    await bo.take(
      live((ev) => ev.t === "msg" && ev.text === text2),
      20_000,
    );
    const d2 = await poll(deliveries, (d) => d.open >= d1.open + 2, { what: "two deliveries over open connections" });
    expect(d2.failed).toBe(before.failed);
  });

  it("every member saw the same events, chained by prev", async () => {
    const chain = ana.lines
      .map(json)
      .filter(
        (ev): ev is Record<string, unknown> => ev !== null && ev.seq !== undefined && Number(ev.seq) > anaJoinSeq,
      );
    expect(chain.length).toBeGreaterThan(0);
    for (let i = 1; i < chain.length; i++) expect(chain[i]?.prev).toBe(chain[i - 1]?.seq);
    expect(await status()).toMatchObject({ members: 3, present: 2 });
  });
});
