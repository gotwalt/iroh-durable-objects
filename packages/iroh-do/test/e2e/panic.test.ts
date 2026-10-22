// Panic recovery (design note 013, "Panics") with the wasm module's test-panic build. All
// objects of a Worker share one isolate in workerd, and with it one wasm instance. Two
// endpoints A and B, each with a stock peer holding a relay socket:
//
// 1. A panics inside a call it makes. A catches the trap, instantiates the module again and
//    rebuilds its node at once.
// 2. B's next event finds its node built for the old instance and rebuilds it from its socket
//    attachments and storage, as a wake does. Its relay socket is the same.
// 3. A panics in a task on a later microtask, where no object catches the trap. The panic
//    hook records it; each object's next event rebuilds.
// 4. A panic while A has a QUIC connection open: the peer learns when its idle timeout
//    fires, and redials over the same relay socket.
import { fileURLToPath } from "node:url";
import { poll, startPeer, startWorker, type TestPeer, type TestWorker } from "iroh-do-testing";
import { cargoBin, nativeWrapper } from "iroh-do-testing/repo";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

const dist = fileURLToPath(new URL("../fixture/dist", import.meta.url));

describe("panic recovery", () => {
  let w: TestWorker;
  let A: string;
  let B: string;
  let pa: TestPeer;
  let pb: TestPeer;
  let socketsA: unknown;
  let socketsB: unknown;
  let wakesB: unknown;

  beforeAll(async () => {
    w = await startWorker({ dist, script: "iroh-do-test", classes: { ENDPOINTS: "Probe" }, vars: { T_IDLE_MS: 3000 } });
    A = (await w.mint()).id;
    B = (await w.mint()).id;
    pa = await startPeer(cargoBin("iroh-do-test-peer"), A, w.port, nativeWrapper);
    pb = await startPeer(cargoBin("iroh-do-test-peer"), B, w.port, nativeWrapper);
  });
  afterAll(async () => {
    pa?.stop();
    pb?.stop();
    await w?.dispose();
  });

  const status = async (id: string) => {
    const s = await w.status(id);
    if (!s) throw new Error(`no status for ${id}`);
    return s;
  };
  const panic = async (id: string, inTask: boolean) => {
    const stub = await w.object<{ irohTestPanic(t: boolean): Promise<void> }>("ENDPOINTS", id);
    // A panic in the call is turned into a normal return by the object; one in a task too.
    await stub.irohTestPanic(inTask);
  };

  it("both echo on generation 0", async () => {
    expect(await pa.run("echo a0")).toMatch(/^echo: a0/);
    expect(await pb.run("echo b0")).toMatch(/^echo: b0/);
    const [sa, sb] = [await status(A), await status(B)];
    expect([sa.generation, sb.generation]).toEqual([0, 0]);
    socketsA = sa.sockets;
    socketsB = sb.sockets;
    wakesB = sb.wakes;
  });

  it("a panic in A's call: A rebuilds on generation 1 with the same socket", async () => {
    await panic(A, false);
    const sa = await status(A);
    expect({ generation: sa.generation, rebuilds: sa.rebuilds, sockets: sa.sockets }).toEqual({
      generation: 1,
      rebuilds: 1,
      sockets: socketsA,
    });
    expect(w.events("wasm_panic")).toHaveLength(1);
    expect(w.events("wasm_reset")).toHaveLength(1);
  });

  it("B rebuilds on its next event, keeping its socket and its wake count", async () => {
    const sb = await status(B);
    expect({ generation: sb.generation, rebuilds: sb.rebuilds, sockets: sb.sockets, wakes: sb.wakes }).toEqual({
      generation: 1,
      rebuilds: 1,
      sockets: socketsB,
      wakes: wakesB,
    });
    expect(await pb.run("echo b1")).toMatch(/^echo: b1/);
    expect(await pa.run("echo a1")).toMatch(/^echo: a1/);
  });

  it("a panic in a task is recorded by the hook, and both rebuild on their next event", async () => {
    await poll(
      () => status(A),
      (s) => s.state === "Warm",
      { what: "A Warm" },
    );
    await panic(A, true);
    await poll(
      () => w.events("wasm_panic").length,
      (n) => n === 2,
      { what: "the second wasm_panic" },
    );
    expect(w.events("wasm_panic")[1]).toContain("in a task");
    // B's peer first: B's message event performs the reset.
    expect(await pb.run("echo b2")).toMatch(/^echo: b2/);
    expect(await pa.run("echo a2")).toMatch(/^echo: a2/);
    const [sa, sb] = [await status(A), await status(B)];
    expect([sa.generation, sb.generation, sa.sockets, sb.sockets]).toEqual([2, 2, socketsA, socketsB]);
    expect(w.events("node_rebuild")).toHaveLength(4);
  });

  it("a panic with a connection open: the peer times out, then redials over the same socket", async () => {
    pa.send("idle");
    await poll(
      () => status(A),
      (s) => s.state === "Active" && s.connections === 1,
      { what: "A Active" },
    );
    await panic(A, true);
    const sa = await poll(
      () => status(A),
      (s) => s.generation === 3,
      { what: "A on generation 3" },
    );
    expect(sa.connections).toBe(0);
    const lost = await pa.take((l) => l.startsWith("closed"), 90_000);
    expect(lost).toMatch(/timed out/);
    expect(await pa.run("echo a3")).toMatch(/^echo: a3/);
    expect((await status(A)).sockets).toEqual(socketsA);
  });
});
