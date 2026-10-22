//! Runs the golden fixtures in `examples/chat/common/fixtures/` against the Rust chat app.
//!
//! The fixtures define the wire behavior of `iroh-do/room/1`, `iroh-do/ctl/1`,
//! `iroh-do/echo/1` and `iroh-do/notify/1` for every implementation of the chat server (see
//! `examples/chat/common/PROTOCOL.md` and the README next to the fixtures for the format).
//!
//! Each fixture starts a new object and one stock iroh endpoint per named member, with the
//! object's relay as its home relay as the chat client configures it. Steps run in order. After
//! each step the runner collects what the object delivered unprompted, so every delivery is
//! attributed to the step that caused it.
//!
//! `RECORD_FIXTURES=1 cargo test -p iroh-do-chat-app --test fixtures` fills in the `expect`,
//! `expect_raw` and `deliveries` of steps that have none, from what this server does, and
//! writes the files back. Steps that already have an expectation are verified, never rewritten.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use iroh::{
    Endpoint, EndpointAddr, RelayMap, RelayMode, RelayUrl,
    endpoint::{Connection, RecvStream, SendStream, presets},
};
use iroh_do_chat_app::{
    CTL_ALPN, ChatApp, DELIVERY_BATCH, ECHO_ALPN, MAX_CALLBACK_SECS, MAX_LOG_TEXT, NOTIFY_ALPN,
    ROOM_ALPN, store,
};
use iroh_do_harness::{DoObject, Harness, Timings};
use serde_json::{Map, Value, json};
use tokio::{
    sync::{Mutex, mpsc},
    time::{sleep, timeout},
};

/// How long a listed delivery may take to arrive.
const DELIVERY_WAIT: Duration = Duration::from_secs(5);
/// How long the runner waits after a step for deliveries that should not happen.
const SETTLE: Duration = Duration::from_millis(250);
/// The same, when recording.
const SETTLE_RECORD: Duration = Duration::from_millis(500);
const IO_TIMEOUT: Duration = Duration::from_secs(10);

fn record_mode() -> bool {
    std::env::var_os("RECORD_FIXTURES").is_some()
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../common/fixtures")
}

type Binds = HashMap<String, Value>;

/// One member: a stock iroh endpoint whose home relay is the object.
struct Actor {
    ep: Endpoint,
    addr: EndpointAddr,
    lines: Mutex<mpsc::UnboundedReceiver<String>>,
    tx: mpsc::UnboundedSender<String>,
    ctl: Mutex<Option<CtlSession>>,
}

struct CtlSession {
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
    buf: Vec<u8>,
}

impl Actor {
    async fn new(url: &RelayUrl, id: iroh::EndpointId) -> Arc<Self> {
        let ep = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Custom(RelayMap::from(url.clone())))
            .alpns(vec![ROOM_ALPN.to_vec(), NOTIFY_ALPN.to_vec()])
            .bind()
            .await
            .expect("bind member");
        timeout(Duration::from_secs(15), ep.online())
            .await
            .expect("home relay connected");
        let (tx, rx) = mpsc::unbounded_channel();
        let (ep2, tx2) = (ep.clone(), tx.clone());
        tokio::spawn(async move {
            while let Some(inc) = ep2.accept().await {
                let Ok(accepting) = inc.accept() else {
                    continue;
                };
                let Ok(conn) = accepting.await else { continue };
                tokio::spawn(read_unprompted(conn, tx2.clone()));
            }
        });
        Arc::new(Self {
            addr: EndpointAddr::new(id).with_relay_url(url.clone()),
            ep,
            lines: Mutex::new(rx),
            tx,
            ctl: Mutex::new(None),
        })
    }

    async fn connect(&self, alpn: &[u8]) -> Connection {
        let conn = self
            .ep
            .connect(self.addr.clone(), alpn)
            .await
            .expect("connect");
        tokio::spawn(read_unprompted(conn.clone(), self.tx.clone()));
        conn
    }
}

/// Pushes every line of every uni stream of `conn` to `tx`.
async fn read_unprompted(conn: Connection, tx: mpsc::UnboundedSender<String>) {
    while let Ok(mut uni) = conn.accept_uni().await {
        let Ok(bytes) = uni.read_to_end(1 << 20).await else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        for l in text.split_terminator('\n') {
            let _ = tx.send(l.to_string());
        }
    }
}

