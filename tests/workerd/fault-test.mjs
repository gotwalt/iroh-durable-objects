// Recovery from failures, on Miniflare (workerd): failed and trapped alarm work, failed and
// trapped mints, and an object that traps while its endpoint is bound.
//
//   node tests/workerd/fault-test.mjs
//
// Uses the bundle of tests/workerd/fault-worker in build/fault-worker/, built with
//   deploy/build-worker.sh tests/workerd/fault-worker "$PWD/build/fault-worker"
// and the peer at target/debug/iroh-do-test-peer (override with IROH_DO_TEST_PEER). The
// fault worker hosts iroh's echo app and apps that fail on purpose (see its crate docs).
// Assertions read the objects' SQLite files and the Worker's structured logs.
//
// The alarm case waits out the real retry delays (30 s, then 60 s), so the test takes about
// two minutes.
import { spawn } from "node:child_process";
import { mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { fileURLToPath } from "node:url";
import { Miniflare } from "miniflare";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "..", "..");
const bundle = join(repo, "build", "fault-worker");
const peerBin = process.env.IROH_DO_TEST_PEER ?? join(repo, "target", "debug", "iroh-do-test-peer");
const port = Number(process.env.PORT ?? 8851);
const ADMIN_TOKEN = "fault-test-token";
const SCRIPT = "iroh-do";
const CLASS = "endpoint";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;
const check = (name, ok, detail = "") => {
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? `: ${detail}` : ""}`);
  if (!ok) failures++;
};

/**
 * The Worker's log lines. Each event is one logged object (design note 010), which workerd
 * prints in Node's inspect format: `event: 'mint'`, `app: 'echo'`.
 */
const logs = [];
const logged = (event, ...fields) =>
  logs.some((m) => m.includes(`event: '${event}'`) && fields.every((f) => m.includes(f)));
const persistDir = mkdtempSync(join(tmpdir(), "iroh-do-fault-"));
const text = (value) => ({ type: "text", value });
const mf = new Miniflare({
  host: "127.0.0.1",
  resourcePersistencePath: persistDir,
  port,
  handleStructuredLogs: (log) => {
    logs.push(String(log.message));
    if (process.env.VERBOSE) console.log(`  | ${log.message}`);
  },
  workers: [
    {
      config: {
        name: SCRIPT,
        type: "worker",
        compatibilityDate: "2026-09-15",
        manifest: {
          mainModule: "index.js",
          modules: {
            "index.js": { type: "esm", contents: readFileSync(join(bundle, "index.js"), "utf8") },
            "index_bg.wasm": { type: "wasm", contents: new Uint8Array(readFileSync(join(bundle, "index_bg.wasm"))) },
          },
        },
        env: {
          ENDPOINTS: { type: "durable-object", worker: SCRIPT, exportName: CLASS },
          REGISTRY: { type: "durable-object", worker: SCRIPT, exportName: "registry" },
          ZONE_SUFFIX: text("localhost"),
          ADMIN_TOKEN: text(ADMIN_TOKEN),
          PKARR_RELAY: text(""),
          BUNDLE_SHA256: text("fault-test"),
          LOG_LEVEL: text("info,iroh=warn,noq=warn"),
        },
        exports: { [CLASS]: { type: "durable-object", storage: "sqlite" }, registry: { type: "durable-object", storage: "sqlite" } },
      },
    },
  ],
});

/** Every endpoint object's SQLite file, by path. */
const objectFiles = () =>
  readdirSync(persistDir, { recursive: true })
    .filter((f) => f.endsWith(".sqlite") && f.includes(CLASS))
    .map((f) => join(persistDir, f));

/** Runs `sql` on a SQLite file, read-only; [] if a table is missing. */
const query = (file, sql, ...params) => {
  const db = new DatabaseSync(file, { readOnly: true });
  try {
    return db.prepare(sql).all(...params);
  } catch {
    return [];
  } finally {
    db.close();
  }
};
const identities = () => objectFiles().flatMap((f) => query(f, "SELECT app FROM identity"));

const mint = async (app, timeoutMs = 30_000) => {
  try {
    const res = await mf.dispatchFetch(`http://admin.localhost:${port}/mint`, {
      method: "POST",
      headers: { Authorization: `Bearer ${ADMIN_TOKEN}` },
      body: JSON.stringify({ app }),
      signal: AbortSignal.timeout(timeoutMs),
    });
    return { status: res.status, body: await res.text() };
  } catch (e) {
    return { status: 0, body: String(e?.name ?? e) };
  }
};
const listed = async () => {
  const res = await mf.dispatchFetch(`http://admin.localhost:${port}/endpoints.json`);
  return res.status === 200 ? (await res.json()).map((e) => e.app) : [`status ${res.status}`];
};

