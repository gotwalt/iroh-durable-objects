//! `lifecycle`: the scripted run that shows an object hibernating and waking
//! (spec §13.5, design notes 003, 005 and 006).
//!
//! 1. Session 1 on ctl/1: echo, the unprompted uni stream, `/status`.
//! 2. Stay quiet until the object closes the connection after `T_idle`
//!    (design note 005).
//! 3. Wait past `T_linger` and the platform's ~10 s hibernation delay, so the
//!    object drops its `Endpoint` (Warm) and hibernates.
//! 4. Session 2: cold-wake connect, `/status` showing `wakes` and
//!    `endpoint_builds` increased, `/callback N`, close. 0-RTT is attempted
//!    with the session-1 ticket; on stock iroh 1.4.0 the object rejects it,
//!    which is reported as an `info` row.
//! 5. The hibernated object wakes on its alarm and dials the peer on notify/1.
//! 6. Session 3: the dial-back was logged.
//!
//! Message log (design note 007): `records` must strictly increase across
//! sessions and across hibernation, and `/log` after hibernation must contain
//! the line sent before it. An object without the log fails these checks; they
//! are not skipped.
//!
//! With `--room`, the run adds the room steps (design note 008), whose names
//! start with `room`. Member `a` joins and exits; member `b` joins and stays
//! present but quiet until the object hibernates. Then `a` returns with the
//! same key and says a line, which `b` must receive as a delivery: the object
//! dials `b` back over `b`'s relay socket. Last, `b` says a line to `a`.
//!
//! The step names and the `--json` report are parsed by
//! `tests/workerd/test.sh`; change them together.

use std::{
    fmt::Write as _,
    process::ExitCode,
    time::{Duration, Instant},
};

use iroh::{Endpoint, endpoint::ConnectionError};
use n0_error::Result;
use serde::Serialize;

use crate::{
    client::{Client, Options, Origin, Sent, Update},
    ctl::{CtlSession, LogRow, Status, parse_log},
    expect_status,
    net::{CTL_ALPN, Notice, Target, ZeroRtt, connect, ms, path_summary, spawn_notify_acceptor},
    room::{Event, Input},
};

/// Arguments of `lifecycle`.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// EndpointId of the object, z-base-32.
    #[arg(value_name = "E")]
    pub endpoint: String,
    /// How long to wait for the object's idle close (its T_idle is 10 s).
    #[arg(long, default_value_t = 30, value_name = "SECS")]
    pub idle_close_timeout: u64,
    /// Wait after the close so the object goes Warm (T_linger, 2 s) and hibernates (10 s idle).
    #[arg(long, default_value_t = 15, value_name = "SECS")]
    pub hibernate_wait: u64,
    /// Dial-back delay requested with /callback, at most 50: the peer's relay socket closes 60 s after its last send.
    #[arg(long, default_value_t = 20, value_name = "SECS")]
    pub callback: u64,
    /// Extra time allowed for the dial-back after its scheduled time.
    #[arg(long, default_value_t = 30, value_name = "SECS")]
    pub notify_grace: u64,
    /// Also write the results as JSON to this file.
    #[arg(long, value_name = "PATH")]
    pub json: Option<std::path::PathBuf>,
    /// Also run the room steps: two members exchange lines after the object hibernated.
    #[arg(long)]
    pub room: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Verdict {
    Pass,
    Fail,
    Info,
    Skip,
}

impl Verdict {
    fn label(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Info => "info",
            Verdict::Skip => "skip",
        }
    }
}

#[derive(Debug, Serialize)]
struct Row {
    step: String,
    verdict: Verdict,
    /// Elapsed time since the run started, in ms.
    t_ms: f64,
    /// The step's own measured duration, in ms.
    value_ms: Option<f64>,
    /// `records` (durable log rows) observed at this step, if any.
    records: Option<u64>,
    detail: String,
}

#[derive(Debug, Serialize)]
struct Report {
    endpoint: String,
    via: String,
    local_endpoint: String,
    #[serde(skip)]
    started: Option<Instant>,
    rows: Vec<Row>,
    status_1: Option<Status>,
    status_2: Option<Status>,
    status_3: Option<Status>,
    notify_line: Option<String>,
    passed: bool,
}