// ---- patterns -------------------------------------------------------------------------

/// A piece of a byte string or text line.
#[derive(Debug)]
enum Seg {
    Lit(Vec<u8>),
    /// One or more ASCII digits.
    Int,
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Resolves a parts value (a string, a part object, or an array of those) to segments.
fn segs(parts: &Value, b: &Binds) -> Result<Vec<Seg>, String> {
    let mut out = Vec::new();
    match parts {
        Value::Array(a) => {
            for p in a {
                out.extend(segs(p, b)?);
            }
        }
        Value::String(s) => out.push(Seg::Lit(s.as_bytes().to_vec())),
        Value::Object(o) if o.len() == 1 => {
            let (k, v) = o.iter().next().unwrap();
            match k.as_str() {
                "$repeat" => {
                    let a = v.as_array().filter(|a| a.len() == 2).ok_or("bad $repeat")?;
                    let s = a[0].as_str().ok_or("bad $repeat text")?;
                    let n = a[1].as_u64().ok_or("bad $repeat count")? as usize;
                    out.push(Seg::Lit(s.repeat(n).into_bytes()));
                }
                "$hex" => out.push(Seg::Lit(
                    hex_decode(v.as_str().ok_or("bad $hex")?).ok_or("bad $hex digits")?,
                )),
                "$ref" => {
                    let name = v.as_str().ok_or("bad $ref")?;
                    let val = b.get(name).ok_or(format!("unbound $ref {name}"))?;
                    out.push(Seg::Lit(value_text(val).into_bytes()));
                }
                "$any" if v == "int" => out.push(Seg::Int),
                _ => return Err(format!("bad part {parts}")),
            }
        }
        other => return Err(format!("bad part {other}")),
    }
    Ok(out)
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok())
        .collect()
}

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The bytes a request is made of.
fn request_bytes(parts: &Value, b: &Binds) -> Vec<u8> {
    let mut out = Vec::new();
    for s in segs(parts, b).expect("send parts") {
        match s {
            Seg::Lit(l) => out.extend(l),
            Seg::Int => panic!("$any in a request"),
        }
    }
    out
}

fn match_bytes(parts: &Value, actual: &[u8], b: &Binds) -> Result<(), String> {
    let mut rest = actual;
    for s in segs(parts, b)? {
        match s {
            Seg::Lit(l) => {
                rest = rest
                    .strip_prefix(l.as_slice())
                    .ok_or_else(|| format!("expected {:?}, got {:?}", lossy(&l), lossy(rest)))?;
            }
            Seg::Int => {
                let n = rest.iter().take_while(|c| c.is_ascii_digit()).count();
                if n == 0 {
                    return Err(format!("expected digits, got {:?}", lossy(rest)));
                }
                rest = &rest[n..];
            }
        }
    }
    if rest.is_empty() {
        Ok(())
    } else {
        Err(format!("trailing {:?}", lossy(rest)))
    }
}

fn lossy(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(b);
    if s.len() > 120 {
        format!("{}...({} bytes)", &s[..s.floor_char_boundary(120)], s.len())
    } else {
        s.into_owned()
    }
}

fn is_placeholder<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    if o.len() == 1 || (key == "$capture" && o.len() == 2 && o.contains_key("as")) {
        o.get(key)
    } else {
        None
    }
}

fn any_matches(ty: &str, v: &Value) -> bool {
    match ty {
        "int" => v.is_u64() || v.is_i64(),
        "ts" => v.as_u64().is_some_and(|t| t > 1_600_000_000_000),
        "string" => v.is_string(),
        "bool" => v.is_boolean(),
        "any" => true,
        _ => false,
    }
}

