// Connection lifetime on Miniflare (README.md, "Connection lifetime"; design note 013).
//
//   npm run build:test && cargo build -p iroh-do-test-peer && node test/lifetime-test.mjs
//
// Uses the test app (test/fixture.ts, bundle in test/dist) and the stock test peer:
//
// 1. A handler that returns closes its connection: code 0, reason `done`, at once, not after
//    t_idle.
// 2. Stream halves the app abandoned unfinished but still references stop counting when
//    their connection closes: `open_streams` returns to 0, and the object reaches Warm.
// 3. A handler that keeps its promise pending keeps the connection (echo).
// 4. A dialed connection (`endpoint.connect`) is not closed when the call that dialed it
//    returns; the idle close ends it after t_idle.
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterMs, check, fail, finish, sleep, startPeer, startWorker } from "./harness.mjs";

const dist = join(fileURLToPath(import.meta.url), "..", "dist");
const port = Number(process.env.PORT ?? 8819);
const w = startWorker({ dist, script: "iroh-do-test", className: "Probe", port });
let peer;
try {
  await w.mf.ready;
  const { id: E } = await w.mint();
  peer = startPeer(E, port);
  const ready = await peer.next();
  const peerId = ready.split(" ")[1];
  check("peer ready", ready.startsWith("ready") && peerId?.length === 52, ready);

  // 3. Echo: the handler awaits conn.closed(), so the peer's close ends it.
  peer.send("echo hello");
  const echo = await peer.next();
  check("held connection: echo round trip", echo.startsWith("echo: hello"), echo);

  // 1. The handler returns after reading the stream: the node closes with `done`.
  peer.send("send x iroh-do-test/done/0");
  const done = await peer.next();
  check("settled handler: connection closed with code 0, reason done, before t_idle",
    /closed by peer: done \(code 0\)/.test(done) && afterMs(done) < 2_000, done);

  // 2. The handler keeps the halves of an unfinished stream and returns.
  peer.send("send x iroh-do-test/keep/0");
  const keep = await peer.next();
  check("abandoned halves: connection closed with reason done",
    /closed by peer: done \(code 0\)/.test(keep) && afterMs(keep) < 2_000, keep);
  const kept = await (await w.admin(`/test/kept/${E}`)).json();
  const s1 = await w.status(E);
  check("abandoned halves still referenced by the app no longer count as open",
    kept.kept === 1 && s1.open_streams === 0 && s1.connections === 0,
    `kept=${kept.kept} open_streams=${s1.open_streams} connections=${s1.connections}`);
  await sleep(4_000);
  const s2 = await w.status(E);
  check("Warm after t_linger, no tasks", s2.state === "Warm" && s2.tasks === 0, `state=${s2.state} tasks=${s2.tasks}`);

  // 4. Dial-back over the peer's relay socket.
  const dialRes = await w.admin(`/test/dial/${E}?peer=${peerId}`);
  const dial = await dialRes.json();
  check("dial-back: endpoint.connect reaches the peer over its relay socket", dial.reply === "ok:back", JSON.stringify(dial));
  await sleep(2_000);
  const s3 = await w.status(E);
  check("dialed connection stays open after the dialing call returned", s3.connections === 1 && s3.state === "Active",
    `connections=${s3.connections} state=${s3.state}`);
  const back = await peer.next(/^back/, 30_000);
  check("dialed connection closed by the idle close after ~10 s", /closed by peer: idle \(code 0\)/.test(back) && afterMs(back) >= 9_500 && afterMs(back) < 13_500, back);

  const opens = w.events("conn_open").length;
  const closes = w.events("conn_close");
  check("conn_open and conn_close events logged", opens >= 4 && closes.length >= 4 && closes.some((l) => /done/.test(l)),
    `conn_open=${opens} conn_close=${closes.length}`);
  console.log(`info ${closes.find((l) => /done/.test(l))}`);
} catch (e) {
  fail(e);
} finally {
  peer?.stop();
  await w.dispose();
}
finish();

