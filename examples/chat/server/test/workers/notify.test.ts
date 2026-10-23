// Dial-backs over `iroh-do/notify/1` (PROTOCOL.md) by DialBack on real storage, with a fake
// endpoint.
import { describe, expect, it } from "vitest";
import { DialBack } from "../../src/notify";
import { NOTIFY_ALPN } from "../../src/protocol";
import { asEndpoint, FakeEndpoint } from "../fakes/iroh";
import { chatStorage, inObject, testLogger } from "./helpers";

const E = "objectendpointid";

function setUp(state: DurableObjectState) {
  const { storage } = chatStorage(state);
  const endpoint = new FakeEndpoint(E);
  const log = testLogger();
  const dialBack = new DialBack({
    storage,
    log,
    endpoint: () => asEndpoint(endpoint),
    endpointId: () => E,
    wakes: () => 7,
  });
  const callbacks = () => storage.recentRecords(10).filter((r) => r.kind === "callback");
  return { storage, endpoint, log, dialBack, callbacks };
}

describe("dial-back", () => {
  it("writes the notify line on one stream and records it, without waiting for the peer", () =>
    inObject(async (state) => {
      const t = setUp(state);
      t.storage.append({ peer: "ana", kind: "ctl", nick: "", text: "x" }, Date.now());
      t.endpoint.present.add("ana");
      await t.dialBack.run("ana");
      const conn = await t.endpoint.dialed("ana", NOTIFY_ALPN);
      // run() has returned while the connection is still open for the peer to read.
      expect(conn.closeReason()).toBeNull();
      expect(await conn.nextUni()).toBe(`callback from ${E} wakes=7 records=1\n`);
      expect(conn.unis).toHaveLength(1);
      expect(t.callbacks()).toMatchObject([{ peer: "ana", text: "delivered" }]);
      expect(t.log.events("dial_back")).toMatchObject([{ peer: "ana", ok: true }]);
      // The peer closes once it has read the line.
      conn.closeByPeer();
    }));

  it("records a failed dial and resolves", () =>
    inObject(async (state) => {
      const t = setUp(state);
      await t.dialBack.run("ana");
      expect(t.endpoint.dials).toMatchObject([{ peer: "ana", alpn: NOTIFY_ALPN }]);
      expect(t.callbacks()).toMatchObject([{ peer: "ana", text: "failed: no relay socket for ana" }]);
      expect(t.log.events("dial_back")).toMatchObject([{ peer: "ana", ok: false }]);
    }));

  it("records a failed stream and closes the connection", () =>
    inObject(async (state) => {
      const t = setUp(state);
      t.endpoint.present.add("ana");
      t.endpoint.onDial = (conn) => {
        conn.openUniError = new Error("stream refused");
      };
      await t.dialBack.run("ana");
      const conn = await t.endpoint.dialed("ana", NOTIFY_ALPN);
      expect(conn.closeReason()).toMatch(/^closed by object/);
      expect(t.callbacks()).toMatchObject([{ text: "failed: stream refused" }]);
    }));

  it("resolves when the record cannot be stored, so that the peer is not notified twice", () =>
    inObject(async (state) => {
      const t = setUp(state);
      t.endpoint.present.add("ana");
      state.storage.sql.exec("CREATE TRIGGER refuse BEFORE INSERT ON log BEGIN SELECT RAISE(ABORT, 'refused'); END");
      await expect(t.dialBack.run("ana")).resolves.toBeUndefined();
      const conn = await t.endpoint.dialed("ana", NOTIFY_ALPN);
      expect(await conn.nextUni()).toMatch(/^callback from /);
      expect(t.log.events("dial_back_record_failed")).toHaveLength(1);
      conn.closeByPeer();
    }));
});
