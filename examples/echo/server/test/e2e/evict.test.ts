// Forced hibernation of the echo app on Miniflare, the idle and silent closes, and the
// eviction marker.
//
// Hibernation is forced with `unsafeEvictDurableObject(..., { webSockets: "hibernate" })`,
// which workerd refuses while the object has pending timers, fetches or events, so a prompt
// success shows that nothing runs in Warm. An eviction with the endpoint bound is simulated by
// restarting Miniflare on the same storage, which must log `evicted_active`.
import { fileURLToPath } from "node:url";
import { afterMs, poll, startPeer, startWorker, type TestPeer, type TestWorker } from "iroh-do-testing";
import { cargoBin, nativeWrapper } from "iroh-do-testing/repo";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

const dist = fileURLToPath(new URL("../../dist", import.meta.url));
const SCRIPT = "iroh-ts";
const CLASS = "Echo";
const T_IDLE_MS = 3_000;
const T_SILENT_MS = 8_000;
const opts = { dist, script: SCRIPT, classes: { ENDPOINTS: CLASS }, vars: { T_IDLE_MS, T_SILENT_MS } };

const Z32 = "ybndrfg8ejkmcpqxot1uwisza345h769";
/** The 32 bytes of a z-base-32 EndpointId. */
function z32Bytes(s: string): number[] {
  const out: number[] = [];
  let bits = 0;
  let value = 0;
  for (const ch of s) {
    value = (value << 5) | Z32.indexOf(ch);
    bits += 5;
    if (bits >= 8) {
      out.push((value >>> (bits - 8)) & 0xff);
      bits -= 8;
    }
  }
  return out.slice(0, 32);
}

describe("hibernation and eviction", () => {
  let w: TestWorker;
  let E: string;
  let peer: TestPeer | undefined;

  beforeAll(async () => {
    w = await startWorker(opts);
    E = (await w.mint()).id;
  });
  afterAll(async () => {
    peer?.stop();
    await w?.dispose();
  });

  const status = async () => {
    const s = await w.status(E);
    if (!s) throw new Error("no status");
    return s;
  };
  const evict = async () => {
    const t0 = Date.now();
    await w.mf.unsafeEvictDurableObject(SCRIPT, CLASS, { name: E, webSockets: "hibernate" });
    return Date.now() - t0;
  };

  it("a socket hibernated mid-handshake keeps its challenge and denies a bad signature", async () => {
    const up = await w.fetch(E, "/relay", {
      headers: { Upgrade: "websocket", "Sec-WebSocket-Protocol": "iroh-relay-v2, iroh-relay-v1" },
    });
    expect([up.status, up.headers.get("sec-websocket-protocol")]).toEqual([101, "iroh-relay-v2"]);
    const ws = (up as unknown as { webSocket: WebSocket }).webSocket;
    const frames: Uint8Array[] = [];
    let closed: number | null = null;
    ws.addEventListener("message", (e) => frames.push(new Uint8Array(e.data as ArrayBuffer)));
    ws.addEventListener("close", (e) => {
      closed = e.code;
    });
    (ws as unknown as { accept(): void }).accept();
    const first = await poll(
      () => frames[0],
      (f) => f !== undefined,
      { what: "the challenge" },
    );
    expect([first?.[0], first?.length]).toEqual([0, 17]);
    expect(await evict()).toBeLessThan(5_000);
    ws.send(new Uint8Array([1, ...z32Bytes(E), 64, ...new Uint8Array(64)]));
    await poll(
      () => closed,
      (c) => c !== null,
      { what: "the close" },
    );
    expect(frames.some((f) => f[0] === 3)).toBe(true);
    expect(closed).toBe(1008);
  });

  it("one relay socket across an echo, a hibernation and a second echo", async () => {
    peer = await startPeer(cargoBin("iroh-do-test-peer"), E, w.port, nativeWrapper);
    expect(await peer.run("echo one")).toMatch(/^echo: one/);
    const warm = await poll(status, (s) => s.state === "Warm" && s.tasks === 0, { what: "Warm" });
    expect(warm.sockets).toHaveLength(1);
    expect(await evict()).toBeLessThan(5_000);
    const woken = await status();
    expect({ wakes: woken.wakes, sockets: woken.sockets, state: woken.state }).toEqual({
      wakes: Number(warm.wakes) + 1,
      sockets: warm.sockets,
      state: "Warm",
    });
    expect(await peer.run("echo two")).toMatch(/^echo: two/);
    const after = await status();
    expect(after.sockets).toEqual(warm.sockets);
    expect(after.endpoint_builds).toBe(Number(warm.endpoint_builds) + 1);
  });

  it("a connection without streams is closed as idle after t_idle", async () => {
    const line = await peer?.run("idle");
    expect(line).toMatch(/idle/);
    expect(afterMs(line ?? "")).toBeGreaterThanOrEqual(T_IDLE_MS - 500);
    expect(afterMs(line ?? "")).toBeLessThan(T_IDLE_MS + 3_000);
  });

  it("a connection with an open stream is closed as silent after t_silent", async () => {
    const line = await peer?.run("silent");
    expect(line).toMatch(/silent/);
    expect(afterMs(line ?? "")).toBeGreaterThanOrEqual(T_SILENT_MS - 500);
    expect(afterMs(line ?? "")).toBeLessThan(T_SILENT_MS + 3_000);
  });

  it("Warm again with nothing open, and hibernates", async () => {
    const s = await poll(status, (s) => s.state === "Warm" && s.tasks === 0, { what: "Warm" });
    expect(s.open_streams).toBe(0);
    expect(await evict()).toBeLessThan(5_000);
    expect(await peer?.run("echo three")).toMatch(/^echo: three/);
    expect(w.events("evicted_active")).toEqual([]);
  });

  it("an eviction with the endpoint bound is logged as evicted_active at the next construction", async () => {
    peer?.send("idle");
    await poll(status, (s) => s.state === "Active" && s.connections === 1, { what: "Active" });
    peer?.stop();
    peer = undefined;
    await w.dispose(true);
    w = await startWorker({ ...opts, persist: w.persistDir });
    const s = await status();
    const ev = w.events("evicted_active");
    expect(ev).toHaveLength(1);
    expect(ev[0]).toContain("since_bind_ms");
    expect(s.state).toBe("Cold");
  });
});