/// Matches a parsed JSON value against a pattern.
fn match_value(pat: &Value, v: &Value, b: &mut Binds, at: &str) -> Result<(), String> {
    match pat {
        Value::Object(o) => {
            if let Some(ty) = is_placeholder(o, "$any") {
                let ty = ty.as_str().ok_or("bad $any")?;
                return if any_matches(ty, v) {
                    Ok(())
                } else {
                    Err(format!("{at}: {v} is not {ty}"))
                };
            }
            if let Some(name) = is_placeholder(o, "$capture") {
                let name = name.as_str().ok_or("bad $capture")?;
                if let Some(ty) = o.get("as")
                    && !any_matches(ty.as_str().ok_or("bad as")?, v)
                {
                    return Err(format!("{at}: {v} is not {ty}"));
                }
                if b.insert(name.to_string(), v.clone()).is_some() {
                    return Err(format!("$capture {name} is already bound"));
                }
                return Ok(());
            }
            if let Some(name) = is_placeholder(o, "$ref") {
                let name = name.as_str().ok_or("bad $ref")?;
                let want = b.get(name).ok_or(format!("unbound $ref {name}"))?;
                return if want == v {
                    Ok(())
                } else {
                    Err(format!("{at}: {v} is not {name} = {want}"))
                };
            }
            if let Some(parts) = is_placeholder(o, "$text") {
                let s = v.as_str().ok_or(format!("{at}: {v} is not a string"))?;
                return match_bytes(parts, s.as_bytes(), b).map_err(|e| format!("{at}: {e}"));
            }
            let obj = v.as_object().ok_or(format!("{at}: {v} is not an object"))?;
            let mut want: Vec<_> = o.keys().collect();
            let mut got: Vec<_> = obj.keys().collect();
            want.sort();
            got.sort();
            if want != got {
                return Err(format!("{at}: keys {got:?}, expected {want:?}"));
            }
            for (k, p) in o {
                match_value(p, &obj[k], b, &format!("{at}.{k}"))?;
            }
            Ok(())
        }
        Value::Array(a) => {
            let arr = v.as_array().ok_or(format!("{at}: {v} is not an array"))?;
            if a.len() != arr.len() {
                return Err(format!("{at}: {} items, expected {}", arr.len(), a.len()));
            }
            for (i, (p, x)) in a.iter().zip(arr).enumerate() {
                match_value(p, x, b, &format!("{at}[{i}]"))?;
            }
            Ok(())
        }
        lit if lit == v => Ok(()),
        lit => Err(format!("{at}: got {v}, expected {lit}")),
    }
}

/// Matches one received line against an expectation entry: a string or `{"$text": ...}` for a
/// text line, anything else for a JSON line.
fn match_line(pat: &Value, line: &str, b: &mut Binds) -> Result<(), String> {
    match pat {
        Value::String(_) => match_bytes(pat, line.as_bytes(), b),
        Value::Object(o) if o.len() == 1 && o.contains_key("$text") => {
            match_bytes(&o["$text"], line.as_bytes(), b)
        }
        _ => {
            let v: Value =
                serde_json::from_str(line).map_err(|e| format!("not JSON ({e}): {line:?}"))?;
            match_value(pat, &v, b, "$")
        }
    }
}

// ---- recording --------------------------------------------------------------------------

/// Turns an observed line into an expectation entry.
fn templatize_line(line: &str, ids: &[(String, String)]) -> Value {
    let jsonish = line.starts_with('{') || line.starts_with('[');
    if jsonish && let Ok(v) = serde_json::from_str::<Value>(line) {
        return templatize_value(&v, "", ids);
    }
    let parts = text_parts(line, ids);
    if parts.len() == 1 && parts[0].is_string() {
        parts[0].clone()
    } else {
        json!({ "$text": parts })
    }
}

/// Splits `s` at known ids and counters: `wakes=<n>` varies with the host.
fn text_parts(s: &str, ids: &[(String, String)]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut rest = s;
    'outer: while !rest.is_empty() {
        for (name, id) in ids {
            if name.starts_with("peer8:") {
                continue;
            }
            if let Some(tail) = rest.strip_prefix(id.as_str()) {
                out.push(json!({ "$ref": name }));
                rest = tail;
                continue 'outer;
            }
        }
        if let Some(tail) = rest.strip_prefix("wakes=") {
            let n = tail.bytes().take_while(u8::is_ascii_digit).count();
            if n > 0 {
                out.push(Value::String("wakes=".into()));
                out.push(json!({ "$any": "int" }));
                rest = &tail[n..];
                continue;
            }
        }
        let c = rest.chars().next().unwrap();
        match out.last_mut() {
            Some(Value::String(l)) => l.push(c),
            _ => out.push(Value::String(c.to_string())),
        }
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn templatize_value(v: &Value, key: &str, ids: &[(String, String)]) -> Value {
    match v {
        Value::String(s) => match ids.iter().find(|(_, id)| id == s) {
            Some((name, _)) => json!({ "$ref": name }),
            None => v.clone(),
        },
        Value::Number(_) if key == "ts" || key == "ts_ms" => json!({ "$any": "ts" }),
        Value::Array(a) => Value::Array(a.iter().map(|x| templatize_value(x, key, ids)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), templatize_value(x, k, ids)))
                .collect(),
        ),
        other => other.clone(),
    }
}

