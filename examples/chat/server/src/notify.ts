// Dial-backs over `iroh-do/notify/1` (design note 007, PROTOCOL.md): when a `/callback` delay
// ends, the object dials the peer over the peer's relay socket and writes one line.
import { type Connection, type Endpoint, EndpointId } from "iroh-do";
import { rethrowTrap, unlessTrap } from "./errors";
import { background, errorText, type Logger } from "./log";
import { encodeUtf8, NOTIFY_ALPN, notifyLine } from "./protocol";
import type { ChatStorage } from "./storage";

/** How long a dial-back waits for the peer to close the connection before closing it. */
const LINGER_MS = 5_000;
/** QUIC application close code 0, as the node closes connections whose handler returned. */
const CLOSE_OK = 0n;

export interface DialBackOptions {
  storage: ChatStorage;
  log: Logger;
  /** Called only once the node is built, from the alarm. */
  endpoint(): Endpoint;
  /** The object's wake count, for the notify line. */
  wakes(): number;
}

export class DialBack {
  readonly #options: DialBackOptions;

  constructor(options: DialBackOptions) {
    this.#options = options;
  }

  /**
   * Dials `peer` (z-base-32), writes the notify line and returns. The connection then stays
   * open in the background until the peer closes it, at most LINGER_MS (PROTOCOL.md), so that
   * the alarm that runs this does not wait for the peer. Every attempt ends with a `callback`
   * record, `delivered` or `failed: <reason>`, and a `dial_back` event. Nothing is retried, so
   * that a peer is never notified twice: the promise rejects only for a wasm trap.
   */
  async run(peer: string): Promise<void> {
    const started = Date.now();
    let conn: Connection;
    try {
      conn = await this.#options.endpoint().connect(EndpointId.fromZ32(peer), NOTIFY_ALPN);
    } catch (error) {
      rethrowTrap(error);
      // No relay socket for the peer, or the peer did not answer the handshake.
      this.#ended(peer, started, "connect", error);
      return;
    }
    try {
      const { storage } = this.#options;
      const line = notifyLine(this.#options.endpoint().id().toZ32(), this.#options.wakes(), storage.totalRecords());
      const stream = await conn.openUni();
      await stream.writeAll(encodeUtf8(line));
      await stream.finish();
    } catch (error) {
      rethrowTrap(error);
      close(conn);
      this.#ended(peer, started, "stream", error);
      return;
    }
    this.#ended(peer, started);
    // Closing at once could drop the line before the peer has read it; the peer closes the
    // connection once it has.
    background(linger(conn), this.#options.log, "dial-back linger");
  }

  /** Logs and records the outcome of an attempt; a failure is given by its stage and error. */
  #ended(peer: string, started: number, stage?: string, error?: unknown): void {
    const { storage, log } = this.#options;
    const ms = Date.now() - started;
    let text: string;
    if (stage === undefined) {
      log.info("dial_back", { peer, ok: true, ms });
      text = "delivered";
    } else {
      log.warn("dial_back", { peer, ok: false, ms, error: `${stage}: ${errorText(error)}` });
      text = `failed: ${errorText(error)}`;
    }
    try {
      storage.append({ peer, kind: "callback", nick: "", text }, Date.now());
    } catch (recordError) {
      rethrowTrap(recordError);
      // Not thrown: the alarm would run the dial-back again and notify the peer twice.
      log.error("dial_back_record_failed", { peer, error: errorText(recordError) });
    }
  }
}

/** Waits until `conn` closes or LINGER_MS has passed, then closes it. */
async function linger(conn: Connection): Promise<void> {
  await closedWithin(conn, LINGER_MS);
  close(conn);
}

/** Closes `conn` as the node closes connections whose handler returned. */
function close(conn: Connection): void {
  try {
    conn.close(CLOSE_OK, encodeUtf8("done"));
  } catch (error) {
    // Already closed with the instance it belonged to.
    rethrowTrap(error);
  }
}

/** Resolves when `conn` closes or after `ms`, whichever comes first. */
async function closedWithin(conn: Connection, ms: number): Promise<void> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<void>((resolve) => {
    timer = setTimeout(resolve, ms);
  });
  try {
    await Promise.race([conn.closed().catch(unlessTrap("")), timeout]);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
}
