// Test app for lifetime-test.mjs and panic-test.mjs. Each ALPN exercises one rule of the
// connection lifetime (README.md, "Connection lifetime").
import { EndpointId, IrohDurableObject, irohRouter, type BiStream, type Connection } from "../src/index";

const decoder = new TextDecoder();

export class Probe extends IrohDurableObject<Env> {
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
    };
  }

  /** Dials `peer` back, exchanges one stream, and keeps the connection. */
  async dial(peer: string): Promise<string> {
    const conn = await this.endpoint.connect(EndpointId.fromZ32(peer), "iroh-do-test/back/0");
    const bi = await conn.openBi();
    await bi.send.writeAll(new TextEncoder().encode("back"));
    await bi.send.finish();
    const reply = decoder.decode(await bi.recv.readToEnd(1024));
    this.#dialed = conn;
    return reply;
  }

  /** Whether the app still references the kept halves. */
  kept(): number {
    return this.#kept.length + (this.#dialed ? 1 : 0);
  }
}

const router = irohRouter({ binding: "ENDPOINTS" });

// Test routes on admin.<zone>/test/..., ahead of the router.
export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    const url = new URL(req.url);
    const m = /^\/test\/(dial|panic|kept)\/([^/]+)$/.exec(url.pathname);
    if (!url.hostname.startsWith("admin.") || !m) return router(req, env);
    const stub = env.ENDPOINTS.get(env.ENDPOINTS.idFromName(m[2]));
    try {
      switch (m[1]) {
        case "dial":
          return Response.json({ reply: await stub.dial(url.searchParams.get("peer") ?? "") });
        case "panic":
          await stub.testPanic(url.searchParams.get("task") === "1");
          return Response.json({ ok: true });
        default:
          return Response.json({ kept: await stub.kept() });
      }
    } catch (e) {
      return Response.json({ error: String(e) }, { status: 500 });
    }
  },
};
