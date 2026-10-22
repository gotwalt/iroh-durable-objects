//! Checks the client's wire types and constants against the golden fixtures in
//! `examples/chat/common/` (`PROTOCOL.md`, `constants.json`, `fixtures/`).
//!
//! The fixtures are the cross-implementation definition of the chat protocols; the Rust chat
//! app runs them in `examples/chat/app/tests/fixtures.rs`. The checks here read the files from
//! the repository and use only the client's own code, so the client keeps no dependency on the
//! app or on iroh-do crates.
//!
//! Every reply and delivered event line in the fixtures is instantiated (placeholders replaced
//! by sample values of their type), parsed with [`Event`] and serialized again. The test fails
//! on a line the client cannot parse and on a field it drops, except the fields in
//! [`DROPPED_ON_PURPOSE`].

use std::{collections::HashMap, fs, path::PathBuf};

use serde_json::{Value, json};

use crate::{
    client::MAX_HISTORY,
    ctl::{Counts, LogRow, Status},
    net::{CTL_ALPN, ECHO_ALPN, NOTIFY_ALPN, ROOM_ALPN},
    room::{Event, MAX_REQUEST, check_nick, check_request},
};

/// Fields of room lines that the client parses past and does not keep.
///
/// `hist` marks events in replies to `/join`, `/history` and `/since`. The client tells
/// history from deliveries by the request it made, so it has no field for it.
const DROPPED_ON_PURPOSE: &[&str] = &["hist"];

/// Nicks the server accepts and the client refuses, because the nick names a state directory
/// (`room::check_nick`).
const CLIENT_STRICTER_NICKS: &[&str] = &[".", ".."];

const SAMPLE_ID: &str = "ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1u";

fn common_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../common")
}

fn read_json(path: PathBuf) -> Value {
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn fixtures() -> Vec<(String, Value)> {
    let mut paths: Vec<_> = fs::read_dir(common_dir().join("fixtures"))
        .expect("fixtures directory")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    assert!(paths.len() >= 10, "fixtures missing: {paths:?}");
    paths
        .into_iter()
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                read_json(p),
            )
        })
        .collect()
}

fn sample(ty: &str) -> Value {
    match ty {
        "int" => json!(1),
        "ts" => json!(1_792_573_500_000_u64),
        "bool" => json!(true),
        _ => json!("x"),
    }
}

/// Instantiates a pattern: a concrete value that the pattern matches.
fn instantiate(pat: &Value, caps: &mut HashMap<String, Value>) -> Value {
    match pat {
        Value::Array(a) => Value::Array(a.iter().map(|x| instantiate(x, caps)).collect()),
        Value::Object(o) if o.len() == 1 && o.contains_key("$any") => {
            sample(o["$any"].as_str().unwrap())
        }
        Value::Object(o) if o.contains_key("$capture") => {
            let v = sample(o["as"].as_str().expect("$capture needs `as`"));
            caps.insert(o["$capture"].as_str().unwrap().to_string(), v.clone());
            v
        }
        Value::Object(o) if o.len() == 1 && o.contains_key("$ref") => {
            let name = o["$ref"].as_str().unwrap();
            if name == "room" || name.starts_with("peer:") {
                json!(SAMPLE_ID)
            } else if name.starts_with("peer8:") {
                json!(&SAMPLE_ID[..8])
            } else {
                caps.get(name)
                    .unwrap_or_else(|| panic!("unbound $ref {name}"))
                    .clone()
            }
        }
        Value::Object(o) if o.len() == 1 && o.contains_key("$text") => {
            let mut out = String::new();
            for p in o["$text"].as_array().expect("$text parts") {
                match p {
                    Value::String(s) => out.push_str(s),
                    other => out.push_str(&text_of(&instantiate(other, caps))),
                }
            }
            json!(out)
        }
        Value::Object(o) if o.len() == 1 && o.contains_key("$repeat") => {
            let r = o["$repeat"].as_array().unwrap();
            json!(
                r[0].as_str()
                    .unwrap()
                    .repeat(r[1].as_u64().unwrap() as usize)
            )
        }
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), instantiate(v, caps)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The request line a step sends, without terminator; `None` if it is not UTF-8.
fn request_line(step: &Value, caps: &mut HashMap<String, Value>) -> Option<String> {
    let parts = match &step["send"] {
        Value::Array(a) => a.clone(),
        s => vec![s.clone()],
    };
    let mut out = String::new();
    for p in &parts {
        match p {
            Value::String(s) => out.push_str(s),
            Value::Object(o) if o.contains_key("$hex") => return None,
            other => out.push_str(&text_of(&instantiate(other, caps))),
        }
    }
    let line = out.strip_suffix('\n').unwrap_or(&out);
    let line = line.strip_suffix('\r').unwrap_or(line);
    Some(line.to_string())
}

/// Parses `v` as a room line and fails if the client cannot, or drops a field.
fn check_room_line(v: &Value, ctx: &str) {
    let line = v.to_string();
    let ev = Event::parse(&line).unwrap_or_else(|e| panic!("{ctx}: cannot parse {line}: {e}"));
    assert!(!matches!(ev, Event::Unknown), "{ctx}: unknown line {line}");
    let mut want = v.clone();
    if let Some(o) = want.as_object_mut() {
        for k in DROPPED_ON_PURPOSE {
            if let Some(x) = o.remove(*k) {
                assert_eq!(x, json!(true), "{ctx}: {k} is only ever true");
            }
        }
    }
    let got = serde_json::to_value(&ev).unwrap();
    assert_eq!(
        got, want,
        "{ctx}: the client drops or changes fields of {line}"
    );
}

/// Parses a ctl JSON reply with the client's ctl types.
fn check_ctl_line(v: &Value, ctx: &str) {
    let line = v.to_string();
    match v {
        Value::Array(_) => {
            let rows: Vec<LogRow> =
                serde_json::from_str(&line).unwrap_or_else(|e| panic!("{ctx}: {e}: {line}"));
            assert_eq!(
                serde_json::to_value(&rows).unwrap(),
                *v,
                "{ctx}: dropped fields"
            );
        }
        Value::Object(o) if o.contains_key("peers") => {
            let c = Counts::parse(&line).unwrap();
            assert_eq!(
                serde_json::to_value(&c).unwrap(),
                *v,
                "{ctx}: dropped fields"
            );
        }
        Value::Object(o) if o.contains_key("wakes") => {
            // The client's Status keeps the fields the lifecycle run uses; the app adds its own.
            let s = Status::parse(&line).unwrap();
            assert_eq!(s.id, text_of(&v["id"]), "{ctx}");
            assert_eq!(s.records, v["records"].as_u64(), "{ctx}");
        }
        other => panic!("{ctx}: unexpected ctl JSON {other}"),
    }
}

/// Whether an expectation entry is a JSON line (rather than a text line).
fn is_json_entry(e: &Value) -> bool {
    match e {
        Value::Array(_) => true,
        Value::Object(o) => !(o.len() == 1 && o.contains_key("$text")),
        _ => false,
    }
}

/// Every reply and delivered event line parses with the client's types and keeps its fields.
#[test]
fn fixture_lines_parse_without_loss() {
    let mut room_lines = 0;
    let mut ctl_lines = 0;
    for (name, fx) in fixtures() {
        let mut caps = HashMap::new();
        for (i, step) in fx["steps"].as_array().unwrap().iter().enumerate() {
            let proto = step.get("proto").and_then(Value::as_str).unwrap_or("room");
            let ctx = format!("{name} step {i}");
            for e in step
                .get("expect")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if !is_json_entry(e) {
                    continue;
                }
                let v = instantiate(e, &mut caps);
                if proto == "ctl" {
                    check_ctl_line(&v, &ctx);
                    ctl_lines += 1;
                } else {
                    check_room_line(&v, &ctx);
                    room_lines += 1;
                }
            }
            for (who, entries) in step
                .get("deliveries")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
            {
                for e in entries.as_array().unwrap() {
                    if is_json_entry(e) {
                        check_room_line(&instantiate(e, &mut caps), &format!("{ctx} to {who}"));
                        room_lines += 1;
                    }
                }
            }
        }
    }
    // A fixture set that stops covering the protocol should not pass silently.
    assert!(room_lines > 200, "{room_lines} room lines");
    assert!(ctl_lines >= 8, "{ctl_lines} ctl lines");
}

