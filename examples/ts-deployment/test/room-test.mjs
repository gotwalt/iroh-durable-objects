// The chat room in Miniflare with two present members, across a forced hibernation.
//
//   npm run build && cargo build -p iroh-do-chat && node test/room-test.mjs
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
import { check, fail, finish, sleep, startWorker } from "iroh-do/testing";
import { json, runChat } from "./clients.mjs";

const dist = join(fileURLToPath(import.meta.url), "..", "..", "dist");
const port = Number(process.env.PORT ?? 8832);
const SCRIPT = "iroh-ts";
const classes = { ENDPOINTS: "Echo", CHAT: "Chat", REGISTRY: "Registry" };
const state = mkdtempSync(join(tmpdir(), "iroh-do-room-test-"));

const w = startWorker({ dist, script: SCRIPT, classes, port });
const members = [];
try {
  await w.mf.ready;
  const E = (await w.mint({ app: "chat", label: "room test" })).id;
  const member = (nick, extra) => {
    const c = runChat(port, ["chat", E, "--nick", nick, "--state-dir", state, ...extra]);
    members.push(c);
    return c;
  };
  const live = (pred) => (l) => {
    const ev = json(l);
    return ev !== null && ev.hist === undefined && pred(ev);
  };
  const chatStats = async () => {
    const ns = await w.mf.getDurableObjectNamespace("CHAT", SCRIPT);
    return ns.get(ns.idFromName(E)).chatStats();
  };

  // 1. Two listeners.
  const ana = member("ana", ["--listen-for", "120"]);
  const anaReady = await ana.waitFor((l) => json(l)?.t === "join" && json(l)?.nick === "ana", 60_000);
  check("ana joined (her join is in the /join history)", json(anaReady).hist === true, anaReady);
  const bo = member("bo", ["--listen-for", "120"]);
  await bo.waitFor((l) => json(l)?.t === "join" && json(l)?.nick === "bo", 60_000);
  const boJoin = await ana.waitFor(live((ev) => ev.t === "join" && ev.nick === "bo"), 20_000);
  check("bo's join delivered live to ana", true, boJoin);

  // 2. Warm, then forced hibernation.
  let s = null;
  for (let i = 0; i < 40; i++) {
    s = await w.status(E);
    if (s.state === "Warm" && s.tasks === 0) break;
    await sleep(1_000);
  }
  check("Warm with both members present, no tasks", s.state === "Warm" && s.tasks === 0 && s.members === 2 && s.present === 2 && s.sockets.length === 2,
    `state=${s.state} tasks=${s.tasks} members=${s.members} present=${s.present} sockets=${s.sockets.length}`);
  const t0 = Date.now();
  let evicted = null;
  try {
    await w.mf.unsafeEvictDurableObject(SCRIPT, "Chat", { name: E, webSockets: "hibernate" });
    evicted = Date.now() - t0;
  } catch (e) {
    check("forced eviction", false, e.message);
  }
  check("forced eviction in Warm succeeds promptly", evicted !== null && evicted < 5_000, `${evicted} ms`);
  const s2 = await w.status(E);
  check("woken object: wakes +1, same two sockets, both present",
    s2.wakes === s.wakes + 1 && JSON.stringify([...s2.sockets].sort()) === JSON.stringify([...s.sockets].sort()) && s2.present === 2,
    `wakes ${s.wakes} -> ${s2.wakes}, sockets ${s2.sockets.length}, present ${s2.present}`);
  const fresh = await chatStats();
  check("new construction: no deliveries yet", fresh.deliveries_ok === 0 && fresh.delivery_dials === 0, JSON.stringify(fresh));

  // 3. Deliveries after the eviction.
  const text1 = "after the eviction: \"quoted\" é";
  const cy1 = member("cy", ["--say", text1]);
  const said = await cy1.waitFor((l) => json(l)?.t === "msg", 60_000);
  check("cy's --say printed the stored message", json(said).text === text1 && json(said).nick === "cy", said);
  await cy1.exited;
  const a1 = await ana.waitFor(live((ev) => ev.t === "msg" && ev.text === text1), 20_000);
  const b1 = await bo.waitFor(live((ev) => ev.t === "msg" && ev.text === text1), 20_000);
  check("ana and bo got the line delivered by dial-back", json(a1).seq === json(b1).seq && json(a1).nick === "cy", `${a1} | ${b1}`);
  const st1 = await chatStats();
  check("each delivery dialed the member (no connection was open)", st1.delivery_dials >= 2 && st1.deliveries_failed === 0,
    JSON.stringify(st1));

  const text2 = "second line, within t_idle";
  const cy2 = member("cy", ["--say", text2]);
  await cy2.waitFor((l) => json(l)?.t === "msg", 60_000);
  await cy2.exited;
  await ana.waitFor(live((ev) => ev.t === "msg" && ev.text === text2), 20_000);
  await bo.waitFor(live((ev) => ev.t === "msg" && ev.text === text2), 20_000);
  const st2 = await chatStats();
  check("the next deliveries reuse the dialed connections", st2.delivery_reuses >= st1.delivery_reuses + 2 && st2.deliveries_failed === 0,
    `reuses ${st1.delivery_reuses} -> ${st2.delivery_reuses}, dials ${st1.delivery_dials} -> ${st2.delivery_dials}`);
  const via = w.events("room_delivery").map((l) => /via: '(\w+)'/.exec(l)?.[1]);
  check("room_delivery events: via dial, then via open", via.includes("dial") && via.includes("open"), via.join(","));

  // Same events in the same order for both members, with prev chaining.
  const events = (c) => c.lines.map(json).filter((ev) => ev && ev.seq !== undefined);
  const chain = events(ana).filter((ev) => ev.seq > json(anaReady).seq);
  check("ana's events chain by prev (no gap)", chain.every((ev, i) => i === 0 || ev.prev === chain[i - 1].seq),
    chain.map((ev) => `${ev.t}#${ev.seq}<-${ev.prev}`).join(" "));
  const s3 = await w.status(E);
  check("status: 3 members, 2 present, 5 records", s3.members === 3 && s3.present === 2 && s3.records === 5,
    `members=${s3.members} present=${s3.present} records=${s3.records}`);
} catch (e) {
  fail(e);
} finally {
  for (const m of members) m.stop();
  await w.dispose();
  rmSync(state, { recursive: true, force: true });
}
finish();
