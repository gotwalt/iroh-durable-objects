// Connection lifetime and stream accounting against the stock test peer (README.md,
// "Connection lifetime"; design note 013):
//
// - A handler that returns closes its connection: code 0, reason `done`, at once.
// - Stream halves the app abandoned but still references stop counting when their
//   connection closes, and the object reaches Warm.
// - A handler that keeps its promise pending keeps the connection (echo).
// - A dialed connection is not closed when the dialing call returns; the idle close ends it.
// - B-M2: a half whose read failed (the peer reset the stream) or whose write failed (the peer
//   stopped it) no longer counts as open: the connection is closed as idle after t_idle, not
//   as silent after t_silent.
import { fileURLToPath } from "node:url";
import { afterMs, poll, startPeer, startWorker, type TestPeer, type TestWorker } from "iroh-do-testing";
import { cargoBin, nativeWrapper } from "iroh-do-testing/repo";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

const dist = fileURLToPath(new URL("../fixture/dist", import.meta.url));
const T_IDLE_MS = 3_000;
const T_SILENT_MS = 30_000;

describe("connection lifetime", () => {
  let w: TestWorker;
  let peer: TestPeer;
  let E: string;

  beforeAll(async () => {
    w = await startWorker({
      dist,
      script: "iroh-do-test",
      classes: { ENDPOINTS: "Probe" },
      vars: { T_IDLE_MS, T_SILENT_MS },
    });
    E = (await w.mint()).id;
    peer = await startPeer(cargoBin("iroh-do-test-peer"), E, w.port, nativeWrapper);
  });
  afterAll(async () => {
    peer?.stop();
    await w?.dispose();
  });

  /** The fixture's RPC methods on `E`. */
  const probe = () => w.object<{ dial(p: string): Promise<string>; kept(): Promise<number> }>("ENDPOINTS", E);

  const status = async () => {
    const s = await w.status(E);
    if (!s) throw new Error("no status");
    return s;
  };

  it("a held connection echoes", async () => {
    expect(await peer.run("echo hello")).toMatch(/^echo: hello /);
  });

  it("a settled handler closes its connection with code 0, reason done, at once", async () => {
    const line = await peer.run("send x iroh-do-test/done/0");
    expect(line).toMatch(/closed: closed by peer: done \(code 0\)/);
    expect(afterMs(line)).toBeLessThan(2_000);
  });

  it("abandoned halves the app still references stop counting when the connection closes", async () => {
    const line = await peer.run("send x iroh-do-test/keep/0");
    expect(line).toMatch(/closed by peer: done \(code 0\)/);
    expect(await (await probe()).kept()).toBe(1);
    const s = await poll(status, (s) => s.connections === 0, { what: "no connection" });
    expect(s.open_streams).toBe(0);
    const warm = await poll(status, (s) => s.state === "Warm" && s.tasks === 0, { what: "Warm" });
    expect(warm.tasks).toBe(0);
  });

  it("B-M2: a read that fails because the peer reset the stream releases its half", async () => {
    const line = await peer.run("reset iroh-do-test/read/0");
    expect(line).toMatch(/closed by peer: idle \(code 0\)/);
    expect(afterMs(line)).toBeGreaterThanOrEqual(T_IDLE_MS - 500);
    expect(afterMs(line)).toBeLessThan(T_SILENT_MS / 2);
  });

  it("B-M2: a write that fails because the peer stopped the stream releases its half", async () => {
    const line = await peer.run("stop iroh-do-test/write/0");
    expect(line).toMatch(/closed by peer: idle \(code 0\)/);
    expect(afterMs(line)).toBeLessThan(T_SILENT_MS / 2);
  });

  it("a stream the peer leaves open counts: closed as silent after t_silent", async () => {
    const line = await peer.run("silent", T_SILENT_MS + 30_000);
    expect(line).toMatch(/closed by peer: silent \(code 0\)/);
    expect(afterMs(line)).toBeGreaterThanOrEqual(T_SILENT_MS - 500);
  });

  it("a dialed connection outlives the dialing call; the idle close ends it", async () => {
    await poll(status, (s) => s.state === "Warm", { what: "Warm" });
    expect(await (await probe()).dial(peer.id)).toBe("ok:back");
    const s = await status();
    expect([s.state, s.connections]).toEqual(["Active", 1]);
    const back = await peer.take((l) => l.startsWith("back"), T_IDLE_MS + 30_000);
    expect(back).toMatch(/closed by peer: idle \(code 0\)/);
    expect(afterMs(back)).toBeGreaterThanOrEqual(T_IDLE_MS - 500);
  });

  it("logs conn_open and conn_close", () => {
    expect(w.events("conn_open").length).toBeGreaterThanOrEqual(5);
    expect(w.events("conn_close").some((l) => l.includes("done"))).toBe(true);
  });
});
