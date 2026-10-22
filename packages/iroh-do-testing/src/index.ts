// Test helpers for apps on iroh-do: a Worker bundle in Miniflare, stock iroh peers as child
// processes read line by line, and polling with deadlines. Nothing here sleeps for a fixed
// time: every wait is for a condition, and fails with what it last saw when its deadline
// passes.
import { type ChildProcessWithoutNullStreams, spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Miniflare } from "miniflare";

/** One module of a bundle, as `iroh-do-bundle` lists it in `manifest.json`. */
interface ManifestModule {
  name: string;
  path: string;
  content_type: string;
}

interface Manifest {
  main_module: string;
  modules: ManifestModule[];
  bundle_sha256: string;
}

function readManifest(dir: string): Manifest {
  const m = JSON.parse(readFileSync(join(dir, "manifest.json"), "utf8")) as Partial<Manifest>;
  if (typeof m.main_module !== "string" || !Array.isArray(m.modules) || typeof m.bundle_sha256 !== "string") {
    throw new Error(`${dir}/manifest.json is not a bundle manifest`);
  }
  return m as Manifest;
}

/** Polls `probe` until `done(value)` holds, or throws with the last value after `timeoutMs`. */
export async function poll<T>(
  probe: () => Promise<T> | T,
  done: (v: T) => boolean,
  { timeoutMs = 30_000, intervalMs = 100, what = "condition" } = {},
): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const v = await probe();
    if (done(v)) return v;
    if (Date.now() >= deadline) {
      throw new Error(`timed out after ${timeoutMs} ms waiting for ${what}; last: ${JSON.stringify(v)}`);
    }
    await new Promise((r) => setTimeout(r, intervalMs));
  }
}

export interface WorkerOptions {
  /** Directory with `manifest.json` and the modules it names. */
  dist: string;
  /** The script name, as in the deployment (Durable Object namespaces are `<script>_<class>`). */
  script: string;
  /** Durable Object classes by binding name, all SQLite-backed. */
  classes: Record<string, string>;
  /** Vars; `ZONE_SUFFIX` (localhost), `ADMIN_TOKEN`, `PKARR_RELAY` ("") have defaults. */
  vars?: Record<string, string | number>;
  /** Storage directory to reuse, to simulate an eviction by restarting. Default: a new one. */
  persist?: string;
  /** Compatibility date. Default: the newest the bundled workerd accepts that the repo uses. */
  compatibilityDate?: string;
}

/** A log line of the Worker, as workerd printed it, one per console call. */
export type LogLine = string;

export interface TestWorker {
  mf: Miniflare;
  /** The port Miniflare listens on, chosen by the OS. */
  port: number;
  /** Storage directory, for `startWorker({ persist })`. */
  persistDir: string;
  /** Every log line so far. */
  logs: LogLine[];
  /**
   * Log lines of `event`: the library logs objects with an `event` field, which workerd
   * prints as `event: 'name'`.
   */
  events(event: string): LogLine[];
  /** `fetch` against `<host>.localhost:<port>`. */
  fetch(host: string, path: string, init?: RequestInit): Promise<Response>;
  /** `fetch` against the admin host, with the bearer token if `auth`. */
  admin(path: string, init?: RequestInit & { auth?: boolean }): Promise<Response>;
  /** `POST /mint`; throws unless 200. */
  mint(body?: unknown): Promise<{ id: string; relay_url: string; published: boolean; app?: string }>;
  /** `GET /status/<E>` with the token: the full status, or null on 404. */
  status(id: string): Promise<Record<string, unknown> | null>;
  /**
   * The Durable Object named `name` in the namespace bound as `binding`, as an RPC stub typed
   * by the caller: Miniflare's own types do not carry the class.
   */
  object<T>(binding: string, name: string): Promise<T>;
  /** The admin token the Worker was started with. */
  token: string;
  /** Stops Miniflare; deletes the storage unless `keep`. */
  dispose(keep?: boolean): Promise<void>;
}

