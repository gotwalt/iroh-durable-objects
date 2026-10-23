// `iroh-do/ctl/1` (PROTOCOL.md) served by Ctl on real storage, with fake connections.
import { describe, expect, it } from "vitest";
import { Ctl } from "../../src/ctl";
import { LIMITS } from "../../src/protocol";
import { asConnection, FakeConnection } from "../fakes/iroh";
import { chatStorage, inObject, testLogger } from "./helpers";

const E = "objectendpointid";
const encoder = new TextEncoder();

function setUp(state: DurableObjectState) {
  const { storage } = chatStorage(state);
  const log = testLogger();
  const dialBacks: [string, number][] = [];
  const ctl = new Ctl({
    storage,
    log,
    endpointId: () => E,
    status: () => ({ id: E, app: "chat", state: "Active" }),
    scheduleDialBack: (peer, delayMs) => dialBacks.push([peer, delayMs]),
  });
  const conn = new FakeConnection("ana");
  const serving = ctl.serve(asConnection(conn));
  return {
    storage,
    log,
    dialBacks,
    conn,
    /** Writes `text` on a new stream and finishes it; resolves with the object's reply text. */
    async exchange(text: string | Uint8Array): Promise<string> {
      const { send, recv } = conn.openBi();
      await send.writeAll(typeof text === "string" ? encoder.encode(text) : text);
      await send.finish();
      return recv.text();
    },
    async done(): Promise<void> {
      conn.closeByPeer();
      await serving;
    },
  };
}

describe("requests", () => {
  it("replies one line per request line, in order, with \\r\\n and a final unterminated line", () =>
    inObject(async (state) => {
      const t = setUp(state);
      const reply = await t.exchange("/count\r\nhello\n/log 1");
      expect(reply.split("\n")).toEqual([
        '{"total":0,"yours":0,"peers":0}',
        "echo: hello (#1, total 1, yours 1)",
        expect.stringMatching(/^\[\{"seq":1,"ts_ms":\d+,"peer":"ana","kind":"ctl","text":"hello"\}\]$/),
        "",
      ]);
      await t.done();
    }));

  it("reports the status, and schedules a dial-back for the asking peer", () =>
    inObject(async (state) => {
      const t = setUp(state);
      const reply = await t.exchange("/status\n/callback 3\n/callback 51\n");
      expect(reply.split("\n")).toEqual([
        '{"id":"objectendpointid","app":"chat","state":"Active"}',
        "ok: callback in 3s",
        `error: usage /callback <secs>, secs <= ${LIMITS.maxCallbackSecs}`,
        "",
      ]);
      expect(t.dialBacks).toEqual([["ana", 3000]]);
      // None of these is logged.
      expect(t.storage.totalRecords()).toBe(0);
      await t.done();
    }));

  it("writes the unprompted hello once per connection, after the first reply", () =>
    inObject(async (state) => {
      const t = setUp(state);
      expect(t.conn.unis).toHaveLength(0);
      await t.exchange("/count\n");
      await t.exchange("/count\n");
      expect(await t.conn.nextUni()).toBe(`hello from ${E}: you are ana\n`);
      expect(t.conn.unis).toHaveLength(1);
      await t.done();
    }));
});

describe("errors", () => {
  it("answers a line that is too long with an error and ends the stream", () =>
    inObject(async (state) => {
      const t = setUp(state);
      const reply = await t.exchange(`${"x".repeat(LIMITS.ctlMaxLine + 1)}\n/count\n`);
      expect(reply).toBe("error: line too long\n");
      expect(t.storage.totalRecords()).toBe(0);
      await t.done();
    }));

  it("answers a line that is not UTF-8 with an error and ends the stream", () =>
    inObject(async (state) => {
      const t = setUp(state);
      expect(await t.exchange(new Uint8Array([0xc3, 0x28, 0x0a]))).toBe("error: line is not utf-8\n");
      await t.done();
    }));

  it("answers a request whose storage fails with an error and ends the stream", () =>
    inObject(async (state) => {
      const t = setUp(state);
      state.storage.sql.exec("CREATE TRIGGER refuse BEFORE INSERT ON log BEGIN SELECT RAISE(ABORT, 'refused'); END");
      const reply = await t.exchange("hello\n/count\n");
      expect(reply).toMatch(/^error: .*refused.*\n$/);
      expect(t.log.events("ctl_request_failed")).toHaveLength(1);
      // The connection still serves new streams.
      state.storage.sql.exec("DROP TRIGGER refuse");
      expect(await t.exchange("/count\n")).toBe('{"total":0,"yours":0,"peers":0}\n');
      await t.done();
    }));
});
