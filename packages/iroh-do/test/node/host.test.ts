// The wasm library in Node (B-M1): node state is per WasmNode, not per EndpointId, and a node
// whose object is garbage-collected releases its state. Run with --expose-gc
// (vitest.unit.config.ts).
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { beforeAll, describe, expect, it } from "vitest";
import * as glue from "../../pkg/iroh_do_wasm.js";

const pkg = (f: string) => fileURLToPath(new URL(`../../pkg/${f}`, import.meta.url));

beforeAll(() => {
  glue.initSync({ module: readFileSync(pkg("iroh_do_wasm_bg.wasm")) });
});

/** A host that records the sockets it was asked to write to. */
function recordingHost(frames: string[]): glue.NodeHost {
  return {
    sendFrame: (sid) => void frames.push(sid),
    accept: () => Promise.resolve(),
    schedule: () => undefined,
    cancel: () => undefined,
    endpointBuilt: () => undefined,
    endpointClosed: () => undefined,
  };
}

function node(secret: Uint8Array, host: glue.NodeHost): glue.WasmNode {
  return new glue.WasmNode(
    secret,
    { wakes: 1, endpointBuilds: 0, bundle: "", relayUrl: "http://x.localhost/", protocols: [] },
    host,
  );
}

/** Upgrades a relay socket and opens it, which writes the challenge to the host. */
function openSocket(n: glue.WasmNode): string {
  const r = n.upgrade("GET", "/relay", "iroh-relay-v2", Date.now());
  if (!r.ok) throw new Error(r.message);
  n.opened(r.sid);
  return r.sid;
}

async function collect(until: () => boolean): Promise<void> {
  const gc = (globalThis as { gc?: () => void }).gc;
  if (!gc) throw new Error("run with --expose-gc");
  for (let i = 0; i < 50 && !until(); i++) {
    gc();
    // FinalizationRegistry callbacks run on a later task.
    await new Promise((r) => setImmediate(r));
  }
}

describe("node state", () => {
  it("two nodes of one EndpointId do not reach each other's sockets", () => {
    const secret = glue.generateSecretKey();
    const oldFrames: string[] = [];
    const newFrames: string[] = [];
    const before = glue.liveNodes();
    const oldNode = node(secret, recordingHost(oldFrames));
    const newHost = recordingHost(newFrames);
    const newNode = node(secret, newHost);
    expect(glue.liveNodes()).toBe(before + 2);

    const sid = openSocket(newNode);
    expect(newFrames).toEqual([sid]);
    expect(oldFrames).toEqual([]);

    // The old node going away (destroy detaches; the GC frees) leaves the new one intact.
    oldNode.detach();
    oldNode.free();
    expect(glue.liveNodes()).toBe(before + 1);
    const sid2 = openSocket(newNode);
    expect(newFrames).toEqual([sid, sid2]);
    newNode.free();
    expect(glue.liveNodes()).toBe(before);
  });

  it("a node whose object was collected releases its state", async () => {
    const before = glue.liveNodes();
    (() => {
      // The shape of IrohDurableObject: the object holds the node and the host, and the
      // host's callbacks capture the object.
      const object: { node?: glue.WasmNode; host?: glue.NodeHost; big: Uint8Array } = { big: new Uint8Array(1 << 20) };
      object.host = {
        ...recordingHost([]),
        sendFrame: () => void object.big.byteLength,
      };
      object.node = node(glue.generateSecretKey(), object.host);
      openSocket(object.node);
    })();
    expect(glue.liveNodes()).toBe(before + 1);
    await collect(() => glue.liveNodes() === before);
    expect(glue.liveNodes()).toBe(before);
  });

  it("upgrade fails once the node is detached", () => {
    const n = node(glue.generateSecretKey(), recordingHost([]));
    n.detach();
    expect(() => n.upgrade("GET", "/relay", "iroh-relay-v2", Date.now())).toThrow(/detached/);
    n.free();
  });
});