/** Starts Miniflare on a bundle built by `iroh-do-bundle`, on a port the OS picks. */
export async function startWorker(opts: WorkerOptions): Promise<TestWorker> {
  const manifest = readManifest(opts.dist);
  const modules = Object.fromEntries(
    manifest.modules.map((m) => {
      const path = join(opts.dist, m.path);
      return [
        m.name,
        m.content_type === "application/wasm"
          ? { type: "wasm", contents: new Uint8Array(readFileSync(path)) }
          : { type: "esm", contents: readFileSync(path, "utf8") },
      ];
    }),
  );
  const persistDir = opts.persist ?? mkdtempSync(join(tmpdir(), "iroh-do-test-"));
  const token = "test-token";
  const logs: LogLine[] = [];
  const vars: Record<string, string> = {
    ZONE_SUFFIX: "localhost",
    BUNDLE_SHA256: manifest.bundle_sha256,
    ADMIN_TOKEN: token,
    PKARR_RELAY: "",
    LOG_LEVEL: process.env.LOG_LEVEL ?? "info,iroh=warn,noq=warn,netwatch=error,iroh_do=info",
    ...Object.fromEntries(Object.entries(opts.vars ?? {}).map(([k, v]) => [k, String(v)])),
  };
  const text = (value: string) => ({ type: "text", value });
  const options = {
    host: "127.0.0.1",
    port: 0,
    resourcePersistencePath: persistDir,
    handleStructuredLogs: ({ message }: { message: string }) => {
      // workerd prints a logged object over several lines.
      const line = message.replace(/\s*\n\s*/g, " ");
      logs.push(line);
      if (process.env.VERBOSE) console.log(`  w ${line}`);
    },
    workers: [
      {
        config: {
          name: opts.script,
          type: "worker",
          compatibilityDate: opts.compatibilityDate ?? "2026-09-15",
          manifest: { mainModule: manifest.main_module, modules },
          env: {
            ...Object.fromEntries(
              Object.entries(opts.classes).map(([b, c]) => [
                b,
                { type: "durable-object", worker: opts.script, exportName: c },
              ]),
            ),
            ...Object.fromEntries(Object.entries(vars).map(([k, v]) => [k, text(v)])),
          },
          exports: Object.fromEntries(
            Object.values(opts.classes).map((c) => [c, { type: "durable-object", storage: "sqlite" }]),
          ),
        },
      },
    ],
  };
  // The worker `config` form of Miniflare 5 is not in its option types yet.
  const mf = new Miniflare(options as unknown as ConstructorParameters<typeof Miniflare>[0]);
  const url = await mf.ready;
  const port = Number(url.port);
  const fetchAt = (host: string, path: string, init?: RequestInit) =>
    mf.dispatchFetch(`http://${host}.localhost:${port}${path}`, init as never) as unknown as Promise<Response>;
  const admin = (path: string, init: RequestInit & { auth?: boolean } = {}) => {
    const { auth, ...rest } = init;
    const headers = new Headers(rest.headers);
    if (auth) headers.set("authorization", `Bearer ${token}`);
    return fetchAt("admin", path, { ...rest, headers });
  };
  return {
    mf,
    port,
    persistDir,
    logs,
    token,
    object: async <T>(binding: string, name: string) => {
      const ns = (await mf.getDurableObjectNamespace(binding, opts.script)) as unknown as {
        idFromName(n: string): unknown;
        get(id: unknown): unknown;
      };
      return ns.get(ns.idFromName(name)) as T;
    },
    events: (event) => logs.filter((l) => l.includes(`event: '${event}'`)),
    fetch: fetchAt,
    admin,
    mint: async (body) => {
      const r = await admin("/mint", {
        method: "POST",
        auth: true,
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      });
      if (r.status !== 200) throw new Error(`mint: ${r.status} ${await r.text()}`);
      return (await r.json()) as { id: string; relay_url: string; published: boolean; app?: string };
    },
    status: async (id) => {
      const r = await admin(`/status/${id}`, { auth: true });
      if (r.status === 404) return null;
      if (r.status !== 200) throw new Error(`status: ${r.status} ${await r.text()}`);
      return (await r.json()) as Record<string, unknown>;
    },
    dispose: async (keep = false) => {
      await mf.dispose();
      if (!keep) rmSync(persistDir, { recursive: true, force: true });
    },
  };
}

/** A child process whose stdout is read as lines. */
export interface LineProcess {
  child: ChildProcessWithoutNullStreams;
  /** Every stdout line so far. */
  lines: string[];
  /** The last 4 kB of stderr. */
  stderr(): string;
  /** Writes a line to stdin. */
  send(line: string): void;
  /**
   * The first line accepted by `pred` that no earlier `take` consumed, seen or still to come.
   * Rejects after `timeoutMs`, or when the process exits first.
   */
  take(pred: (line: string) => boolean, timeoutMs?: number): Promise<string>;
  /** Resolves with the exit code. */
  exited: Promise<number | null>;
  stop(): void;
}

