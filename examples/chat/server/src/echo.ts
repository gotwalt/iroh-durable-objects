// `iroh-do/echo/1`: every bidirectional stream is echoed back, and recorded in the log as
// `<n> bytes` once it ends (PROTOCOL.md).
import type { BiStream, Connection } from "iroh-do";
import { rethrowTrap, unlessTrap } from "./errors";
import { background, type Logger } from "./log";
import type { ChatStorage } from "./storage";

const READ_CHUNK = 16 * 1024;

export async function serveEcho(conn: Connection, storage: ChatStorage, log: Logger): Promise<void> {
  const peer = conn.remoteId().toZ32();
  for (;;) {
    // acceptBi rejects once the connection is closed, by either side or when idle.
    const stream = await conn.acceptBi().catch(unlessTrap(null));
    if (stream === null) return;
    const echoed = echo(stream).then((bytes) => {
      storage.append({ peer, kind: "echo", nick: "", text: `${bytes} bytes` }, Date.now());
    });
    background(echoed, log, "echo stream");
  }
}

/** Echoes one stream; returns the number of bytes echoed. */
async function echo(stream: BiStream): Promise<number> {
  let total = 0;
  try {
    for (;;) {
      const chunk = await stream.recv.read(READ_CHUNK);
      // `read` resolves with an empty chunk at the end of the stream.
      if (chunk.length === 0) break;
      await stream.send.writeAll(chunk);
      total += chunk.length;
    }
    await stream.send.finish();
  } catch (error) {
    rethrowTrap(error);
    // The peer reset or stopped the stream, or closed the connection; what was echoed counts.
  }
  return total;
}
