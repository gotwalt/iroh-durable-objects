// Shared pieces of the Miniflare tests: loading a bundle built by scripts/bundle.mjs,
// capturing the Worker's log lines, and driving the stock test peer (crates/iroh-do-test-peer).
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { Miniflare } from "miniflare";

export const repo = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
export const peerBin = process.env.PEER
  ?? join(process.env.CARGO_TARGET_DIR ?? join(repo, "target"), "debug", "iroh-do-test-peer");
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let failures = 0;
/** Prints one PASS or FAIL line. */
export function check(name, ok, detail = "") {
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? `: ${detail}` : ""}`);
  if (!ok) failures++;
}
/** Prints the summary and exits with the result. */
export function finish() {
  console.log(failures ? `${failures} FAILURE(S)` : "ALL PASS");
  process.exit(failures ? 1 : 0);
}
export function fail(e) {
  console.log(`FAIL exception: ${e?.stack ?? e}`);
  failures++;
}

/**
 * Starts Miniflare on the bundle in `dist` (manifest.json from scripts/bundle.mjs), with one
 * Durable Object class bound as ENDPOINTS. `persist` keeps storage across instances, to
 * simulate an eviction by restarting. Collects the Worker's log lines in `logs`.
 */
export function startWorker({ dist, script, className, port, vars = {}, persist }) {
  const manifest = JSON.parse(readFileSync(join(dist, "manifest.json"), "utf8"));
  const modules = Object.fromEntries(manifest.modules.map((m) => [m.name, m.content_type === "application/wasm"
    ? { type: "wasm", contents: new Uint8Array(readFileSync(join(dist, m.path))) }
    : { type: "esm", contents: readFileSync(join(dist, m.path), "utf8") }]));
  const persistDir = persist ?? mkdtempSync(join(tmpdir(), "iroh-do-test-"));
  const logs = [];
  const text = (value) => ({ type: "text", value });
  const mf = new Miniflare({
    host: "127.0.0.1",
    port,
    resourcePersistencePath: persistDir,
    handleStructuredLogs: ({ level, message }) => {
      // One entry per console call; workerd prints a logged object over several lines.
      const line = message.replace(/\s*\n\s*/g, " ");
      logs.push(line);
      if (process.env.VERBOSE) console.log(`  w ${level} ${line}`);
    },
    workers: [{
      config: {
        name: script,
        type: "worker",
        compatibilityDate: "2026-09-15",
        manifest: { mainModule: manifest.main_module, modules },
        env: {
          ENDPOINTS: { type: "durable-object", worker: script, exportName: className },
          ZONE_SUFFIX: text("localhost"),
          BUNDLE_SHA256: text(manifest.bundle_sha256),
          ADMIN_TOKEN: text("test-token"),
          PKARR_RELAY: text(""),
          LOG_LEVEL: text(process.env.LOG_LEVEL ?? "info,iroh=warn,noq=warn,netwatch=error,iroh_do=info"),
          ...Object.fromEntries(Object.entries(vars).map(([k, v]) => [k, text(String(v))])),
        },
        exports: { [className]: { type: "durable-object", storage: "sqlite" } },
      },
    }],
  });
  const base = `http://admin.localhost:${port}`;
  const auth = { authorization: "Bearer test-token" };
  return {
    mf,
    logs,
    persistDir,
    /** Log entries whose `event` field is `event`. */
    events: (event) => logs.filter((l) => new RegExp(`event["']?: ?["']${event}["']`).test(l)),
    mint: async () => (await mf.dispatchFetch(`${base}/mint`, { method: "POST", headers: auth })).json(),
    status: async (E) => {
      const r = await mf.dispatchFetch(`${base}/status/${E}`);
      return r.status === 200 ? r.json() : null;
    },
    admin: (path, init = {}) => mf.dispatchFetch(`${base}${path}`, init),
    auth,
    dispose: async (keep = false) => {
      await mf.dispose();
      if (!keep) rmSync(persistDir, { recursive: true, force: true });
    },
  };
}

/** Starts the stock test peer for endpoint `E` on the local workerd at `port`. */
export function startPeer(E, port) {
  const child = spawn(join(repo, "scripts", "macos-sign-run.sh"), [peerBin, E, "--local", String(port)], { stdio: ["pipe", "pipe", "pipe"] });
  const lines = [];
  const waiters = [];
  let buf = "";
  const deliver = () => {
    for (let i = 0; i < waiters.length; i++) {
      const w = waiters[i];
      const j = lines.findIndex((l) => w.re.test(l));
      if (j >= 0) {
        const [line] = lines.splice(j, 1);
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
      if (process.env.VERBOSE) console.log(`  p ${line}`);
      lines.push(line);
    }
    deliver();
  });
  let stderr = "";
  child.stderr.on("data", (d) => {
    stderr = (stderr + d).slice(-2000);
    if (process.env.VERBOSE) process.stdout.write(`  ! ${d}`);
  });
  child.on("exit", (code) => {
    for (const w of waiters.splice(0)) {
      clearTimeout(w.timer);
      w.reject(new Error(`peer exited (${code}) waiting for ${w.re}: ${stderr.trim()}`));
    }
  });
  /** The next line matching `re` (default: the next line not from a dial-back). */
  const next = (re = /^(?!back)/, timeoutMs = 90_000) => new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`peer timeout waiting for ${re}`)), timeoutMs);
    waiters.push({ re, resolve, reject, timer });
    deliver();
  });
  return {
    child,
    send: (l) => child.stdin.write(l + "\n"),
    next,
    stop: () => {
      child.stdin.end();
      child.kill();
    },
  };
}

/** Milliseconds from a peer line ending in `after_ms=<n>`. */
export const afterMs = (line) => Number(/after_ms=(\d+)/.exec(line)?.[1]);
