//! `iroh-do-demo`: a command-line peer for an iroh endpoint hosted in a
//! Cloudflare Durable Object.
//!
//! It is an ordinary iroh 1.4.0 application: default features, `presets::N0`,
//! default address lookup. It must not need anything a stock iroh peer would
//! not have (spec P3), so whatever works here works for any iroh peer.
//!
//! It speaks the demo ALPNs the object serves (design notes 006 and 008):
//! `iroh-do/echo/1` (`ping`, `bench`), `iroh-do/ctl/1` (`ctl`, `listen`,
//! `log`, `lifecycle`), `iroh-do/room/1` (`chat`) and, as the accepting side,
//! `iroh-do/notify/1`, on which the object dials the peer back. `lifecycle` is
//! the scripted run behind `just demo` and `just local`.
//!
//! The harness scripts match this program's output: `harness/workerd/test.sh`
//! greps `ping` and `ctl` lines and parses the `lifecycle --json` report, and
//! `harness/workerd/evict-test.mjs` matches `ctl` and `listen` lines. Change
//! printed text together with those scripts.

mod client;
mod ctl;
mod lifecycle;
mod net;
mod room;
mod tui;

use std::{
    process::ExitCode,
    time::{Duration, Instant},
};

use clap::{Parser, Subcommand};
use iroh::Endpoint;
use n0_error::{Result, StdResultExt, anyerr, bail_any, ensure_any};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::{
    ctl::{Counts, CtlSession, Status},
    net::{
        CTL_ALPN, ECHO_ALPN, Target, ZeroRtt, bind_endpoint, connect, ms, parse_endpoint_id,
        path_summary, spawn_notify_acceptor,
    },
};

const AFTER_HELP: &str = "\
E is the object's EndpointId in its 52-character z-base-32 form, as printed by
`just mint` and used in the object's hostname. Hex ids are rejected.

By default the peer dials EndpointAddr::new(E) and iroh's default address lookup
(pkarr and DNS) finds E's relay URL, which the object published when it was
minted. This is the path any stock iroh peer takes. --relay-url URL skips the
lookup and dials EndpointAddr::new(E).with_relay_url(URL). --local does the same
with http://<E>.localhost:<local-port>/, for a local `wrangler dev`.

Each run binds a new endpoint with a random key. It also accepts
iroh-do/notify/1, so the object can dial it back while the process runs.

`replay` is not implemented. It would replay a captured 0-RTT flight after the
object restarts. On stock iroh 1.4.0 the server's TLS session store belongs to
the Endpoint and cannot be supplied from outside, so the object rejects 0-RTT
each time it rebuilds its Endpoint (after every restart or idle period), and
there is no accepted flight to replay. upstream/ holds an iroh patch that adds
the missing hook.";

