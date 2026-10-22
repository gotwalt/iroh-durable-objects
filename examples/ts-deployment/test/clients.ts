// The stock clients as child processes: iroh-do-chat (examples/chat/client) and the test
// peer (crates/iroh-do-test-peer). Both are unmodified stock iroh peers; the tests only read
// what they print.
import { type LineProcess, spawnLines } from "iroh-do-testing";
import { cargoBin, nativeWrapper } from "iroh-do-testing/repo";

/** Runs `iroh-do-chat --local --local-port <port> <args>`. */
export function runChat(port: number, args: string[]): LineProcess {
  return spawnLines(cargoBin("iroh-do-chat"), ["--local", "--local-port", String(port), ...args], nativeWrapper);
}

/** A line parsed as a JSON object, or null. */
export function json(line: string): Record<string, unknown> | null {
  try {
    const v: unknown = JSON.parse(line);
    return typeof v === "object" && v !== null && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}