impl Report {
    fn row(
        &mut self,
        step: &str,
        verdict: Verdict,
        value: Option<Duration>,
        detail: impl Into<String>,
    ) {
        self.row_r(step, verdict, value, None, detail);
    }

    /// Like [`Report::row`], also recording the `records` value seen at this step.
    fn row_r(
        &mut self,
        step: &str,
        verdict: Verdict,
        value: Option<Duration>,
        records: Option<u64>,
        detail: impl Into<String>,
    ) {
        let t_ms = self
            .started
            .map(|s| s.elapsed().as_secs_f64() * 1000.0)
            .unwrap_or_default();
        let row = Row {
            step: step.to_string(),
            verdict,
            t_ms,
            value_ms: value.map(|d| d.as_secs_f64() * 1000.0),
            records,
            detail: detail.into(),
        };
        println!(
            "[{:>8.1}s] {:<4} {}{}{}",
            t_ms / 1000.0,
            verdict.label(),
            row.step,
            value.map(|d| format!(" ({})", ms(d))).unwrap_or_default(),
            if row.detail.is_empty() {
                String::new()
            } else {
                format!(": {}", row.detail)
            }
        );
        self.rows.push(row);
    }

    fn check(
        &mut self,
        step: &str,
        ok: bool,
        value: Option<Duration>,
        detail: impl Into<String>,
    ) -> bool {
        self.row(
            step,
            if ok { Verdict::Pass } else { Verdict::Fail },
            value,
            detail,
        );
        ok
    }

    /// A check that needs `records` (design note 007); fails if the object does
    /// not report it.
    fn check_records(
        &mut self,
        step: &str,
        ok: Option<bool>,
        records: Option<u64>,
        detail: String,
    ) {
        match ok {
            Some(ok) => self.row_r(
                step,
                if ok { Verdict::Pass } else { Verdict::Fail },
                None,
                records,
                detail,
            ),
            None => self.row_r(
                step,
                Verdict::Fail,
                None,
                records,
                "the object does not report records (no message log)",
            ),
        }
    }

    fn table(&self) -> String {
        let w = self
            .rows
            .iter()
            .map(|r| r.step.len())
            .max()
            .unwrap_or(4)
            .max(4);
        let mut out = String::new();
        let _ = writeln!(out, "\nlifecycle evidence for {}", self.endpoint);
        let _ = writeln!(out, "  via   {}", self.via);
        let _ = writeln!(out, "  peer  {}\n", self.local_endpoint);
        let _ = writeln!(
            out,
            " #  {:<w$}  result  {:>11}  {:>11}  {:>7}  detail",
            "step", "t", "measured", "records"
        );
        let _ = writeln!(
            out,
            "--  {}  ------  {:>11}  {:>11}  {:>7}  ------",
            "-".repeat(w),
            "-----------",
            "-----------",
            "-------"
        );
        for (i, r) in self.rows.iter().enumerate() {
            let _ = writeln!(
                out,
                "{:>2}  {:<w$}  {:<6}  {:>9.1} s  {:>11}  {:>7}  {}",
                i + 1,
                r.step,
                r.verdict.label(),
                r.t_ms / 1000.0,
                r.value_ms.map(|v| format!("{v:.1} ms")).unwrap_or_default(),
                r.records.map(|v| v.to_string()).unwrap_or_default(),
                r.detail
            );
        }
        let failed = self
            .rows
            .iter()
            .filter(|r| r.verdict == Verdict::Fail)
            .count();
        let _ = writeln!(
            out,
            "\nresult: {}",
            if failed == 0 {
                "PASS".to_string()
            } else {
                format!("FAIL ({failed} step(s) failed)")
            }
        );
        out
    }
}

