// iroh's echo example (iroh 1.4.0 examples/echo.rs) as a TypeScript Durable Object.
import { IrohDurableObject, irohRouter, type Connection, type Timings } from "./iroh-do";

export class Echo extends IrohDurableObject<Env> {
  protocols() {
    return { "iroh-example/echo/0": { accept: (c: Connection) => this.serve(c) } };
  }

  async serve(conn: Connection) {
    const bi = await conn.acceptBi();
    await bi.send.writeAll(await bi.recv.readToEnd(65536));
    await bi.send.finish();
    await conn.closed();
  }

  // Tests shorten the idle limits through vars; production keeps iroh-do-core's defaults.
  protected override timings(): Timings {
    const n = (v: unknown) => (v === undefined || v === "" ? undefined : Number(v));
    const env = this.env as unknown as Record<string, unknown>;
    return { tIdleMs: n(env.T_IDLE_MS), tSilentMs: n(env.T_SILENT_MS) };
  }
}

export default { fetch: irohRouter({ binding: "ENDPOINTS" }) };
