// `iroh-do/room/1` (PROTOCOL.md) served by Room on real storage, with fake connections. The
// wire behavior of single requests is covered by the golden fixtures; these tests cover what
// the fixtures cannot reach: deliveries (batching, reuse, dial-back, /quit, failures, the
// queue bound) and how failures are answered.
import { describe, expect, it } from "vitest";
import { LIMITS, ROOM_ALPN, ROOM_ERRORS } from "../../src/protocol";
import { Room } from "../../src/room";
import { asConnection, asEndpoint, FakeConnection, FakeEndpoint } from "../fakes/iroh";
import { chatStorage, eventually, inObject, sleep, testLogger } from "./helpers";

const E = "objectendpointid";

type Line = Record<string, unknown> & { t: string };

const parse = (text: string): Line[] =>
  text
    .split("\n")
    .filter((l) => l !== "")
    .map((l) => JSON.parse(l) as Line);

/** Long enough for a delivery batch to be written. */
const AFTER_BATCH = LIMITS.deliveryBatchMs * 3;

function setUp(state: DurableObjectState) {
  const { storage } = chatStorage(state);
  const endpoint = new FakeEndpoint(E);
  const log = testLogger();
  const room = new Room({ storage, endpoint: () => asEndpoint(endpoint), endpointId: () => E, log });
  const conns: FakeConnection[] = [];
  const serving: Promise<void>[] = [];
  return {
    state,
    storage,
    endpoint,
    log,
    room,
    /** A member that dialed the object and holds a relay socket to it, unless `absent`. */
    dial(peer: string, { absent = false } = {}): FakeConnection {
      if (!absent) endpoint.present.add(peer);
      const conn = new FakeConnection(peer);
      conns.push(conn);
      serving.push(room.serve(asConnection(conn)));
      return conn;
    },
    /** One request on `conn`; its reply lines. */
    async ask(conn: FakeConnection, request: string): Promise<Line[]> {
      return parse(await conn.request(request));
    },
    /** Closes every connection, so that the room's tasks end with the test. */
    async done(): Promise<void> {
      for (const d of endpoint.dials) if (d.conn !== undefined) conns.push(d.conn);
      for (const conn of conns) conn.closeByPeer();
      await Promise.all(serving);
      await sleep(AFTER_BATCH);
    },
  };
}

type Setup = ReturnType<typeof setUp>;

/** Members that dialed the object, are present, and have joined with their names as nicks. */
async function joined(t: Setup, ...peers: string[]): Promise<FakeConnection[]> {
  const conns = [];
  for (const peer of peers) {
    const conn = t.dial(peer);
    const reply = await t.ask(conn, `/join ${peer}`);
    expect(reply.at(-1)?.t).toBe("ready");
    conns.push(conn);
  }
  // The joins are delivered to the members that joined before; wait them out.
  await sleep(AFTER_BATCH);
  for (const conn of conns) conn.unis.splice(0);
  return conns;
}

/** Waits a batch and returns the event lines of every delivery stream `conn` got, in order. */
async function delivered(conn: FakeConnection): Promise<Line[]> {
  await sleep(AFTER_BATCH);
  const texts = await Promise.all(conn.unis.map((u) => u.text()));
  return texts.flatMap(parse);
}