#[derive(Parser, Debug)]
#[command(
    name = "iroh-do-demo",
    version,
    about = "Stock iroh 1.4 peer for an iroh endpoint hosted in a Cloudflare Durable Object",
    after_help = AFTER_HELP
)]
struct Cli {
    /// Dial E at this relay URL instead of looking it up.
    #[arg(long, global = true, value_name = "URL", conflicts_with = "local")]
    relay_url: Option<String>,
    /// Dial E at `http://<E>.localhost:<local-port>/` (local wrangler dev).
    #[arg(long, global = true)]
    local: bool,
    /// Port of the local wrangler dev, used with --local.
    #[arg(long, global = true, default_value_t = 8787, value_name = "PORT")]
    local_port: u16,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Echo a payload over echo/1; print connect time, echo RTT, path and 0-RTT outcome.
    Ping {
        /// EndpointId of the object, z-base-32.
        #[arg(value_name = "E")]
        endpoint: String,
        /// Number of sequential connections; the second and later can attempt 0-RTT.
        #[arg(long, short = 'c', default_value_t = 1)]
        count: u32,
        /// Payload size in bytes.
        #[arg(long, default_value_t = 32)]
        size: usize,
    },
    /// Chat room client (design note 008).
    ///
    /// On a terminal this is a full-screen client. With --say or --listen-for
    /// it runs without a UI and prints JSON lines for scripts. The room's
    /// relay URL is the client's only relay, so the client stays reachable by
    /// the room while it runs.
    Chat(ChatArgs),
    /// Interactive ctl/1 session: send stdin lines, print the object's replies.
    ///
    /// Each line is sent to the object and its one-line reply is printed.
    /// Commands: `/status`, `/count`, `/log [n]` (printed as a table),
    /// `/callback N` (the object dials back on notify/1 after N seconds,
    /// N <= 50). The
    /// object's unprompted uni-stream line and any dial-backs are printed too.
    /// /quit or end of input closes the session.
    Ctl {
        /// EndpointId of the object, z-base-32.
        #[arg(value_name = "E")]
        endpoint: String,
    },
    /// Say hello over ctl/1, close the connection, then print notify/1 dial-backs.
    ///
    /// The local endpoint stays up after the ctl connection closes. Its relay
    /// socket to the object stays open for up to 60 s after its last send, and
    /// the object can dial back only over that socket.
    Listen {
        /// EndpointId of the object, z-base-32.
        #[arg(value_name = "E")]
        endpoint: String,
        /// Ask the object to dial back after this many seconds (at most 50).
        #[arg(long, value_name = "SECS")]
        callback: Option<u32>,
        /// Exit after the first notify/1.
        #[arg(long)]
        once: bool,
        /// Give up after this many seconds (default: run until Ctrl-C).
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
    },
    /// Echo N MiB over one echo/1 stream; print throughput.
    Bench {
        /// EndpointId of the object, z-base-32.
        #[arg(value_name = "E")]
        endpoint: String,
        /// MiB to send (the same amount comes back).
        #[arg(long, default_value_t = 16)]
        mb: u32,
    },
    /// Print the object's message log totals (/count) and newest rows (/log N).
    Log {
        /// EndpointId of the object, z-base-32.
        #[arg(value_name = "E")]
        endpoint: String,
        /// Number of newest rows; the object returns at most 50.
        #[arg(short = 'n', default_value_t = 10)]
        n: u32,
    },
    /// Scripted lifecycle run; print a pass/fail table with timings.
    ///
    /// Steps: a ctl session, the object's idle close, hibernation, a cold-wake
    /// session, a dial-back from the hibernated object, and a check that the
    /// message log survived. Exits non-zero if any step fails.
    Lifecycle(lifecycle::Args),
    /// Not implemented: 0-RTT does not survive an object restart on stock iroh 1.4.0.
    Replay {
        /// EndpointId of the object, z-base-32.
        #[arg(value_name = "E")]
        endpoint: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let endpoint_arg = match &cli.cmd {
        Cmd::Ping { endpoint, .. }
        | Cmd::Ctl { endpoint }
        | Cmd::Listen { endpoint, .. }
        | Cmd::Bench { endpoint, .. }
        | Cmd::Log { endpoint, .. }
        | Cmd::Replay { endpoint } => endpoint.clone(),
        Cmd::Lifecycle(args) => args.endpoint.clone(),
        Cmd::Chat(args) => args.endpoint.clone(),
    };
    if let Cmd::Replay { .. } = cli.cmd {
        bail_any!(
            "`replay` is not implemented: on stock iroh 1.4.0 the object's TLS session store \
             is lost whenever it rebuilds its Endpoint, so 0-RTT is rejected after a restart \
             and there is no flight to replay"
        );
    }
    let id = parse_endpoint_id(&endpoint_arg)?;
    let target = Target::new(id, cli.relay_url.as_deref(), cli.local, cli.local_port)?;
    if let Cmd::Chat(args) = cli.cmd {
        return chat(&target, args).await;
    }
    let ep = bind_endpoint().await?;
    eprintln!("local endpoint: {}", ep.id().to_z32());
    eprintln!("target:         {}", target.describe());

    let res = match cli.cmd {
        Cmd::Ping { count, size, .. } => ping(&ep, &target, count, size)
            .await
            .map(|_| ExitCode::SUCCESS),
        Cmd::Ctl { .. } => ctl_interactive(&ep, &target)
            .await
            .map(|_| ExitCode::SUCCESS),
        Cmd::Listen {
            callback,
            once,
            timeout,
            ..
        } => listen(&ep, &target, callback, once, timeout).await,
        Cmd::Bench { mb, .. } => bench(&ep, &target, mb).await.map(|_| ExitCode::SUCCESS),
        Cmd::Log { n, .. } => show_log(&ep, &target, n).await.map(|_| ExitCode::SUCCESS),
        Cmd::Lifecycle(args) => lifecycle::run(&ep, &target, &args).await,
        Cmd::Replay { .. } | Cmd::Chat(_) => Ok(ExitCode::FAILURE),
    };
    ep.close().await;
    res
}

// ---------------------------------------------------------------------------
// ping

async fn ping(ep: &Endpoint, target: &Target, count: u32, size: usize) -> Result<()> {
    let payload: Vec<u8> = (0..size).map(|i| b'a' + (i % 26) as u8).collect();
    for round in 1..=count.max(1) {
        let t0 = Instant::now();
        let connecting = ep
            .connect_with_opts(target.addr(), ECHO_ALPN, Default::default())
            .await?;
        // Send the payload as 0-RTT data when we hold a ticket; resend it after a rejection.
        let (conn, zero_rtt, handshake, echo_rtt) = match connecting.into_0rtt() {
            Ok(early) => {
                let (send, recv) = early.open_bi().await.anyerr()?;
                let p = payload.clone();
                let writer = tokio::spawn(async move { write_finish(send, &p).await });
                match early.handshake_completed().await? {
                    iroh::endpoint::ZeroRttStatus::Accepted(conn) => {
                        writer.await.anyerr()??;
                        let handshake = t0.elapsed();
                        let back = read_all(recv, size).await?;
                        ensure_any!(back == payload, "echo mismatch");
                        (conn, ZeroRtt::Accepted, handshake, t0.elapsed())
                    }
                    iroh::endpoint::ZeroRttStatus::Rejected(conn) => {
                        writer.abort();
                        let handshake = t0.elapsed();
                        let rtt = echo_on(&conn, &payload).await?;
                        (conn, ZeroRtt::Rejected, handshake, rtt)
                    }
                }
            }
            Err(connecting) => {
                let conn = connecting.await.anyerr()?;
                let handshake = t0.elapsed();
                let rtt = echo_on(&conn, &payload).await?;
                (conn, ZeroRtt::NoTicket, handshake, rtt)
            }
        };
        let (kind, path_rtt) = path_summary(&conn);
        println!(
            "ping {round}/{count}: echo {size} B ok | connect {} | echo RTT {}{} | path RTT {} | {kind} | 0-RTT: {zero_rtt}",
            ms(handshake),
            ms(echo_rtt),
            if zero_rtt == ZeroRtt::Accepted {
                " (from connect start; data rode in 0-RTT)"
            } else {
                ""
            },
            path_rtt.map(ms).unwrap_or_else(|| "?".into()),
        );
        conn.close(0u32.into(), b"done");
    }
    Ok(())
}

async fn write_finish(mut send: iroh::endpoint::SendStream, data: &[u8]) -> Result<()> {
    send.write_all(data).await.anyerr()?;
    send.finish().anyerr()?;
    Ok(())
}

async fn read_all(mut recv: iroh::endpoint::RecvStream, expect: usize) -> Result<Vec<u8>> {
    recv.read_to_end(expect.max(1) * 2 + 1024).await.anyerr()
}

/// One echo exchange on an established connection; returns the round-trip time.
async fn echo_on(conn: &iroh::endpoint::Connection, payload: &[u8]) -> Result<Duration> {
    let t0 = Instant::now();
    let (send, recv) = conn.open_bi().await.anyerr()?;
    write_finish(send, payload).await?;
    let back = read_all(recv, payload.len()).await?;
    ensure_any!(back == payload, "echo mismatch");
    Ok(t0.elapsed())
}

// ---------------------------------------------------------------------------
// chat

/// Arguments of `chat`.
#[derive(clap::Args, Debug)]
#[command(after_help = CHAT_HELP)]
struct ChatArgs {
    /// EndpointId of the room's object, z-base-32.
    #[arg(value_name = "E")]
    endpoint: String,
    /// Member to run as, and its nick. Omitted: $USER, or "me".
    #[arg(long, value_name = "NAME")]
    nick: Option<String>,
    /// Keep the key and cursor under DIR/rooms/<E>/<nick> instead of the user's data directory.
    #[arg(long, value_name = "DIR")]
    state_dir: Option<std::path::PathBuf>,
    /// Without a UI: join, send TEXT, wait for the room's ok, print the message.
    #[arg(long, value_name = "TEXT")]
    say: Option<String>,
    /// Without a UI: join, then print events as JSON lines for SECS seconds.
    #[arg(long, value_name = "SECS")]
    listen_for: Option<u64>,
}

const CHAT_HELP: &str = "\
In the client, typed text is sent to the room. Commands: /me <action>,
/nick <name>, /who, /history [n], /help, /quit [reason]. //text sends text
that starts with /. /quit leaves the room. Esc, Ctrl-C or Ctrl-D exits and
stays a member: the room lists you as away until you return. PgUp, PgDn and
the mouse wheel scroll; scrolling past the top loads older events, up to 200.

The member key and the sequence number of the last event seen are stored per
room and nick, in rooms/<E>/<nick> under the user's data directory (or under
--state-dir). Each nick is a separate member with its own key, so clients with
different nicks can run at once on one machine. With the same key the room
keeps your membership. --nick also sets the nick on join, undoing a /nick from
an earlier session. Without --nick the client uses $USER (or \"me\") as the
member and keeps the nick the room has for it. On start the client sends
/join, shows the history, and asks for every event after the stored sequence
number.

--listen-for prints the /join history (with \"hist\":true), then each event
the room delivers, one JSON object per line, as the room sent it. --say prints
the stored message, built from the room's ok, as one JSON line. With both, the
client says, then listens. Neither sends /quit.

One client runs per room and nick at a time: a second one exits with an
error, because the room keeps only the newest relay socket per EndpointId.
Pick another --nick to join as a second member.";

async fn chat(target: &Target, args: ChatArgs) -> Result<ExitCode> {
    let opts = client::Options {
        nick: args.nick,
        state_dir: args.state_dir,
    };
    if args.say.is_none() && args.listen_for.is_none() {
        use std::io::IsTerminal;
        ensure_any!(
            std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            "chat needs a terminal; use --say or --listen-for without one"
        );
        return tui::run(target, opts).await;
    }
    let listen = args.listen_for.is_some();
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    // Events go to stdout as JSON; everything else to stderr.
    let printer = tokio::spawn(async move {
        while let Some(u) = updates.recv().await {
            match u {
                // --say alone prints only the stored message.
                client::Update::Event { raw, origin, .. }
                    if listen || origin != client::Origin::Hist =>
                {
                    println!("{raw}")
                }
                client::Update::Error(e) => eprintln!("error: {e}"),
                client::Update::Joined { nick } => eprintln!("joined as {nick}"),
                _ => {}
            }
        }
    });
    let mut c = client::Client::start(target, &opts, tx).await?;
    eprintln!("member endpoint: {}", c.endpoint().id().to_z32());
    eprintln!("state:           {}", c.state_dir().display());
    let mut code = ExitCode::SUCCESS;
    if let Some(text) = args.say {
        match room::Input::parse(&text) {
            Ok(input @ room::Input::Say(_)) => match c.send(input).await {
                Ok(client::Sent::Refused(text)) => {
                    eprintln!("error: the room refused the line: {text}");
                    code = ExitCode::FAILURE;
                }
                Ok(_) => {}
                Err(err) => {
                    eprintln!("error: {err:#}");
                    code = ExitCode::FAILURE;
                }
            },
            _ => bail_any!("--say takes plain text; start it with // to send a leading /"),
        }
    }
    if let Some(secs) = args.listen_for {
        c.listen(Duration::from_secs(secs)).await;
    }
    let ep = c.endpoint().clone();
    drop(c);
    ep.close().await;
    let _ = printer.await;
    Ok(code)
}

// ---------------------------------------------------------------------------
// ctl

async fn ctl_interactive(ep: &Endpoint, target: &Target) -> Result<()> {
    let mut notices = spawn_notify_acceptor(ep.clone());
    let mut session = open_ctl(ep, target).await?;
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut pending_callback: Option<Instant> = None;
    eprintln!("type lines; /status, /count, /log [n], /callback N, /quit");

    loop {
        tokio::select! {
            line = stdin.next_line() => {
                let line = line.anyerr()?;
                let Some(line) = line else { break };
                let line = line.trim().to_string();
                if line.is_empty() { continue; }
                if line == "/quit" { break; }
                let reply = match session.request(&line).await {
                    Ok(r) => r,
                    Err(err) => match session.conn.close_reason() {
                        Some(reason) => {
                            println!("-- connection closed ({reason}); reconnecting");
                            session = open_ctl(ep, target).await?;
                            session.request(&line).await?
                        }
                        None => return Err(err),
                    },
                };
                print_reply(&line, &reply.0, reply.1);
                let callback_secs = line
                    .strip_prefix("/callback ")
                    .and_then(|s| s.trim().parse::<u64>().ok());
                if let Some(secs) = callback_secs
                    && reply.0.starts_with("ok")
                {
                    pending_callback = Some(Instant::now() + Duration::from_secs(secs));
                    println!("-- the DO will dial back on notify/1 in {secs}s; keep this process running");
                }
                while let Some(u) = session.try_unprompted() {
                    println!("<< (uni) {u}");
                }
            }
            u = session.next_unprompted(Duration::from_secs(3600)) => {
                if let Some(u) = u { println!("<< (uni) {u}"); }
            }
            n = notices.recv() => {
                print_notice(n, pending_callback.take());
            }
        }
    }

    session.finish(Duration::from_secs(2)).await;
    session.conn.close(0u32.into(), b"bye");
    if let Some(due) = pending_callback {
        let deadline = due + Duration::from_secs(30);
        eprintln!("-- ctl closed; waiting for the pending dial-back");
        match tokio::time::timeout_at(deadline.into(), notices.recv()).await {
            Ok(n) => print_notice(n, Some(due)),
            Err(_) => bail_any!("no notify/1 dial-back within 30 s of the scheduled time"),
        }
    }
    Ok(())
}

async fn open_ctl(ep: &Endpoint, target: &Target) -> Result<CtlSession> {
    let c = connect(ep, target, CTL_ALPN).await?;
    let (kind, rtt) = path_summary(&c.conn);
    println!(
        "-- connected in {} | {kind} | path RTT {} | 0-RTT: {}",
        ms(c.handshake),
        rtt.map(ms).unwrap_or_else(|| "?".into()),
        c.zero_rtt
    );
    CtlSession::open(c.conn).await
}

/// Prints a reply as-is; `/log` replies are pretty-printed when they parse.
fn print_reply(line: &str, reply: &str, rtt: Duration) {
    let is_log = line == "/log" || line.starts_with("/log ");
    match is_log.then(|| ctl::parse_log(reply)) {
        Some(Ok(rows)) => {
            println!("<< /log: {} row(s) ({})", rows.len(), ms(rtt));
            print!("{}", ctl::format_log(&rows));
        }
        _ => println!("<< {reply} ({})", ms(rtt)),
    }
}

// ---------------------------------------------------------------------------
// log

async fn show_log(ep: &Endpoint, target: &Target, n: u32) -> Result<()> {
    let mut session = open_ctl(ep, target).await?;
    let (count, rtt) = session.request("/count").await?;
    match Counts::parse(&count) {
        Ok(c) => println!(
            "records: total {} | yours {} | distinct peers {} ({})",
            c.total,
            c.yours,
            c.peers,
            ms(rtt)
        ),
        Err(_) => println!("<< /count: {count} (not JSON: the object may have no message log)"),
    }
    let line = format!("/log {n}");
    let (reply, rtt) = session.request(&line).await?;
    print_reply(&line, &reply, rtt);
    session.finish(Duration::from_secs(2)).await;
    session.conn.close(0u32.into(), b"bye");
    Ok(())
}

fn print_notice(n: Option<Result<net::Notice>>, due: Option<Instant>) {
    match n {
        Some(Ok(n)) => {
            let delay = due.map(|d| {
                if n.at >= d {
                    format!(" (+{} after the scheduled time)", ms(n.at - d))
                } else {
                    format!(" ({} before the scheduled time)", ms(d - n.at))
                }
            });
            println!(
                "<< notify/1 from {}: {}{} [incoming handshake {}]",
                n.from.to_z32(),
                n.line,
                delay.unwrap_or_default(),
                ms(n.handshake)
            );
        }
        Some(Err(err)) => println!("-- incoming connection failed: {err:#}"),
        None => println!("-- endpoint stopped accepting"),
    }
}

// ---------------------------------------------------------------------------
// listen

async fn listen(
    ep: &Endpoint,
    target: &Target,
    callback: Option<u32>,
    once: bool,
    timeout: Option<u64>,
) -> Result<ExitCode> {
    let mut notices = spawn_notify_acceptor(ep.clone());
    let mut session = open_ctl(ep, target).await?;
    let (reply, rtt) = session.request("hello").await?;
    println!("<< {reply} ({})", ms(rtt));
    match session.next_unprompted(Duration::from_secs(5)).await {
        Some(u) => println!("<< (uni) {u}"),
        None => println!("-- no unprompted uni stream within 5 s"),
    }
    let mut due = None;
    if let Some(secs) = callback {
        let (reply, rtt) = session.request(&format!("/callback {secs}")).await?;
        println!("<< {reply} ({})", ms(rtt));
        ensure_any!(reply.starts_with("ok"), "callback refused: {reply}");
        due = Some(Instant::now() + Duration::from_secs(secs.into()));
    }
    session.finish(Duration::from_secs(2)).await;
    session.conn.close(0u32.into(), b"bye");
    let closed_at = Instant::now();
    println!(
        "-- ctl connection closed; endpoint {} stays up and accepts notify/1",
        ep.id().to_z32()
    );
    println!(
        "-- our relay socket to the object closes 60 s after our last send; \
         a dial-back must happen inside that window"
    );

    let deadline = timeout.map(|s| closed_at + Duration::from_secs(s));
    loop {
        let sleep = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d.into()).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            n = notices.recv() => {
                let stop = n.is_none();
                let ok = matches!(n, Some(Ok(_)));
                print_notice(n, due.take());
                if stop { return Ok(ExitCode::FAILURE); }
                if once && ok { return Ok(ExitCode::SUCCESS); }
            }
            _ = tokio::signal::ctrl_c() => return Ok(ExitCode::SUCCESS),
            _ = sleep => {
                eprintln!("-- timeout");
                return Ok(if once { ExitCode::FAILURE } else { ExitCode::SUCCESS });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// bench

async fn bench(ep: &Endpoint, target: &Target, mb: u32) -> Result<()> {
    const CHUNK: usize = 64 * 1024;
    let total = mb as usize * 1024 * 1024;
    let c = connect(ep, target, ECHO_ALPN).await?;
    let (kind, rtt) = path_summary(&c.conn);
    println!(
        "connected in {} | {kind} | path RTT {} | 0-RTT: {}",
        ms(c.handshake),
        rtt.map(ms).unwrap_or_else(|| "?".into()),
        c.zero_rtt
    );
    let (mut send, mut recv) = c.conn.open_bi().await.anyerr()?;
    let t0 = Instant::now();
    let writer = tokio::spawn(async move {
        let mut buf = vec![0u8; CHUNK];
        let mut sent = 0usize;
        while sent < total {
            let n = CHUNK.min(total - sent);
            for (i, b) in buf[..n].iter_mut().enumerate() {
                *b = ((sent + i) % 251) as u8;
            }
            send.write_all(&buf[..n]).await.anyerr()?;
            sent += n;
        }
        send.finish().anyerr()?;
        Ok::<_, n0_error::AnyError>(Instant::now())
    });
    let mut buf = vec![0u8; CHUNK];
    let mut got = 0usize;
    let mut first_byte = None;
    loop {
        let Some(n) = recv.read(&mut buf).await.anyerr()? else {
            break;
        };
        if first_byte.is_none() {
            first_byte = Some(t0.elapsed());
        }
        for (i, b) in buf[..n].iter().enumerate() {
            if *b != ((got + i) % 251) as u8 {
                bail_any!("echo mismatch at byte {}", got + i);
            }
        }
        got += n;
        if got > total {
            bail_any!("DO echoed more bytes ({got}) than sent ({total})");
        }
    }
    let elapsed = t0.elapsed();
    let write_done = writer.await.anyerr()??;
    ensure_any!(
        got == total,
        "DO echoed {got} of {total} bytes (does its echo handler cap the stream size?)"
    );
    let secs = elapsed.as_secs_f64().max(1e-9);
    let mib = total as f64 / (1024.0 * 1024.0);
    println!(
        "echoed {mib:.1} MiB in {:.2} s | upload done after {:.2} s | first byte back after {} | {:.2} MiB/s each way ({:.2} MiB/s aggregate)",
        secs,
        (write_done - t0).as_secs_f64(),
        first_byte.map(ms).unwrap_or_else(|| "?".into()),
        mib / secs,
        2.0 * mib / secs
    );
    c.conn.close(0u32.into(), b"done");
    Ok(())
}

/// Parses `/status` from a ctl reply, with context.
pub(crate) fn expect_status(reply: &str) -> Result<Status> {
    Status::parse(reply).map_err(|err| anyerr!("unexpected /status reply {reply:?}: {err}"))
}
