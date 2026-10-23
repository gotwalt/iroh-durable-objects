// Runs the golden fixtures (examples/chat/common/fixtures) against this chat server, as
// examples/ts-deployment serves it under `wrangler dev`:
//
//   npm run test:fixtures
//
// 1. Builds examples/ts-deployment (the wasm library, the iroh-do package and the bundle).
// 2. Starts `wrangler dev` on a free local port, with an admin token generated for this run.
// 3. Runs `cargo test -p iroh-do-chat-app --test fixtures fixtures` with FIXTURE_TARGET and
//    FIXTURE_ADMIN_TOKEN, which mints one chat endpoint per fixture.
// 4. Stops wrangler, by the process group it was started in, however the run ends.
//
// Exits with cargo's status. Nothing is contacted but 127.0.0.1.
import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { createServer } from "node:net";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repo = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..", "..");
const deployment = join(repo, "examples", "ts-deployment");
/** How long wrangler may take to answer its first request. */
const READY_MS = 120_000;
/** How long wrangler may take to exit after SIGTERM before it is killed. */
const STOP_MS = 10_000;

/** A TCP port on 127.0.0.1 that nothing listens on now. */
function freePort() {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      server.close(() => resolve(typeof address === "object" && address !== null ? address.port : 0));
    });
  });
}

/** Runs `command` to completion with inherited stdio; resolves with its exit code. */
function run(command, args, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: "inherit", ...options });
    child.once("error", reject);
    child.once("exit", (code, signal) => resolve(code ?? (signal ? 1 : 0)));
  });
}

/** Starts wrangler dev in its own process group, so that stopping it stops workerd too. */
function startWrangler(port, inspectorPort, token) {
  const args = ["wrangler", "dev", "--local", "--ip", "127.0.0.1", "--port", String(port)];
  args.push("--inspector-port", String(inspectorPort), "--var", `ADMIN_TOKEN:${token}`);
  const child = spawn("npx", args, { cwd: deployment, stdio: ["ignore", "pipe", "pipe"], detached: true });
  const exited = new Promise((resolve) => child.once("exit", resolve));
  let output = "";
  const keep = (chunk) => {
    output = (output + chunk).slice(-8_000);
    if (process.env.VERBOSE) process.stderr.write(chunk);
  };
  child.stdout.on("data", keep);
  child.stderr.on("data", keep);
  return {
    pid: child.pid,
    exited,
    output: () => output,
    async stop() {
      if (child.exitCode !== null || child.signalCode !== null) return;
      signalGroup(child.pid, "SIGTERM");
      const stopped = await Promise.race([exited.then(() => true), sleep(STOP_MS).then(() => false)]);
      if (!stopped) signalGroup(child.pid, "SIGKILL");
      await exited;
    },
  };
}

/** Signals the process group led by `pid`, which this script started; ignores one that is gone. */
function signalGroup(pid, signal) {
  try {
    process.kill(-pid, signal);
  } catch (error) {
    if (error.code !== "ESRCH") throw error;
  }
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Resolves once the server on `port` answers HTTP; rejects after READY_MS or if it exits. */
async function ready(port, wrangler) {
  const deadline = Date.now() + READY_MS;
  let exited = false;
  wrangler.exited.then(() => {
    exited = true;
  });
  while (Date.now() < deadline) {
    if (exited) throw new Error(`wrangler dev exited:\n${wrangler.output()}`);
    try {
      await fetch(`http://127.0.0.1:${port}/`, { signal: AbortSignal.timeout(2_000) });
      return;
    } catch {
      await sleep(500);
    }
  }
  throw new Error(`wrangler dev did not answer within ${READY_MS / 1000} s:\n${wrangler.output()}`);
}

async function main() {
  const built = await run("npm", ["run", "build"], { cwd: deployment });
  if (built !== 0) return built;

  const port = await freePort();
  const inspectorPort = await freePort();
  const token = randomBytes(24).toString("hex");
  const wrangler = startWrangler(port, inspectorPort, token);
  // Interrupted: stop wrangler, then exit as an interrupted process does.
  const stop = () => {
    wrangler.stop().then(
      () => process.exit(130),
      () => process.exit(130),
    );
  };
  process.once("SIGINT", stop);
  process.once("SIGTERM", stop);
  try {
    await ready(port, wrangler);
    console.log(`wrangler dev (pid ${wrangler.pid}) on 127.0.0.1:${port}`);
    return await run("cargo", ["test", "-p", "iroh-do-chat-app", "--test", "fixtures", "fixtures"], {
      cwd: repo,
      env: { ...process.env, FIXTURE_TARGET: String(port), FIXTURE_ADMIN_TOKEN: token },
    });
  } finally {
    await wrangler.stop();
    process.off("SIGINT", stop);
    process.off("SIGTERM", stop);
  }
}

main().then(
  (code) => process.exit(code),
  (error) => {
    console.error(error);
    process.exit(1);
  },
);
