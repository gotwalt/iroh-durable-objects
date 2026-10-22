import { describe, expect, it } from "vitest";
import { LineError, LineSplitter } from "../src/lines";
import { LIMITS } from "../src/protocol";

const bytes = (text: string) => new TextEncoder().encode(text);

function splitter(...chunks: (string | Uint8Array)[]): LineSplitter {
  const s = new LineSplitter();
  for (const chunk of chunks) s.push(typeof chunk === "string" ? bytes(chunk) : chunk);
  return s;
}

describe("LineSplitter", () => {
  it("splits on \\n and \\r\\n, across chunks", () => {
    const s = splitter("a\r\nb", "c\n");
    expect(s.next(false)).toBe("a");
    expect(s.next(false)).toBe("bc");
    expect(s.next(false)).toBeNull();
  });

  it("returns a final unterminated line at the end only", () => {
    const s = splitter("last");
    expect(s.next(false)).toBeNull();
    expect(s.next(true)).toBe("last");
    expect(s.next(true)).toBeNull();
  });

  it("limits lines to ctl_max_line bytes, whichever read brings the terminator", () => {
    expect(() => splitter(`${"a".repeat(4500)}\n`).next(false)).toThrow(new LineError("line too long"));

    const s = splitter(`${"a".repeat(LIMITS.ctlMaxLine)}\r\nnext`);
    expect(s.next(false)).toHaveLength(LIMITS.ctlMaxLine);
    expect(s.next(false)).toBeNull();
    expect(s.next(true)).toBe("next");

    // Without a terminator, one byte more may still be the `\r` of a `\r\n`.
    const t = splitter("a".repeat(LIMITS.ctlMaxLine + 1));
    expect(t.next(false)).toBeNull();
    t.push(bytes("a"));
    expect(() => t.next(false)).toThrow(new LineError("line too long"));
  });

  it("refuses a line that is not UTF-8", () => {
    expect(() => splitter(Uint8Array.of(0xff, 0x0a)).next(false)).toThrow(new LineError("line is not utf-8"));
  });
});