/** Waits until `pred()` is truthy, polling every 250 ms; returns its value or null. */
const until = async (pred, ms) => {
  const t0 = Date.now();
  for (;;) {
    const v = pred();
    if (v) return v;
    if (Date.now() - t0 > ms) return null;
    await sleep(250);
  }
};

let peer;
try {
  await mf.ready;
  const ns = await mf.getDurableObjectNamespace("ENDPOINTS");

  // 1. A mint whose app schema fails stores nothing: 503, no identity, not listed.
  const broken = await mint("broken");
  check("mint with a failing schema answers 503", broken.status === 503, `${broken.status} ${broken.body}`);
  check("failed mint stored no identity", identities().length === 0, JSON.stringify(identities()));
  check("failed mint is not listed", (await listed()).length === 0, JSON.stringify(await listed()));
  check("failed mint logged mint_failed", await until(() => logged("mint_failed", "app: 'broken'"), 5_000));

  // 2. A mint whose app constructor panics traps inside the transaction, and the identity is
  //    rolled back. workers-rs leaves the promise of a trapped handler unsettled, so the
  //    request may get no answer; the next request is served by a new instance.
  const trapped = await mint("panic", 5_000);
  console.log(`info mint that traps: ${trapped.status || "no answer"} ${trapped.body}`);
  check("trapped mint stored no identity", identities().length === 0, JSON.stringify(identities()));
  check("trapped mint is not listed", (await listed()).length === 0, JSON.stringify(await listed()));

  // 3. Alarm work: the first run fails, the second traps, the third succeeds. The schedule row
  //    survives both failures with its attempt counted, and is deleted after the success,
  //    when the alarm is deleted too.
  const fault = await mint("fault");
  const minted = fault.status === 200 ? JSON.parse(fault.body) : {};
  check("mint the fault app", fault.status === 200 && minted.app === "fault", `${fault.status} ${fault.body}`);
  const E = minted.id;
  const file = () => objectFiles().find((f) => f.endsWith(`${ns.idFromName(E).toString()}.sqlite`));
  const work = () => query(file(), "SELECT due_ms, attempt FROM schedule WHERE key = 'app:work'")[0];
  const runs = () => query(file(), "SELECT attempt FROM fault_runs ORDER BY rowid").map((r) => r.attempt);
  const counter = (name) => query(file(), "SELECT value FROM counters WHERE name = ?", name)[0]?.value;

  const t0 = Date.now();
  const first = await until(() => runs().length === 1 && work()?.attempt === 1 && work(), 10_000);
  check("failed run leaves the work scheduled, attempt 1", first !== null, JSON.stringify({ runs: runs(), work: work() }));
  if (first) {
    const inS = (first.due_ms - Date.now()) / 1000;
    check("retry after 30 s", inS > 20 && inS <= 31, `due in ${inS.toFixed(1)} s`);
  }
  check("alarm_entry_failed logged",
    await until(() => logged("alarm_entry_failed", "key: 'app'", "attempt: 0"), 5_000));

  const wakesBefore = counter("wakes");
  const second = await until(() => work()?.attempt === 2 && work(), 45_000);
  check("trapped run leaves the work scheduled, attempt 2", second !== null, JSON.stringify({ runs: runs(), work: work() }));
  if (second) {
    const inS = (second.due_ms - Date.now()) / 1000;
    check("retry after 60 s", inS > 40 && inS <= 61, `due in ${inS.toFixed(1)} s`);
  }
  // The next event re-instantiates the trapped wasm module and reruns the constructor.
  await mf.dispatchFetch(`http://admin.localhost:${port}/status/${E}`);
  check("the trap re-ran the constructor", counter("wakes") > wakesBefore, `wakes ${wakesBefore} -> ${counter("wakes")}`);

  const done = await until(() => runs().length === 2 && !work(), 75_000);
  check("third run succeeds and deletes the work", done !== null, JSON.stringify({ runs: runs(), work: work() }));
  check("runs that returned: attempts 0 and 2", JSON.stringify(runs()) === "[0,2]", JSON.stringify(runs()));
  check("schedule is empty", query(file(), "SELECT key FROM schedule").length === 0,
    JSON.stringify(query(file(), "SELECT key FROM schedule")));
  const meta = query(file(), "SELECT * FROM _cf_METADATA");
  console.log(`info _cf_METADATA: ${JSON.stringify(meta)}`);
  console.log(`info alarm case took ${((Date.now() - t0) / 1000).toFixed(0)} s`);

  // 4. The bound marker is set while the endpoint is bound and cleared by a clean close: a
  //    connection with an ALPN the app does not serve binds the endpoint, which closes after
  //    t_linger.
  peer = spawn(peerBin, [E, "--local", String(port)], { stdio: ["pipe", "pipe", "pipe"] });
  const lines = [];
  peer.stdout.on("data", (d) => lines.push(...d.toString().split("\n").filter(Boolean)));
  peer.stderr.on("data", (d) => process.env.VERBOSE && process.stdout.write(`  peer| ${d}`));
  const ready = await until(() => lines.find((l) => l.startsWith("ready")), 30_000);
  check("peer ready", ready !== null, lines.join(" | "));
  const builds = counter("endpoint_builds") ?? 0;
  peer.stdin.write("echo x iroh-do-test/none/0\n");
  await until(() => lines.some((l) => l.startsWith("echo")), 15_000);
  check("endpoint bound for the connection", await until(() => counter("endpoint_builds") > builds, 15_000) !== null);
  const cleared = await until(() => counter("bound_at_ms") === undefined, 15_000);
  check("a clean close clears the marker", cleared !== null, `bound_at_ms ${counter("bound_at_ms")}`);

  //    A trap while the endpoint is bound leaves the marker set; the next constructor logs
  //    `evicted_active`.
  const wakes1 = counter("wakes");
  peer.stdin.write("echo x iroh-do-test/trap/0\n");
  const marked = await until(() => counter("bound_at_ms"), 15_000);
  check("bound marker set while the endpoint was bound", marked !== null);
  await until(() => lines.filter((l) => l.startsWith("echo")).length >= 2, 15_000);
  await mf.dispatchFetch(`http://admin.localhost:${port}/status/${E}`);
  check("the trap re-ran the constructor", await until(() => counter("wakes") > wakes1, 15_000) !== null,
    `wakes ${wakes1} -> ${counter("wakes")}`);
  check("evicted_active logged", await until(() => logged("evicted_active", `endpoint: '${E}'`), 5_000));
} catch (e) {
  console.log(`FAIL exception: ${e?.stack ?? e}`);
  failures++;
} finally {
  peer?.stdin.end();
  peer?.kill();
  await mf.dispose();
  rmSync(persistDir, { recursive: true, force: true });
}
console.log(failures ? `${failures} FAILURE(S)` : "ALL PASS");
process.exit(failures ? 1 : 0);