// ---- running ----------------------------------------------------------------------------

struct Run {
    obj: Arc<DoObject>,
    actors: Vec<(String, Arc<Actor>)>,
    binds: Binds,
    /// `(name, value)` for `peer:*`, `peer8:*` and `room`.
    ids: Vec<(String, String)>,
}

impl Run {
    fn actor(&self, name: &str) -> Arc<Actor> {
        self.actors
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("unknown member {name}"))
            .1
            .clone()
    }
}

/// Sends a room request on a new connection; returns the reply lines.
async fn room_request(a: &Actor, bytes: &[u8]) -> Vec<String> {
    let conn = a.connect(ROOM_ALPN).await;
    let (mut send, mut recv) = conn.open_bi().await.expect("open_bi");
    send.write_all(bytes).await.expect("write");
    send.finish().expect("finish");
    let reply = timeout(IO_TIMEOUT, recv.read_to_end(1 << 20))
        .await
        .expect("reply in time")
        .expect("read reply");
    conn.close(0u32.into(), b"done");
    split_reply(&reply)
}

fn split_reply(reply: &[u8]) -> Vec<String> {
    if reply.is_empty() {
        return Vec::new();
    }
    let text = std::str::from_utf8(reply).expect("reply is utf-8");
    assert!(
        text.ends_with('\n'),
        "reply not newline-terminated: {text:?}"
    );
    text[..text.len() - 1]
        .split('\n')
        .map(String::from)
        .collect()
}

/// Sends `bytes` on the member's ctl stream (opening it if needed). With `finish`, ends the
/// stream and reads to its end; otherwise reads one reply line.
async fn ctl_request(a: &Actor, bytes: &[u8], finish: bool) -> Vec<String> {
    let mut slot = a.ctl.lock().await;
    if slot.is_none() {
        let conn = a.connect(CTL_ALPN).await;
        let (send, recv) = conn.open_bi().await.expect("open ctl stream");
        *slot = Some(CtlSession {
            conn,
            send,
            recv,
            buf: Vec::new(),
        });
    }
    let s = slot.as_mut().unwrap();
    s.send.write_all(bytes).await.expect("write");
    if finish {
        s.send.finish().expect("finish");
        let rest = timeout(IO_TIMEOUT, s.recv.read_to_end(1 << 20))
            .await
            .expect("reply in time")
            .expect("read reply");
        let mut all = std::mem::take(&mut s.buf);
        all.extend(rest);
        s.conn.close(0u32.into(), b"done");
        *slot = None;
        return split_reply(&all);
    }
    loop {
        if let Some(p) = s.buf.iter().position(|&c| c == b'\n') {
            let line: Vec<u8> = s.buf.drain(..=p).collect();
            return vec![String::from_utf8(line[..line.len() - 1].to_vec()).expect("utf-8")];
        }
        let mut chunk = [0u8; 4096];
        let n = timeout(IO_TIMEOUT, s.recv.read(&mut chunk))
            .await
            .expect("reply in time")
            .expect("read")
            .expect("stream ended before a reply line");
        s.buf.extend_from_slice(&chunk[..n]);
    }
}

async fn echo_request(a: &Actor, bytes: &[u8]) -> Vec<u8> {
    let conn = a.connect(ECHO_ALPN).await;
    let (mut send, mut recv) = conn.open_bi().await.expect("open_bi");
    send.write_all(bytes).await.expect("write");
    send.finish().expect("finish");
    let reply = timeout(IO_TIMEOUT, recv.read_to_end(1 << 20))
        .await
        .expect("echo in time")
        .expect("read echo");
    conn.close(0u32.into(), b"done");
    reply
}