describe("requests", () => {
  it("join replies with the history and ready; joining again stores nothing", () =>
    inObject(async (state) => {
      const t = setUp(state);
      const ana = t.dial("ana");
      const first = await t.ask(ana, "/join ana");
      expect(first).toEqual([
        { t: "join", seq: 1, prev: 0, ts: expect.any(Number), peer: "ana", nick: "ana", hist: true },
        { t: "ready", room: E, you: { peer: "ana", nick: "ana" }, last_seq: 1 },
      ]);
      const again = await t.ask(ana, "/join");
      expect(again.at(-1)).toEqual({ t: "ready", room: E, you: { peer: "ana", nick: "ana" }, last_seq: 1 });
      expect(t.storage.totalRecords()).toBe(1);
      expect(t.log.events("room_request").map((f) => f.nick)).toEqual(["ana", "ana"]);
      await t.done();
    }));

  it("framing errors are answered before the rate limit and do not count against it", () =>
    inObject(async (state) => {
      const t = setUp(state);
      const [ana] = await joined(t, "ana");
      if (ana === undefined) throw new Error("no member");
      for (let i = 0; i < 12; i++) {
        expect(await t.ask(ana, "x".repeat(LIMITS.maxRequest + 1))).toEqual([{ t: "err", text: ROOM_ERRORS.tooLong }]);
      }
      // The join was the first request of the window.
      for (let i = 0; i < LIMITS.rateLimitRequests - 1; i++) {
        expect((await t.ask(ana, "/who"))[0]?.t).toBe("who");
      }
      expect(await t.ask(ana, "/who")).toEqual([{ t: "err", text: ROOM_ERRORS.rateLimited }]);
      await t.done();
    }));

  it("answers storage error when the write fails, and the request stored nothing", () =>
    inObject(async (state) => {
      const t = setUp(state);
      const [ana] = await joined(t, "ana");
      if (ana === undefined) throw new Error("no member");
      state.storage.sql.exec("CREATE TRIGGER refuse BEFORE INSERT ON log BEGIN SELECT RAISE(ABORT, 'refused'); END");
      expect(await t.ask(ana, "/say lost")).toEqual([{ t: "err", text: ROOM_ERRORS.storage }]);
      state.storage.sql.exec("DROP TRIGGER refuse");
      expect((await t.ask(ana, "/history")).map((l) => l.t)).toEqual(["join"]);
      expect(t.log.events("room_request_failed")).toHaveLength(1);
      await t.done();
    }));

  it("answers ok once the event is stored, even if queueing it for delivery fails", () =>
    inObject(async (state) => {
      const t = setUp(state);
      const [ana, bo] = await joined(t, "ana", "bo");
      if (ana === undefined || bo === undefined) throw new Error("no member");
      t.endpoint.peersError = new Error("peers failed");
      const reply = await t.ask(ana, "/say kept");
      t.endpoint.peersError = undefined;
      expect(reply).toEqual([{ t: "ok", seq: 3, prev: 2 }]);
      expect((await t.ask(ana, "/history 1"))[0]).toMatchObject({ t: "msg", seq: 3, text: "kept" });
      expect(t.log.events("room_queue_failed")).toHaveLength(1);
      expect(await delivered(bo)).toEqual([]);
      await t.done();
    }));

  it("resets the request stream when handling fails other than by a failed write", () =>
    inObject(async (state) => {
      const t = setUp(state);
      const [ana] = await joined(t, "ana");
      if (ana === undefined) throw new Error("no member");
      t.endpoint.peersError = new Error("peers failed");
      await expect(ana.request("/who")).rejects.toThrow(/reset/);
      t.endpoint.peersError = undefined;
      expect(t.log.events("room_request_failed")).toMatchObject([{ peer: "ana", kind: "who" }]);
      // The connection and the member's next request are unaffected.
      expect((await t.ask(ana, "/who"))[0]?.t).toBe("who");
      await t.done();
    }));
});