/// Runs the scripted lifecycle. Returns `ExitCode::FAILURE` if any required step failed.
pub async fn run(ep: &Endpoint, target: &Target, args: &Args) -> Result<ExitCode> {
    let mut rep = Report {
        endpoint: target.z32(),
        via: match &target.relay_url {
            Some(u) => format!("explicit relay URL {u}"),
            None => "default discovery (pkarr + DNS)".into(),
        },
        local_endpoint: ep.id().to_z32(),
        started: Some(Instant::now()),
        rows: Vec::new(),
        status_1: None,
        status_2: None,
        status_3: None,
        notify_line: None,
        passed: false,
    };
    let mut notices = spawn_notify_acceptor(ep.clone());

    let outcome = script(ep, target, args, &mut rep, &mut notices).await;
    if let Err(err) = &outcome {
        rep.row("aborted", Verdict::Fail, None, format!("{err:#}"));
    }
    if args.room
        && let Err(err) = room_steps(ep, target, args, &mut rep).await
    {
        rep.row("room aborted", Verdict::Fail, None, format!("{err:#}"));
    }
    rep.passed = !rep.rows.iter().any(|r| r.verdict == Verdict::Fail);
    print!("{}", rep.table());
    print_latency_summary(&rep);
    if let Some(path) = &args.json {
        match serde_json::to_vec_pretty(&rep) {
            Ok(bytes) => {
                if let Err(err) = std::fs::write(path, bytes) {
                    eprintln!("warning: writing {}: {err}", path.display());
                }
            }
            Err(err) => eprintln!("warning: serializing report: {err}"),
        }
    }
    Ok(if rep.passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn script(
    ep: &Endpoint,
    target: &Target,
    args: &Args,
    rep: &mut Report,
    notices: &mut tokio::sync::mpsc::UnboundedReceiver<Result<Notice>>,
) -> Result<()> {
    // ---- session 1 -------------------------------------------------------
    let c1 = connect(ep, target, CTL_ALPN).await?;
    let (kind, _) = path_summary(&c1.conn);
    rep.row(
        "s1 connect ctl/1 (first contact: possibly cold)",
        Verdict::Pass,
        Some(c1.handshake),
        format!("{kind}; 0-RTT {}", c1.zero_rtt),
    );
    // Unique per run, so `/log` after hibernation can be searched for it.
    let marker = format!(
        "lifecycle {} {}",
        &ep.id().to_z32()[..8],
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default()
    );
    let line1 = format!("{marker} s1");
    let mut s1 = CtlSession::open(c1.conn).await?;
    let (reply, rtt) = s1.request(&line1).await?;
    // Accepts both "echo: <text>" and the design note 007 form
    // "echo: <text> (#seq, total T, yours Y)".
    rep.check(
        "s1 echo (logged line)",
        reply.starts_with(&format!("echo: {line1}")),
        Some(rtt),
        reply,
    );
    let uni = s1.next_unprompted(Duration::from_secs(10)).await;
    let uni_ok = uni
        .as_deref()
        .is_some_and(|u| u.starts_with(&format!("hello from {}", target.z32())));
    rep.check(
        "s1 unprompted uni stream from DO",
        uni_ok,
        None,
        uni.unwrap_or_else(|| "none within 10 s".into()),
    );
    let (reply, rtt) = s1.request("/status").await?;
    let st1 = expect_status(&reply)?;
    let ok = st1.state == "Active" && (st1.id.is_empty() || st1.id == target.z32());
    rep.row_r(
        "s1 /status",
        if ok { Verdict::Pass } else { Verdict::Fail },
        Some(rtt),
        st1.records,
        st1.summary(),
    );
    rep.status_1 = Some(st1.clone());

    // ---- DO idle close (note 005) ---------------------------------------
    s1.finish(Duration::from_secs(2)).await;
    let quiet_since = s1.last_activity;
    let closed = tokio::time::timeout(
        Duration::from_secs(args.idle_close_timeout),
        s1.conn.closed(),
    )
    .await;
    match closed {
        Ok(ConnectionError::ApplicationClosed(close)) => {
            let reason = String::from_utf8_lossy(&close.reason).to_string();
            rep.check(
                "DO closes idle connection (T_idle)",
                reason == "idle",
                Some(quiet_since.elapsed()),
                format!(
                    "application close code {} reason {reason:?}",
                    close.error_code
                ),
            );
        }
        Ok(other) => {
            rep.check(
                "DO closes idle connection (T_idle)",
                false,
                Some(quiet_since.elapsed()),
                format!("unexpected close: {other}"),
            );
        }
        Err(_) => {
            rep.check(
                "DO closes idle connection (T_idle)",
                false,
                None,
                format!(
                    "still open after {} s; closing it ourselves",
                    args.idle_close_timeout
                ),
            );
            s1.conn.close(0u32.into(), b"lifecycle: idle");
        }
    }
    drop(s1);

    // ---- Warm + hibernation ----------------------------------------------
    let wait = Duration::from_secs(args.hibernate_wait);
    tokio::time::sleep(wait).await;
    rep.row(
        "wait for Warm (T_linger) + hibernation",
        Verdict::Info,
        Some(wait),
        "Endpoint dropped, isolate idle",
    );

    // ---- session 2: cold wake --------------------------------------------
    let c2 = connect(ep, target, CTL_ALPN).await?;
    let (kind, _) = path_summary(&c2.conn);
    rep.row(
        "s2 connect ctl/1 (cold wake)",
        Verdict::Pass,
        Some(c2.handshake),
        kind,
    );
    rep.row(
        "s2 0-RTT after Warm",
        Verdict::Info,
        None,
        match c2.zero_rtt {
            ZeroRtt::Rejected => {
                "rejected: expected on stock iroh 1.4.0 (the TLS session store does not survive an Endpoint rebuild; see upstream/)".to_string()
            }
            ZeroRtt::Accepted => "accepted: persistent session store active".to_string(),
            ZeroRtt::NoTicket => "not attempted: no ticket from session 1".to_string(),
        },
    );
    let mut s2 = CtlSession::open(c2.conn).await?;
    let line2 = format!("{marker} s2");
    let (reply, rtt) = s2.request(&line2).await?;
    rep.check(
        "s2 echo (logged line)",
        reply.starts_with(&format!("echo: {line2}")),
        Some(rtt),
        reply,
    );
    let (reply, rtt) = s2.request("/status").await?;
    let st2 = expect_status(&reply)?;
    rep.row_r(
        "s2 /status",
        Verdict::Info,
        Some(rtt),
        st2.records,
        st2.summary(),
    );
    rep.check(
        "s2 wakes increased (isolate was evicted/hibernated)",
        st2.wakes > st1.wakes,
        None,
        format!("wakes {} -> {}", st1.wakes, st2.wakes),
    );
    rep.check(
        "s2 endpoint_builds increased (Active -> Warm -> Active)",
        st2.endpoint_builds > st1.endpoint_builds,
        None,
        format!(
            "endpoint_builds {} -> {}",
            st1.endpoint_builds, st2.endpoint_builds
        ),
    );
    rep.check_records(
        "s2 records increased across hibernation (wakes increased)",
        st1.records
            .zip(st2.records)
            .map(|(a, b)| b > a && st2.wakes > st1.wakes),
        st2.records,
        format!(
            "records {} -> {}, wakes {} -> {}",
            opt(st1.records),
            opt(st2.records),
            st1.wakes,
            st2.wakes
        ),
    );
    let me = ep.id().to_z32();
    match s2.request("/log 50").await.map(|(r, _)| parse_log(&r)) {
        Ok(Ok(rows)) => {
            let found = rows.iter().find(|r| r.text == line1 && r.peer == me);
            rep.check(
                "s2 /log after hibernation contains the s1 line",
                found.is_some(),
                None,
                match found {
                    Some(r) => format!("#{} {} {:?}", r.seq, r.kind, r.text),
                    None => format!("{:?} not among the newest {} rows", line1, rows.len()),
                },
            );
        }
        _ => rep.row(
            "s2 /log after hibernation contains the s1 line",
            Verdict::Fail,
            None,
            "no JSON /log reply (the object has no message log)",
        ),
    }
    rep.row(
        "s2 DO-side time since wake",
        Verdict::Info,
        Some(Duration::from_millis(st2.since_wake_ms)),
        "reported by the DO in /status",
    );
    let uni = s2.next_unprompted(Duration::from_secs(10)).await;
    rep.check(
        "s2 unprompted uni stream from DO",
        uni.is_some(),
        None,
        uni.unwrap_or_else(|| "none within 10 s".into()),
    );
    rep.status_2 = Some(st2.clone());

    let (reply, rtt) = s2.request(&format!("/callback {}", args.callback)).await?;
    let callback_ok = reply.starts_with("ok");
    let due = Instant::now() + Duration::from_secs(args.callback);
    rep.check(
        &format!("s2 /callback {}", args.callback),
        callback_ok,
        Some(rtt),
        reply,
    );
    s2.finish(Duration::from_secs(2)).await;
    s2.conn.close(0u32.into(), b"lifecycle: wait for callback");
    drop(s2);
    rep.row(
        "s2 closed by peer; idle until dial-back",
        Verdict::Info,
        None,
        "",
    );
    if !callback_ok {
        rep.row(
            "notify/1 dial-back",
            Verdict::Skip,
            None,
            "callback refused",
        );
        return Ok(());
    }

    // ---- dial-back from the hibernated DO --------------------------------
    let deadline = due + Duration::from_secs(args.notify_grace);
    match tokio::time::timeout_at(deadline.into(), notices.recv()).await {
        Ok(Some(Ok(n))) => {
            let from_ok = n.from == target.id;
            let lag = n.at.saturating_duration_since(due);
            rep.check(
                "notify/1 dial-back from DO received",
                from_ok
                    && n.line
                        .starts_with(&format!("callback from {}", target.z32())),
                Some(lag),
                format!("{} (measured from the scheduled time)", n.line),
            );
            rep.row(
                "notify/1 incoming handshake",
                Verdict::Info,
                Some(n.handshake),
                "object -> peer over the peer's own relay socket to the object",
            );
            let notify_records = parse_notify_field(&n.line, "records=");
            rep.check_records(
                "notify/1 records >= s2 records",
                notify_records.zip(st2.records).map(|(n, s)| n >= s),
                notify_records,
                format!(
                    "records {} -> {} (the callback row is written after delivery)",
                    opt(st2.records),
                    opt(notify_records)
                ),
            );
            let woke = parse_notify_field(&n.line, "wakes=");
            rep.row(
                "DO hibernated before dial-back",
                Verdict::Info,
                None,
                match woke {
                    Some(w) if w > st2.wakes => format!("yes: wakes {} -> {w}", st2.wakes),
                    Some(w) => format!("no: wakes {} -> {w} (stayed resident)", st2.wakes),
                    None => "unknown (no wakes= in line)".into(),
                },
            );
            rep.notify_line = Some(n.line);
            session_3(ep, target, rep, &st2).await?;
        }
        Ok(Some(Err(err))) => {
            rep.check(
                "notify/1 dial-back from DO received",
                false,
                None,
                format!("{err:#}"),
            );
        }
        Ok(None) | Err(_) => {
            rep.check(
                "notify/1 dial-back from DO received",
                false,
                None,
                format!(
                    "nothing within {} s of the scheduled time",
                    args.notify_grace
                ),
            );
        }
    }
    Ok(())
}

/// Session 3, after the dial-back: the callback was logged durably.
async fn session_3(ep: &Endpoint, target: &Target, rep: &mut Report, st2: &Status) -> Result<()> {
    // The DO logs the callback after we close the notify connection.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let c3 = connect(ep, target, CTL_ALPN).await?;
    rep.row(
        "s3 connect ctl/1",
        Verdict::Pass,
        Some(c3.handshake),
        format!("0-RTT {}", c3.zero_rtt),
    );
    let mut s3 = CtlSession::open(c3.conn).await?;
    let (reply, rtt) = s3.request("/status").await?;
    let st3 = expect_status(&reply)?;
    rep.row_r(
        "s3 /status",
        Verdict::Info,
        Some(rtt),
        st3.records,
        st3.summary(),
    );
    rep.check_records(
        "s3 records increased (callback logged)",
        st2.records.zip(st3.records).map(|(a, b)| b > a),
        st3.records,
        format!("records {} -> {}", opt(st2.records), opt(st3.records)),
    );
    let me = ep.id().to_z32();
    match s3.request("/log 10").await.map(|(r, _)| parse_log(&r)) {
        Ok(Ok(rows)) => {
            let row: Option<&LogRow> = rows
                .iter()
                .rev()
                .find(|r| r.kind == "callback" && r.peer == me);
            rep.check(
                "s3 /log has the callback row",
                row.is_some_and(|r| r.text == "delivered"),
                None,
                row.map(|r| format!("#{} {:?}", r.seq, r.text))
                    .unwrap_or_else(|| "no callback row for this peer".into()),
            );
        }
        _ => rep.row(
            "s3 /log has the callback row",
            Verdict::Fail,
            None,
            "no JSON /log reply (the object has no message log)",
        ),
    }
    rep.status_3 = Some(st3);
    s3.finish(Duration::from_secs(2)).await;
    s3.conn.close(0u32.into(), b"lifecycle: done");
    Ok(())
}

fn opt(v: Option<u64>) -> String {
    v.map(|v| v.to_string()).unwrap_or_else(|| "?".into())
}

fn parse_notify_field(line: &str, key: &str) -> Option<u64> {
    line.split_whitespace()
        .find_map(|w| w.strip_prefix(key))
        .and_then(|w| w.parse().ok())
}

fn print_latency_summary(rep: &Report) {
    let get = |prefix: &str| {
        rep.rows
            .iter()
            .find(|r| r.step.starts_with(prefix))
            .and_then(|r| r.value_ms)
            .map(|v| format!("{v:.1} ms"))
            .unwrap_or_else(|| "n/a".into())
    };
    println!("cold-wake latencies (peer-observed):");
    println!(
        "  session 1 connect (relay dial + DO wake + Endpoint build + QUIC handshake): {}",
        get("s1 connect")
    );
    println!(
        "  session 2 connect after hibernation (same breakdown):                    {}",
        get("s2 connect")
    );
    println!(
        "  dial-back lag after the scheduled alarm (alarm wake + Endpoint build + dial): {}",
        get("notify/1 dial-back")
    );
}

/// Wakes counter from `/status`, read by a short-lived endpoint whose relay
/// socket closes with it, so it does not keep the object busy afterwards.
async fn wakes(target: &Target) -> Result<u64> {
    let ep = crate::net::bind_endpoint().await?;
    let res = wakes_on(&ep, target).await;
    ep.close().await;
    res
}

async fn wakes_on(ep: &Endpoint, target: &Target) -> Result<u64> {
    let c = connect(ep, target, CTL_ALPN).await?;
    let mut s = CtlSession::open(c.conn).await?;
    let (reply, _) = s.request("/status").await?;
    s.finish(Duration::from_secs(2)).await;
    s.conn.close(0u32.into(), b"lifecycle: status");
    Ok(expect_status(&reply)?.wakes)
}

/// The room steps: delivery by dial-back after hibernation, in both directions.
async fn room_steps(ep: &Endpoint, target: &Target, args: &Args, rep: &mut Report) -> Result<()> {
    let base = std::env::temp_dir().join(format!("iroh-do-demo-lifecycle-{}", std::process::id()));
    let res = room_exchange(ep, target, args, rep, &base).await;
    if res.is_err() {
        // A step failed before the members left. Rejoin with each saved key and
        // leave, so test members do not stay on the room's roster.
        let tag = &ep.id().to_z32()[..6];
        for name in ["a", "b"] {
            if base.join(name).exists() {
                let _ = room_leave(target, &room_opts(tag, name, &base)).await;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&base);
    res
}

/// Options for test member `name`: a fixed nick and a state dir under `base`.
fn room_opts(tag: &str, name: &str, base: &std::path::Path) -> Options {
    Options {
        nick: Some(format!("lc-{tag}-{name}")),
        state_dir: Some(base.join(name)),
    }
}

/// Joins as the member whose state is in `opts` and leaves with `/quit`.
async fn room_leave(target: &Target, opts: &Options) -> Result<()> {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut c = Client::start(target, opts, tx).await?;
    let res = c.send(Input::Quit(Some("lifecycle done".into()))).await;
    c.endpoint().close().await;
    res.map(|_| ())
}

async fn room_exchange(
    ep: &Endpoint,
    target: &Target,
    args: &Args,
    rep: &mut Report,
    base: &std::path::Path,
) -> Result<()> {
    let tag = &ep.id().to_z32()[..6];
    let start = |name: &'static str| {
        let opts = room_opts(tag, name, base);
        async move {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let t0 = Instant::now();
            let c = Client::start(target, &opts, tx).await?;
            Ok::<_, n0_error::AnyError>((c, rx, t0.elapsed()))
        }
    };
    let (a, _, took) = start("a").await?;
    rep.row(
        "room member a joins, then exits (stays a member)",
        Verdict::Pass,
        Some(took),
        a.endpoint().id().to_z32(),
    );
    a.endpoint().close().await;
    drop(a);
    let (mut b, mut b_rx, took) = start("b").await?;
    rep.row(
        "room member b joins and stays present",
        Verdict::Pass,
        Some(took),
        format!("{} (home relay: the room)", b.endpoint().id().to_z32()),
    );
    let before = wakes(target).await?;
    // Only b is present. Its relay pings arrive every 15 s, which leaves the
    // object time to hibernate between them; with several present members the
    // pings interleave and it may stay resident.
    let wait = Duration::from_secs(args.idle_close_timeout.min(15) + args.hibernate_wait);
    tokio::time::sleep(wait).await;
    rep.row(
        "room wait for hibernation, b quiet",
        Verdict::Info,
        Some(wait),
        format!("wakes before: {before}"),
    );
    let (mut a, mut a_rx, took) = start("a").await?;
    rep.row(
        "room member a returns (same key)",
        Verdict::Pass,
        Some(took),
        "",
    );
    let after = wakes(target).await?;
    rep.check(
        "room object hibernated while b was present",
        after > before,
        None,
        format!("wakes {before} -> {after}"),
    );
    let ok_ab = room_say(&mut a, &mut b, &mut b_rx, &format!("room check {tag} a->b")).await;
    rep.check(
        "room a says, b gets it delivered (dial-back)",
        ok_ab.is_ok(),
        ok_ab.as_ref().ok().copied(),
        ok_ab
            .as_ref()
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default(),
    );
    let ok_ba = room_say(&mut b, &mut a, &mut a_rx, &format!("room check {tag} b->a")).await;
    rep.check(
        "room b says, a gets it delivered",
        ok_ba.is_ok(),
        ok_ba.as_ref().ok().copied(),
        ok_ba
            .as_ref()
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default(),
    );
    // Leave, so test members do not stay on the room's roster.
    let mut left = true;
    for mut c in [a, b] {
        left &= matches!(
            c.send(Input::Quit(Some("lifecycle done".into()))).await,
            Ok(Sent::Left)
        );
        c.endpoint().close().await;
    }
    rep.check("room members a and b leave (/quit)", left, None, "");
    Ok(())
}

/// `from` says `text`; waits until `to` receives it as a live delivery.
/// Returns the time from sending to delivery.
async fn room_say(
    from: &mut Client,
    to: &mut Client,
    to_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Update>,
    text: &str,
) -> Result<Duration> {
    let t0 = Instant::now();
    if let Sent::Refused(err) = from.send(Input::Say(text.to_string())).await? {
        n0_error::bail_any!("the room refused /say: {err}");
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        while let Ok(u) = to_rx.try_recv() {
            if let Update::Event {
                ev: Event::Msg { text: t, .. },
                origin: Origin::Live,
                ..
            } = u
                && t == text
            {
                return Ok(t0.elapsed());
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        n0_error::ensure_any!(!left.is_zero(), "no delivery within 20 s");
        to.next_delivery(left).await;
    }
}
