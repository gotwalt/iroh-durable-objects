// The test app of the e2e tests (test/e2e), bundled with the wasm module's test-panic build
// by build.mjs. Each ALPN exercises one rule of the connection lifetime (README.md,
// "Connection lifetime") or of the stream accounting.
import {
  type BiStream,
  type Connection,
  EndpointId,
  IrohDurableObject,
  irohRouter,
  type Timings,
} from "../../src/index.js";

const decoder = new TextDecoder();

export class Probe extends IrohDurableObject {
  /** Stream halves an app abandoned but still references; they must not count as open. */
  #kept: BiStream[] = [];
  /** A dialed connection the app holds. */
  #dialed: Connection | null = null;

  protocols() {
    return {
      // Echo, holding the connection until the peer closes it.
      "iroh-example/echo/0": {
        accept: async (conn: Connection) => {
          const bi = await conn.acceptBi();
          await bi.send.writeAll(await bi.recv.readToEnd(65536));
          await bi.send.finish();
          await conn.closed();
        },
      },
      // Returns once the peer's stream has ended: the node closes the connection, `done`.
      "iroh-do-test/done/0": {
        accept: async (conn: Connection) => {
          const bi = await conn.acceptBi();
          await bi.recv.readToEnd(65536);
        },
      },
      // Keeps unfinished halves referenced and returns: closed with `done`, and the halves
      // stop counting.
      "iroh-do-test/keep/0": {
        accept: async (conn: Connection) => {
          const bi = await conn.acceptBi();
          await bi.recv.read(1);
          this.#kept.push(bi);
        },
      },
      // Finishes its send half, reads until the read fails (the peer resets the stream),
      // and holds the connection: with no half open, the idle close ends it after t_idle.
      "iroh-do-test/read/0": {
        accept: async (conn: Connection) => {
          const bi = await conn.acceptBi();
          await bi.send.finish();
          try {
            while ((await bi.recv.read(1024)).length > 0);
          } catch {
            // Reset by the peer: the half is released.
          }
          await conn.closed();
        },
      },
      // Reads the peer's stream to its end, writes until a write fails (the peer stops the
      // stream), and holds the connection: idle close after t_idle.
      "iroh-do-test/write/0": {
        accept: async (conn: Connection) => {
          const bi = await conn.acceptBi();
          await bi.recv.readToEnd(1024);
          const chunk = new Uint8Array(1024);
          try {
            for (;;) await bi.send.write(chunk);
          } catch {
            // Stopped by the peer: the half is released.
          }
          await conn.closed();
        },
      },
    };
  }

  // Tests shorten the limits through vars.
  protected override timings(): Timings {
    const ms = (name: string) => {
      const v: unknown = Reflect.get(this.env as object, name);
      return typeof v === "string" && v !== "" ? Number(v) : undefined;
    };
    return { tIdleMs: ms("T_IDLE_MS"), tSilentMs: ms("T_SILENT_MS") };
  }

  /** RPC: dials `peer` back, exchanges one stream, and keeps the connection. */
  async dial(peer: string): Promise<string> {
    return this.withNode(async (endpoint) => {
      const conn = await endpoint.connect(EndpointId.fromZ32(peer), "iroh-do-test/back/0");
      const bi = await conn.openBi();
      await bi.send.writeAll(new TextEncoder().encode("back"));
      await bi.send.finish();
      const reply = decoder.decode(await bi.recv.readToEnd(1024));
      this.#dialed = conn;
      return reply;
    });
  }

  /** RPC: how many abandoned halves and dialed connections the app still references. */
  async kept(): Promise<number> {
    return this.#kept.length + (this.#dialed ? 1 : 0);
  }
}

export default irohRouter({ binding: "ENDPOINTS" });
