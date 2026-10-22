// The stock clients as child processes: iroh-do-chat (examples/chat/client) and the test
// peer (crates/iroh-do-test-peer, through iroh-do/testing). Both are unmodified stock iroh
// peers; the tests only read what they print.
import { spawn } from "node:child_process";
import { join } from "node:path";
import { repo } from "iroh-do/testing";

export const chatBin = process.env.CHAT_BIN
  ?? join(process.env.CARGO_TARGET_DIR ?? join(repo, "target"), "debug", "iroh-do-chat");

/**
 * Runs `iroh-do-chat --local --local-port <port> <args>`. Collects stdout lines; `waitFor`
 * resolves with the first line, seen or still to come, that `pred` accepts.
 */
export function runChat(port, args) {
  const child = spawn(join(repo, "scripts", "macos-sign-run.sh"), [chatBin, "--local", "--local-port", String(port), ...args], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  const lines = [];
  const waiters = [];
  let buf = "";
  let stderr = "";
  const settle = () => {
    for (let i = 0; i < waiters.length; i++) {
      const w = waiters[i];
      const line = lines.find(w.pred);
      if (line !== undefined) {
        waiters.splice(i--, 1);
        clearTimeout(w.timer);
        w.resolve(line);
      }
    }
  };
  child.stdout.on("data", (d) => {
    buf += d.toString();
    let i;
    while ((i = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, i);
      buf = buf.slice(i + 1);
      if (process.env.VERBOSE) console.log(`  c ${line}`);
      lines.push(line);
    }
    settle();
  });
  child.stderr.on("data", (d) => {
    stderr = (stderr + d).slice(-4000);
    if (process.env.VERBOSE) process.stdout.write(`  ! ${d}`);
  });
  const exited = new Promise((resolve) => child.on("exit", (code) => {
    for (const w of waiters.splice(0)) {
      clearTimeout(w.timer);
      w.reject(new Error(`iroh-do-chat exited (${code}): ${stderr.trim().slice(-600)}`));
    }
    resolve(code);
  }));
  return {
    lines,
    exited,
    stderr: () => stderr,
    waitFor: (pred, timeoutMs = 30_000) => new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`timeout waiting for a line (${stderr.trim().slice(-300)})`)), timeoutMs);
      waiters.push({ pred, resolve, reject, timer });
      settle();
    }),
    stop: () => child.kill(),
  };
}

/** Parses a JSON line; null if it is not JSON. */
export function json(line) {
  try {
    return JSON.parse(line);
  } catch {
    return null;
  }
}
