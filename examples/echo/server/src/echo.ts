// iroh's echo example (iroh 1.4.0 examples/echo.rs) as a TypeScript Durable Object.
import { type Connection, IrohDurableObject, type Timings } from "iroh-do";
import { milliseconds } from "./config.js";

/** The vars the class reads; tests shorten the limits through them. */
export interface EchoEnv {
  T_IDLE_MS?: string;
  T_SILENT_MS?: string;
}

export class Echo extends IrohDurableObject<EchoEnv> {
  static override app = "echo";

  protocols() {
    return { "iroh-example/echo/0": { accept: (c: Connection) => this.serve(c) } };
  }

  async serve(conn: Connection) {
    const bi = await conn.acceptBi();
    await bi.send.writeAll(await bi.recv.readToEnd(65536));
    await bi.send.finish();
    // The connection closes when this returns; the peer closes it once it has the reply.
    await conn.closed();
  }

  protected override timings(): Timings {
    return {
      tIdleMs: milliseconds(this.env.T_IDLE_MS, "T_IDLE_MS"),
      tSilentMs: milliseconds(this.env.T_SILENT_MS, "T_SILENT_MS"),
    };
  }
}
