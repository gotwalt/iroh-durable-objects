// Line framing of `iroh-do/ctl/1`: `\n` or `\r\n` terminated UTF-8 lines of at most
// LIMITS.ctlMaxLine bytes, a final unterminated line counting at the end of the stream.
import { decodeUtf8, LIMITS } from "./protocol";

/** A line the protocol refuses; its message is the text after `error: `. */
export class LineError extends Error {
  override readonly name = "LineError";
}

/** Splits bytes, pushed as they arrive, into lines. */
export class LineSplitter {
  #buffer = new Uint8Array(0);

  push(chunk: Uint8Array): void {
    const joined = new Uint8Array(this.#buffer.length + chunk.length);
    joined.set(this.#buffer);
    joined.set(chunk, this.#buffer.length);
    this.#buffer = joined;
  }

  /**
   * The next complete line without its terminator, or null if none is complete yet. With
   * `ended`, the stream has ended and a non-empty unterminated rest is the last line. Throws
   * LineError for a line that is too long, however the reads split it, or not UTF-8.
   */
  next(ended: boolean): string | null {
    const buffer = this.#buffer;
    let end = buffer.indexOf(0x0a);
    if (end < 0) {
      // One byte more than the limit may still be the `\r` of a `\r\n`.
      if (buffer.length > LIMITS.ctlMaxLine + 1) throw new LineError("line too long");
      if (!ended || buffer.length === 0) return null;
      end = buffer.length;
    }
    let line = buffer.subarray(0, end);
    this.#buffer = buffer.subarray(Math.min(end + 1, buffer.length));
    if (line[line.length - 1] === 0x0d) line = line.subarray(0, line.length - 1);
    if (line.length > LIMITS.ctlMaxLine) throw new LineError("line too long");
    const text = decodeUtf8(line);
    if (text === null) throw new LineError("line is not utf-8");
    return text;
  }
}