/// The client accepts the request lines the server accepts, and refuses those the server
/// refuses for size or framing, before sending them. Nicks follow the same rule, except for
/// the nicks that are directory names.
#[test]
fn client_request_checks_agree_with_the_fixtures() {
    let mut checked = 0;
    for (name, fx) in fixtures() {
        let mut caps = HashMap::new();
        for (i, step) in fx["steps"].as_array().unwrap().iter().enumerate() {
            if step.get("proto").is_some() || step.get("send").is_none() {
                continue;
            }
            let ctx = format!("{name} step {i}");
            let Some(line) = request_line(step, &mut caps) else {
                continue;
            };
            let first = instantiate(&step["expect"][0], &mut caps);
            let err_text =
                (first["t"] == "err").then(|| first["text"].as_str().unwrap().to_string());
            let refused_by_size = err_text.as_deref().is_some_and(|t| {
                t.starts_with("request too long") || t == "one request per stream"
            });
            let client = check_request(&line);
            assert_eq!(
                client.is_err(),
                refused_by_size,
                "{ctx}: {line:.60?}: {client:?}"
            );
            checked += 1;

            let (cmd, arg) = line.split_once(' ').unwrap_or((&line, ""));
            if (cmd == "/nick" || cmd == "/join") && !arg.trim().is_empty() && !refused_by_size {
                let nick = arg.trim();
                let invalid = err_text
                    .as_deref()
                    .is_some_and(|t| t.starts_with("invalid nick"));
                let accepted = check_nick(nick).is_ok();
                if CLIENT_STRICTER_NICKS.contains(&nick) {
                    assert!(!invalid && !accepted, "{ctx}");
                } else {
                    assert_eq!(accepted, !invalid, "{ctx}: nick {nick:?}");
                }
            }
        }
    }
    assert!(checked > 100, "{checked} requests checked");
}

/// ALPNs and limits that the client duplicates match `constants.json`, which the app's test
/// checks as well.
#[test]
fn constants_match() {
    let c = read_json(common_dir().join("constants.json"));
    let alpn = |k: &str| c["alpn"][k].as_str().unwrap().as_bytes().to_vec();
    assert_eq!(alpn("room"), ROOM_ALPN);
    assert_eq!(alpn("ctl"), CTL_ALPN);
    assert_eq!(alpn("echo"), ECHO_ALPN);
    assert_eq!(alpn("notify"), NOTIFY_ALPN);
    let l = &c["limits"];
    assert_eq!(l["max_request"], MAX_REQUEST);
    assert_eq!(l["history_max"], MAX_HISTORY);
    let nick_max = l["nick_max"].as_u64().unwrap() as usize;
    assert!(check_nick(&"n".repeat(nick_max)).is_ok());
    assert!(check_nick(&"n".repeat(nick_max + 1)).is_err());
}
