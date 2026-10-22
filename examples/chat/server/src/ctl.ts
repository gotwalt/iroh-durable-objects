// `iroh-do/ctl/1`: the diagnostic line protocol (design note 006, PROTOCOL.md). One reply line
// per request line on each stream the peer opens, and once per connection an unprompted line
// on a stream the object opens, to show that it can open streams on a connection the peer
// dialed.
import type { BiStream, Connection, RecvStream, SendStream } from "iroh-do";
import { rethrowTrap, unlessTrap } from "./errors";
import { logEntry } from "./events";
import { LineSplitter } from "./lines";
import { background, errorText, type Logger } from "./log";
import { type CtlReply, ctlHello, encodeCtlReply, encodeUtf8, parseCtlRequest } from "./protocol";
import type { ChatStorage } from "./storage";

export interface CtlOptions {
  storage: ChatStorage;
  log: Logger;
  /** The object's endpoint id, z-base-32. */
  endpointId(): string;
  /** The endpoint's status, as `/status` reports it. */
  status(): Readonly<Record<string, unknown>>;
  /** Schedules a dial-back to `peer` after `delayMs`. */
  scheduleDialBack(peer: string, delayMs: number): void;
}

/** Reads lines from a stream. Each read takes at most this many bytes. */
const READ_CHUNK = 1024;

export class Ctl {
  readonly #options: CtlOptions;
  readonly #storage: ChatStorage;
  readonly #log: Logger;

  constructor(options: CtlOptions) {
    this.#options = options;
    this.#storage = options.storage;
    this.#log = options.log;
  }

  async serve(conn: Connection): Promise<void> {
    const peer = conn.remoteId().toZ32();
    let greeted = false;
    const afterFirstReply = () => {
      if (greeted) return;
      greeted = true;
      background(this.#greet(conn, peer), this.#log, "ctl hello");
    };
    for (;;) {
      // acceptBi rejects once the connection is closed, by either side or when idle.
      const stream = await conn.acceptBi().catch(unlessTrap(null));
      if (stream === null) return;
      background(this.#serveStream(peer, stream, afterFirstReply), this.#log, "ctl stream");
    }
  }

  async #serveStream(peer: string, stream: BiStream, afterFirstReply: () => void): Promise<void> {
    const lines = lineReader(stream.recv);
    for (;;) {
      let line: string | null;
      try {
        line = await lines.next();
      } catch (error) {
        rethrowTrap(error);
        // A line the protocol refuses (LineError) or a failed read: report it and stop reading
        // this stream, as PROTOCOL.md requires.
        if (await write(stream.send, `error: ${errorText(error)}\n`)) await finish(stream.send);
        return;
      }
      if (line === null) break;
      let reply: string;
      try {
        reply = encodeCtlReply(this.#respond(peer, line));
      } catch (error) {
        rethrowTrap(error);
        // In practice storage. Answered like a line that cannot be read: one error line, and
        // the stream ends.
        this.#log.error("ctl_request_failed", { peer, error: errorText(error) });
        if (await write(stream.send, `error: ${errorText(error)}\n`)) await finish(stream.send);
        return;
      }
      if (!(await write(stream.send, `${reply}\n`))) return;
      afterFirstReply();
    }
    await finish(stream.send);
  }

  #respond(peer: string, line: string): CtlReply {
    const request = parseCtlRequest(line);
    switch (request.command) {
      case "status":
        return { reply: "status", status: this.#options.status() };
      case "count":
        return {
          reply: "count",
          total: this.#storage.totalRecords(),
          yours: this.#storage.recordsOf(peer),
          peers: this.#storage.peersInLog(),
        };
      case "log":
        return { reply: "log", records: this.#storage.recentRecords(request.count).map(logEntry) };
      case "callback":
        this.#options.scheduleDialBack(peer, request.secs * 1000);
        return { reply: "callback", secs: request.secs };
      case "echo": {
        const { seq } = this.#storage.append({ peer, kind: "ctl", nick: "", text: line }, Date.now());
        return { reply: "echo", line, seq, yours: this.#storage.recordsOf(peer) };
      }
      case "invalid":
        return { reply: "error", text: request.error };
    }
  }

  async #greet(conn: Connection, peer: string): Promise<void> {
    try {
      const stream = await conn.openUni();
      await stream.writeAll(encodeUtf8(ctlHello(this.#options.endpointId(), peer)));
      await stream.finish();
    } catch (error) {
      rethrowTrap(error);
      // The connection closed first; the greeting is best effort.
    }
  }
}

/** The lines of `recv`, read as they arrive. `next()` resolves with null at the end. */
function lineReader(recv: RecvStream): { next(): Promise<string | null> } {
  const splitter = new LineSplitter();
  let ended = false;
  return {
    async next() {
      for (;;) {
        const line = splitter.next(ended);
        if (line !== null || ended) return line;
        // `read` resolves with an empty chunk at the end of the stream.
        const chunk = await recv.read(READ_CHUNK);
        if (chunk.length === 0) ended = true;
        else splitter.push(chunk);
      }
    },
  };
}

/** Writes `text`; false if the peer closed the connection or stopped the stream. */
async function write(send: SendStream, text: string): Promise<boolean> {
  try {
    await send.writeAll(encodeUtf8(text));
    return true;
  } catch (error) {
    rethrowTrap(error);
    return false;
  }
}

async function finish(send: SendStream): Promise<void> {
  try {
    await send.finish();
  } catch (error) {
    rethrowTrap(error);
    // The peer stopped the stream or the connection closed; there is nothing left to end.
  }
}