describe("deliveries", () => {
  it("delivers an event to the other joined, present members, not to its author", () =>
    inObject(async (state) => {
      const t = setUp(state);
      const [ana, bo, cy] = await joined(t, "ana", "bo", "cy");
      if (ana === undefined || bo === undefined || cy === undefined) throw new Error("no member");
      const [ok] = await t.ask(bo, "/say hi");
      const event = {
        t: "msg",
        seq: ok?.seq,
        prev: ok?.prev,
        ts: expect.any(Number),
        peer: "bo",
        nick: "bo",
        text: "hi",
      };
      expect(await delivered(ana)).toEqual([event]);
      expect(await delivered(cy)).toEqual([event]);
      expect(await delivered(bo)).toEqual([]);
      await t.done();
    }));

  it("writes the events queued within the batch delay on one stream, in seq order", () =>
    inObject(async (state) => {
      const t = setUp(state);
      const [ana, bo] = await joined(t, "ana", "bo");
      if (ana === undefined || bo === undefined) throw new Error("no member");
      await Promise.all([t.ask(bo, "/say one"), t.ask(bo, "/me two"), t.ask(bo, "/say three")]);
      const lines = await delivered(ana);
      expect(ana.unis).toHaveLength(1);
      expect(lines.map((l) => l.seq)).toEqual([3, 4, 5]);
      await t.done();
    }));

  it("does not deliver to a joined member that is not present, and who says so", () =>
    inObject(async (state) => {
      const t = setUp(state);
      const [ana, bo] = await joined(t, "ana", "bo");
      if (ana === undefined || bo === undefined) throw new Error("no member");
      ana.closeByPeer();
      t.endpoint.present.delete("ana");
      await t.ask(bo, "/say anyone?");
      await sleep(AFTER_BATCH);
      expect(t.endpoint.dials).toEqual([]);
      expect(t.log.events("room_event").at(-1)).toMatchObject({ recipients: 0 });
      expect(await t.ask(bo, "/who")).toEqual([
        {
          t: "who",
          members: [
            { peer: "ana", nick: "ana", present: false },
            { peer: "bo", nick: "bo", present: true },
          ],
        },
      ]);
      await t.done();
    }));

  it("reuses a member's open connection, else dials it back and serves requests on that connection", () =>
    inObject(async (state) => {
      const t = setUp(state);
      const [ana, bo] = await joined(t, "ana", "bo");
      if (ana === undefined || bo === undefined) throw new Error("no member");
      await t.ask(bo, "/say on the open connection");
      expect((await delivered(ana)).map((l) => l.text)).toEqual(["on the open connection"]);
      expect(t.endpoint.dials).toEqual([]);

      // ana's connection closes; it still holds its relay socket.
      ana.closeByPeer();
      await t.ask(bo, "/say dialed");
      const back = await t.endpoint.dialed("ana", ROOM_ALPN);
      expect((await delivered(back)).map((l) => l.text)).toEqual(["dialed"]);
      // ana sends its next request on the connection the object dialed.
      expect(await t.ask(back, "/say over the dialed connection")).toEqual([{ t: "ok", seq: 5, prev: 4 }]);
      await t.ask(bo, "/say again");
      expect((await delivered(back)).map((l) => l.text)).toEqual(["dialed", "again"]);
      expect(t.endpoint.dials).toHaveLength(1);
      await t.done();
    }));

  it("drops the queued deliveries of a member that quits, and delivers its part to the others", () =>
    inObject(async (state) => {
      const t = setUp(state);
      const [ana, bo, cy] = await joined(t, "ana", "bo", "cy");
      if (ana === undefined || bo === undefined || cy === undefined) throw new Error("no member");
      // Within the batch delay: bo's line is still queued for ana when ana quits.
      const [said] = await t.ask(bo, "/say before you go");
      await t.ask(ana, "/quit bye");
      expect(await delivered(ana)).toEqual([]);
      expect((await delivered(cy)).map((l) => [l.t, l.seq])).toEqual([
        ["msg", said?.seq],
        ["part", (said?.seq as number) + 1],
      ]);
      expect((await delivered(bo)).map((l) => [l.t, l.reason])).toEqual([["part", "bye"]]);
      await t.done();
    }));

  it("does not retry a failed delivery, and later events still reach the member", () =>
    inObject(async (state) => {
      const t = setUp(state);
      const [ana, bo] = await joined(t, "ana", "bo");
      if (ana === undefined || bo === undefined) throw new Error("no member");
      ana.closeByPeer();
      // The delivery fails unexpectedly: listing the present peers throws when it dials.
      await t.ask(bo, "/say lost");
      t.endpoint.peersError = new Error("peers failed");
      await sleep(AFTER_BATCH);
      t.endpoint.peersError = undefined;
      expect(t.endpoint.dials).toEqual([]);

      await t.ask(bo, "/say found");
      const back = await t.endpoint.dialed("ana", ROOM_ALPN);
      const lines = await delivered(back);
      expect(lines.map((l) => l.text)).toEqual(["found"]);
      // ana sees the gap: the event before `found` never arrived.
      expect(lines[0]?.prev).toBe(3);
      await t.done();
    }));

  it("keeps at most SINCE_MAX lines for a member that does not read, dropping the oldest", () =>
    inObject(async (state) => {
      const t = setUp(state);
      const [ana] = await joined(t, "ana");
      if (ana === undefined) throw new Error("no member");
      const release: (() => void)[] = [];
      ana.stallUnis = new Promise<void>((resolve) => release.push(resolve));
      // The first delivery to ana starts and does not finish.
      await t.ask(t.dial("first"), "/join first");
      await eventually(() => ana.unis.length === 1);
      // Enough events from enough members (the rate limit is per member) to pass the bound
      // while that delivery is stuck.
      const senders = Math.ceil((LIMITS.sinceMax + 50) / LIMITS.rateLimitRequests);
      for (let i = 0; i < senders; i++) {
        const peer = `m${i}`;
        const conn = t.dial(peer);
        await t.ask(conn, `/join ${peer}`);
        await Promise.all(Array.from({ length: LIMITS.rateLimitRequests - 1 }, () => t.ask(conn, "/say x")));
      }
      for (const resolve of release) resolve();
      await eventually(() => ana.unis.length === 2 && ana.unis[1]?.state === "finished");
      const [first, second] = await Promise.all(ana.unis.map(async (u) => parse(await u.text())));
      expect(second).toHaveLength(LIMITS.sinceMax);
      // The newest events arrived; the gap after the first delivery shows in prev.
      expect(second?.at(-1)?.seq).toBe(t.storage.roomSeq());
      expect(second?.[0]?.prev).not.toBe(first?.at(-1)?.seq);
      const toAna = t.log.events("room_delivery").filter((f) => f.peer === "ana");
      expect(toAna.at(-1)).toMatchObject({ events: LIMITS.sinceMax, ok: true });
      expect(toAna.at(-1)?.dropped).toBeGreaterThan(0);
      await t.done();
    }));
});