/**
 * Spawns `command args`, through `wrapper` if given (for example a script that signs a
 * native binary before running it).
 */
export function spawnLines(command: string, args: string[], wrapper: string[] = []): LineProcess {
  const [cmd, ...pre] = [...wrapper, command];
  if (cmd === undefined) throw new Error("no command");
  const child = spawn(cmd, [...pre, ...args], { stdio: ["pipe", "pipe", "pipe"] });
  const lines: string[] = [];
  const taken = new Set<number>();
  const waiters: { pred: (l: string) => boolean; resolve: (l: string) => void; reject: (e: Error) => void }[] = [];
  let buf = "";
  let stderr = "";
  let exitCode: number | null | undefined;
  const settle = () => {
    for (let i = 0; i < waiters.length; i++) {
      const w = waiters[i];
      if (!w) continue;
      const j = lines.findIndex((l, k) => !taken.has(k) && w.pred(l));
      if (j >= 0) {
        taken.add(j);
        waiters.splice(i--, 1);
        w.resolve(lines[j] ?? "");
      }
    }
  };
  child.stdout.on("data", (d: Buffer) => {
    buf += d.toString();
    for (let i = buf.indexOf("\n"); i >= 0; i = buf.indexOf("\n")) {
      const line = buf.slice(0, i);
      buf = buf.slice(i + 1);
      if (process.env.VERBOSE) console.log(`  ${command.split("/").pop()} ${line}`);
      lines.push(line);
    }
    settle();
  });
  child.stderr.on("data", (d: Buffer) => {
    stderr = (stderr + d.toString()).slice(-4000);
  });
  const exited = new Promise<number | null>((resolve) =>
    child.on("exit", (code) => {
      exitCode = code;
      for (const w of waiters.splice(0))
        w.reject(new Error(`${command} exited (${code}): ${stderr.trim().slice(-600)}`));
      resolve(code);
    }),
  );
  return {
    child,
    lines,
    stderr: () => stderr,
    send: (l) => child.stdin.write(`${l}\n`),
    take: (pred, timeoutMs = 30_000) =>
      new Promise((resolve, reject) => {
        if (exitCode !== undefined) {
          const j = lines.findIndex((l, k) => !taken.has(k) && pred(l));
          if (j < 0) return reject(new Error(`${command} already exited (${exitCode})`));
        }
        const timer = setTimeout(() => {
          const k = waiters.indexOf(w);
          if (k >= 0) waiters.splice(k, 1);
          reject(
            new Error(
              `timed out after ${timeoutMs} ms waiting for a line of ${command}; lines: ${JSON.stringify(lines.slice(-5))}; stderr: ${stderr.trim().slice(-300)}`,
            ),
          );
        }, timeoutMs);
        const w = {
          pred,
          resolve: (l: string) => {
            clearTimeout(timer);
            resolve(l);
          },
          reject: (e: Error) => {
            clearTimeout(timer);
            reject(e);
          },
        };
        waiters.push(w);
        settle();
      }),
    exited,
    stop: () => {
      child.stdin.end();
      child.kill();
    },
  };
}

/** The stock test peer (`crates/iroh-do-test-peer`) driven over stdin. */
export interface TestPeer extends LineProcess {
  /** The peer's own EndpointId, z-base-32. */
  id: string;
  /** Sends `command` and returns the next line that is not from a dial-back. */
  run(command: string, timeoutMs?: number): Promise<string>;
}

/** Starts the test peer for endpoint `E` on Miniflare at `port`; resolves once it is ready. */
export async function startPeer(bin: string, id: string, port: number, wrapper: string[] = []): Promise<TestPeer> {
  const p = spawnLines(bin, [id, "--local", String(port)], wrapper);
  const ready = await p.take((l) => l.startsWith("ready "), 60_000);
  return {
    ...p,
    id: ready.slice("ready ".length),
    run: (command, timeoutMs = 90_000) => {
      p.send(command);
      return p.take((l) => !l.startsWith("back"), timeoutMs);
    },
  };
}

/** Milliseconds from a peer line ending in `after_ms=<n>`, or NaN. */
export function afterMs(line: string): number {
  return Number(/after_ms=(\d+)/.exec(line)?.[1]);
}