async fn run_fixture(h: Arc<Harness>, path: PathBuf) -> Result<(), String> {
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut fx: Value = serde_json::from_str(&text).map_err(|e| format!("parse: {e}"))?;
    let record = record_mode();
    let obj = h.mint::<ChatApp>(Timings::default());
    let url: RelayUrl = h.relay_url(&obj.id()).parse().expect("relay url");
    let names: Vec<String> = fx["members"]
        .as_array()
        .ok_or("members")?
        .iter()
        .map(|m| m.as_str().unwrap().to_string())
        .collect();
    let mut run = Run {
        obj,
        actors: Vec::new(),
        binds: Binds::new(),
        ids: Vec::new(),
    };
    run.binds
        .insert("room".into(), run.obj.id().to_z32().into());
    run.ids.push(("room".into(), run.obj.id().to_z32()));
    for n in &names {
        let a = Actor::new(&url, run.obj.id()).await;
        let z = a.ep.id().to_z32();
        run.binds.insert(format!("peer:{n}"), z.clone().into());
        run.binds
            .insert(format!("peer8:{n}"), z[..8].to_string().into());
        run.ids.push((format!("peer:{n}"), z.clone()));
        run.ids.push((format!("peer8:{n}"), z[..8].to_string()));
        run.actors.push((n.clone(), a));
    }
    let mut changed = false;
    let steps = fx["steps"].as_array_mut().ok_or("steps")?;
    for (i, step) in steps.iter_mut().enumerate() {
        let note = step["note"].as_str().unwrap_or("").to_string();
        run_step(&mut run, step, record, &mut changed)
            .await
            .map_err(|e| format!("step {i} ({note}): {e}"))?;
    }
    for (_, a) in &run.actors {
        a.ep.close().await;
    }
    if changed {
        let mut out = serde_json::to_string_pretty(&fx).unwrap();
        out.push('\n');
        std::fs::write(&path, out).map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn run_step(
    run: &mut Run,
    step: &mut Value,
    record: bool,
    changed: &mut bool,
) -> Result<(), String> {
    if let Some(ms) = step.get("sleep_ms").and_then(Value::as_u64) {
        sleep(Duration::from_millis(ms)).await;
        return Ok(());
    }
    let member = step["member"].as_str().ok_or("member")?.to_string();
    let proto = step
        .get("proto")
        .and_then(Value::as_str)
        .unwrap_or("room")
        .to_string();
    let a = run.actor(&member);
    let bytes = request_bytes(&step["send"], &run.binds);
    let times = step.get("times").and_then(Value::as_u64).unwrap_or(1);
    let finish = step.get("finish").and_then(Value::as_bool).unwrap_or(false);
    let unrecorded = step.get("expect").is_none() && step.get("expect_raw").is_none();
    if unrecorded && !record {
        return Err("no expectation; run with RECORD_FIXTURES=1 to record it".into());
    }
    let mut recorded: Option<Vec<String>> = None;
    let mut recorded_raw: Option<Vec<u8>> = None;
    for _ in 0..times {
        if proto == "echo" {
            let got = echo_request(&a, &bytes).await;
            if unrecorded {
                recorded_raw = Some(got);
            } else {
                match_bytes(&step["expect_raw"], &got, &run.binds)
                    .map_err(|e| format!("echo reply: {e}"))?;
            }
            continue;
        }
        let lines = match proto.as_str() {
            "room" => room_request(&a, &bytes).await,
            "ctl" => ctl_request(&a, &bytes, finish).await,
            p => return Err(format!("unknown proto {p}")),
        };
        if unrecorded {
            recorded = Some(lines);
            continue;
        }
        let want = step["expect"].as_array().ok_or("expect must be an array")?;
        if want.len() != lines.len() {
            return Err(format!(
                "reply has {} lines, expected {}: {lines:?}",
                lines.len(),
                want.len()
            ));
        }
        for (j, (p, l)) in want.iter().zip(&lines).enumerate() {
            match_line(p, l, &mut run.binds).map_err(|e| format!("reply line {j}: {e}"))?;
        }
    }

    // Unprompted lines.
    if record {
        sleep(SETTLE_RECORD).await;
    } else {
        if let Some(d) = step.get("deliveries").and_then(Value::as_object) {
            for (who, entries) in d {
                let ma = run.actor(who);
                let mut rx = ma.lines.lock().await;
                for (j, p) in entries.as_array().ok_or("deliveries")?.iter().enumerate() {
                    let line = timeout(DELIVERY_WAIT, rx.recv())
                        .await
                        .map_err(|_| format!("{who}: delivery {j} did not arrive"))?
                        .ok_or("channel closed")?;
                    match_line(p, &line, &mut run.binds)
                        .map_err(|e| format!("{who}: delivery {j}: {e}"))?;
                }
            }
        }
        sleep(SETTLE).await;
    }
    let mut got: Vec<(String, Vec<String>)> = Vec::new();
    for (n, ma) in &run.actors {
        let mut rx = ma.lines.lock().await;
        let mut lines = Vec::new();
        while let Ok(l) = rx.try_recv() {
            lines.push(l);
        }
        if !lines.is_empty() {
            got.push((n.clone(), lines));
        }
    }
    if record && unrecorded {
        let ids = &run.ids;
        let obj = step.as_object_mut().unwrap();
        if let Some(raw) = recorded_raw {
            obj.insert("expect_raw".into(), raw_template(&raw));
        } else {
            let lines = recorded.unwrap_or_default();
            obj.insert(
                "expect".into(),
                Value::Array(lines.iter().map(|l| templatize_line(l, ids)).collect()),
            );
        }
        if !got.is_empty() {
            let d: Map<String, Value> = got
                .iter()
                .map(|(n, ls)| {
                    (
                        n.clone(),
                        Value::Array(ls.iter().map(|l| templatize_line(l, ids)).collect()),
                    )
                })
                .collect();
            obj.insert("deliveries".into(), Value::Object(d));
        }
        *changed = true;
    } else if !got.is_empty() && !record {
        return Err(format!("unexpected deliveries: {got:?}"));
    }
    Ok(())
}

fn raw_template(raw: &[u8]) -> Value {
    match std::str::from_utf8(raw) {
        Ok(s) => Value::String(s.to_string()),
        Err(_) => json!([{ "$hex": hex_encode(raw) }]),
    }
}

/// The fixtures, each on a new object, in file name order.
#[tokio::test(flavor = "multi_thread")]
async fn fixtures() {
    let h = Arc::new(Harness::start().await.unwrap());
    let mut files: Vec<_> = std::fs::read_dir(fixture_dir())
        .expect("fixtures directory")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no fixtures");
    // Each fixture has its own object and members, so they run concurrently.
    let mut tasks = Vec::new();
    for f in files {
        let name = f.file_name().unwrap().to_string_lossy().into_owned();
        tasks.push((name, tokio::spawn(run_fixture(h.clone(), f))));
    }
    let mut failures = Vec::new();
    for (name, t) in tasks {
        match t.await {
            Ok(Ok(())) => println!("fixture {name}: ok"),
            Ok(Err(e)) => failures.push(format!("{name}: {e}")),
            Err(e) => failures.push(format!("{name}: panicked: {e}")),
        }
    }
    assert!(
        failures.is_empty(),
        "fixtures failed:\n{}",
        failures.join("\n")
    );
}

/// The constants file that PROTOCOL.md and the client test also check agrees with this app.
#[test]
fn constants_match_the_app() {
    let c: Value = serde_json::from_str(
        &std::fs::read_to_string(fixture_dir().join("../constants.json")).expect("constants.json"),
    )
    .unwrap();
    let alpn = |k: &str| c["alpn"][k].as_str().unwrap().as_bytes().to_vec();
    assert_eq!(alpn("room"), ROOM_ALPN);
    assert_eq!(alpn("ctl"), CTL_ALPN);
    assert_eq!(alpn("echo"), ECHO_ALPN);
    assert_eq!(alpn("notify"), NOTIFY_ALPN);
    let l = &c["limits"];
    assert_eq!(l["max_callback_secs"], MAX_CALLBACK_SECS);
    assert_eq!(l["max_log_text"], MAX_LOG_TEXT);
    assert_eq!(l["delivery_batch_ms"], DELIVERY_BATCH.as_millis() as u64);
    assert_eq!(l["log_retention_rows"], store::LOG_RETENTION);
    assert_eq!(l["log_max_age_secs"], store::LOG_MAX_AGE.as_secs());
}
