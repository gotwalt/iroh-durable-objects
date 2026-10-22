// iroh's echo example (iroh 1.4.0 examples/echo.rs) as a TypeScript Durable Object.
import { IrohDurableObject, irohRouter, type Connection, type Timings } from "iroh-do";

export class Echo extends IrohDurableObject<Env> {
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

  // Tests shorten the limits through vars; the deployment leaves them unbound.
  protected override timings(): Timings {
    const n = (v: string | undefined) => (v === undefined || v === "" ? undefined : Number(v));
    const env = this.env as unknown as Record<string, string | undefined>;
    return { tIdleMs: n(env.T_IDLE_MS), tSilentMs: n(env.T_SILENT_MS) };
  }
}

export default { fetch: irohRouter({ binding: "ENDPOINTS" }) };
