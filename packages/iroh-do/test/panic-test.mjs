// Panic recovery on Miniflare (design note 013, "Panics").
//
//   npm run build:test && cargo build -p iroh-do-test-peer && node test/panic-test.mjs
//
// The test bundle (test/dist) is built with the wasm module's `test-panic` feature, which
// exports `testPanic`. All objects of a Worker share one isolate in workerd, and with it
// one wasm instance. Two endpoints A and B, each with a stock peer holding a relay socket:
//
// 1. A panics inside a call A makes (the RPC `testPanic(false)`). A catches the trap,
//    instantiates the module again and rebuilds its node at once.
// 2. B's next event finds its node built for the old instance and rebuilds it from its
//    socket attachments and storage, as a wake does. Its relay socket is the same.
// 3. A panics in a task on a later microtask, where no object can catch the trap. The panic
//    hook records it; the next event of each object rebuilds.
// 4. After each panic both peers echo again over their existing relay sockets.
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { check, fail, finish, sleep, startPeer, startWorker } from "./harness.mjs";

const dist = join(fileURLToPath(import.meta.url), "..", "dist");
const port = Number(process.env.PORT ?? 8820);
const w = startWorker({ dist, script: "iroh-do-test", className: "Probe", port });
const peers = [];
try {
  await w.mf.ready;
  const A = (await w.mint()).id;
  const B = (await w.mint()).id;
  const pa = startPeer(A, port);
  const pb = startPeer(B, port);
  peers.push(pa, pb);
  check("peers ready", (await pa.next()).startsWith("ready") && (await pb.next()).startsWith("ready"));
  const echo = async (p, text) => {
    p.send(`echo ${text}`);
    return p.next();
  };
  const a0 = await echo(pa, "a0");
  const b0 = await echo(pb, "b0");
  check("echo on A and B before any panic", a0.startsWith("echo: a0") && b0.startsWith("echo: b0"), `${a0} | ${b0}`);
  const sa0 = await w.status(A);
  const sb0 = await w.status(B);
  check("both nodes on wasm generation 0", sa0.generation === 0 && sb0.generation === 0,
    `A=${sa0.generation} B=${sb0.generation}`);

  // 1. Panic in a call A makes.
  const r1 = await w.admin(`/test/panic/${A}?task=0`, { method: "POST" });
  const sa1 = await w.status(A);
  check("panic in A's call: A rebuilt its node on generation 1, same socket",
    sa1.generation === 1 && sa1.rebuilds === 1 && JSON.stringify(sa1.sockets) === JSON.stringify(sa0.sockets),
    `rpc=${r1.status} generation=${sa1.generation} rebuilds=${sa1.rebuilds} sockets ${JSON.stringify(sa0.sockets)} -> ${JSON.stringify(sa1.sockets)}`);
  check("wasm_panic and wasm_reset logged", w.events("wasm_panic").length === 1 && w.events("wasm_reset").length === 1,
    `${w.events("wasm_panic")[0]} | ${w.events("wasm_reset")[0]}`);

  // 2. B, untouched by the panic, rebuilds on its next event.
  const sb1 = await w.status(B);
  check("B rebuilt on its next event: generation 1, same socket, wakes unchanged",
    sb1.generation === 1 && sb1.rebuilds === 1 && JSON.stringify(sb1.sockets) === JSON.stringify(sb0.sockets) && sb1.wakes === sb0.wakes,
    `generation=${sb1.generation} rebuilds=${sb1.rebuilds} sockets ${JSON.stringify(sb0.sockets)} -> ${JSON.stringify(sb1.sockets)} wakes ${sb0.wakes} -> ${sb1.wakes}`);
  const b1 = await echo(pb, "b1");
  const a1 = await echo(pa, "a1");
  check("echo on B and A after the panic", b1.startsWith("echo: b1") && a1.startsWith("echo: a1"), `${b1} | ${a1}`);

  // 3. Panic in a task: the trap surfaces in a microtask, outside any object's call.
  await sleep(4_000); // both Warm again
  const r2 = await w.admin(`/test/panic/${A}?task=1`, { method: "POST" });
  await sleep(500);
  check("panic in a task: recorded by the panic hook", w.events("wasm_panic").length === 2 && /in a task/.test(w.events("wasm_panic")[1]),
    `rpc=${r2.status} ${w.events("wasm_panic")[1]}`);
  // B's peer sends first this time: B's message event performs the rebuild.
  const b2 = await echo(pb, "b2");
  const a2 = await echo(pa, "a2");
  const sa2 = await w.status(A);
  const sb2 = await w.status(B);
  check("echo on B and A after the task panic", b2.startsWith("echo: b2") && a2.startsWith("echo: a2"), `${b2} | ${a2}`);
  check("both rebuilt on generation 2, relay sockets kept",
    sa2.generation === 2 && sb2.generation === 2 && JSON.stringify(sa2.sockets) === JSON.stringify(sa0.sockets) && JSON.stringify(sb2.sockets) === JSON.stringify(sb0.sockets),
    `A gen=${sa2.generation} sockets=${JSON.stringify(sa2.sockets)}; B gen=${sb2.generation} sockets=${JSON.stringify(sb2.sockets)}`);
  const rebuilds = w.events("node_rebuild");
  check("node_rebuild logged per object and reset", rebuilds.length === 4, `${rebuilds.length}`);

  // 5. A panic while A has a QUIC connection open. The new instance has no state for it:
  //    the peer learns only when its idle timeout fires, then redials.
  pa.send("idle");
  await sleep(1_000);
  const sa3 = await w.status(A);
  check("A Active with one connection before the panic", sa3.state === "Active" && sa3.connections === 1,
    `state=${sa3.state} connections=${sa3.connections}`);
  await w.admin(`/test/panic/${A}?task=1`, { method: "POST" });
  await sleep(200);
  const sa4 = await w.status(A);
  check("after the panic A is rebuilt with no connection", sa4.generation === 3 && sa4.connections === 0,
    `generation=${sa4.generation} connections=${sa4.connections} state=${sa4.state}`);
  const lost = await pa.next(/^closed/, 60_000);
  check("the peer's open connection ends by its own idle timeout", /timed out/.test(lost), lost);
  const a3 = await echo(pa, "a3");
  const sa5 = await w.status(A);
  check("the peer redials over the same relay socket", a3.startsWith("echo: a3") && JSON.stringify(sa5.sockets) === JSON.stringify(sa0.sockets),
    `${a3} sockets=${JSON.stringify(sa5.sockets)}`);
  const uncaught = w.logs.filter((l) => /previous WASM instance|stale object/i.test(l));
  console.log(`info stale-instance errors logged: ${uncaught.length}${uncaught.length ? ` (${uncaught[0]})` : ""}`);
  for (const l of [...w.events("wasm_panic"), ...w.events("wasm_reset"), ...rebuilds]) console.log(`info ${l}`);
} catch (e) {
  fail(e);
} finally {
  for (const p of peers) p.stop();
  await w.dispose();
}
finish();
