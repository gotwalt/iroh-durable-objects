// Dial-backs over `iroh-do/notify/1` (design note 007, PROTOCOL.md): when a `/callback` delay
// ends, the object dials the peer over the peer's relay socket and writes one line.
import { type Connection, type Endpoint, EndpointId } from "iroh-do";
import { errorText, type Logger } from "./log";
import { encodeUtf8, NOTIFY_ALPN, notifyLine } from "./protocol";
import type { ChatStorage } from "./storage";

/** How long a dial-back waits for the peer to close the connection before closing it. */
const LINGER_MS = 5_000;
/** QUIC application close code 0, as the node closes connections whose handler returned. */
const CLOSE_OK = 0n;

export interface DialBackStats {
  dialBacksOk: number;
  dialBacksFailed: number;
}

export interface DialBackOptions {
  storage: ChatStorage;
  log: Logger;
  /** Called only once the node is built, from the alarm. */
  endpoint(): Endpoint;
  /** The object's wake count, for the notify line. */
  wakes(): number;
}

export class DialBack {
  readonly stats: DialBackStats = { dialBacksOk: 0, dialBacksFailed: 0 };
  readonly #options: DialBackOptions;

  constructor(options: DialBackOptions) {
    this.#options = options;
  }

  /**
   * Dials `peer` (z-base-32) and writes the notify line. Every attempt ends with a `callback`
   * record, `delivered` or `failed: <reason>`, and a `dial_back` event. Failures of the dial
   * and the stream are outcomes, not errors: the promise rejects only if storage fails.
   */
  async run(peer: string): Promise<void> {
    const { storage, log } = this.#options;
    const started = Date.now();
    const record = (text: string) => storage.append({ peer, kind: "callback", nick: "", text }, Date.now());
    const failed = (stage: string, error: unknown) => {
      this.stats.dialBacksFailed++;
      log.warn("dial_back", { peer, ok: false, ms: Date.now() - started, error: `${stage}: ${errorText(error)}` });
      record(`failed: ${errorText(error)}`);
    };

    let conn: Connection;
    try {
      conn = await this.#options.endpoint().connect(EndpointId.fromZ32(peer), NOTIFY_ALPN);
    } catch (error) {
      // No relay socket for the peer, or the peer did not answer the handshake.
      failed("connect", error);
      return;
    }
    try {
      const line = notifyLine(this.#options.endpoint().id().toZ32(), this.#options.wakes(), storage.totalRecords());
      const stream = await conn.openUni();
      await stream.writeAll(encodeUtf8(line));
      await stream.finish();
    } catch (error) {
      failed("stream", error);
      conn.close(CLOSE_OK, encodeUtf8("done"));
      return;
    }
    this.stats.dialBacksOk++;
    log.info("dial_back", { peer, ok: true, ms: Date.now() - started });
    record("delivered");
    // Closing at once could drop the line before the peer has read it; the peer closes the
    // connection once it has.
    await closedWithin(conn, LINGER_MS);
    conn.close(CLOSE_OK, encodeUtf8("done"));
  }
}

/** Resolves when `conn` closes or after `ms`, whichever comes first. */
async function closedWithin(conn: Connection, ms: number): Promise<void> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<void>((resolve) => {
    timer = setTimeout(resolve, ms);
  });
  try {
    await Promise.race([conn.closed(), timeout]);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
}
