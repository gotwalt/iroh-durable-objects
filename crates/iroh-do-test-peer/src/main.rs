//! Test peer for the TypeScript host's Miniflare tests: one stock iroh endpoint, driven over
//! stdin, so that several connections share one relay socket to the object.
//!
//! ```text
//! iroh-do-test-peer <E> --local PORT     # prints `ready <its own EndpointId, z-base-32>`
//!   echo TEXT [ALPN]   one echo round trip on a new connection, which the peer then closes
//!   send TEXT ALPN     a new connection: sends TEXT on a stream, finishes it, reads nothing,
//!                      and prints how the object closes the connection
//!   idle [ALPN]        a connection with no streams; prints how the object closes it
//!   silent [ALPN]      a connection with one open stream and no data; prints how it is closed
//!   reset ALPN         a connection with one stream: sends a byte, resets the stream with code
//!                      7, and prints how the object closes the connection
//!   stop ALPN          a connection with one stream: sends a byte and finishes, stops the
//!                      receive side with code 9, and prints how the object closes the connection
//!   quit
//! ```
//!
//! Every command prints exactly one line, `<command> failed: <error>` if it failed; the peer
//! keeps running.
//!
//! The endpoint also accepts connections the object dials back with ALPN
//! `iroh-do-test/back/0`. For each it reads a stream to its end, answers `ok:<data>`, waits
//! for the connection to close and prints `back: closed: <reason> after_ms=<ms>`. Lines from
//! dial-backs can interleave with command output.
//!
//! The ALPN defaults to `iroh-example/echo/0`. Stock iroh with default features, as
//! `examples/echo/client`; no iroh-do crate. The endpoint uses `presets::Minimal` with the
//! object as its only relay: a local test reaches no n0 relay, DNS server or pkarr relay, and
//! the peer holds its relay socket to the object from `ready` on, as a chat member does.

use std::time::{Duration, Instant};

use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl,
    endpoint::{Connection, presets},
};
use n0_error::{Result, StdResultExt, anyerr};
use tokio::io::{AsyncBufReadExt, BufReader};

const ECHO: &str = "iroh-example/echo/0";
const BACK: &[u8] = b"iroh-do-test/back/0";
const USAGE: &str = "usage: iroh-do-test-peer <E> --local PORT";
/// How long `ready` waits for the relay socket to the object.
const ONLINE_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [id, flag, port] = args.as_slice() else {
        return Err(anyerr!("{USAGE}"));
    };
    if flag != "--local" {
        return Err(anyerr!("{USAGE}"));
    }
    let id = EndpointId::from_z32(id).map_err(|e| anyerr!("bad id: {e}"))?;
    let url: RelayUrl = format!("http://{}.localhost:{port}/", id.to_z32())
        .parse()
        .anyerr()?;
    let addr = EndpointAddr::new(id).with_relay_url(url.clone());
    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::custom([url]))
        .alpns(vec![BACK.to_vec()])
        .bind()
        .await?;
    tokio::time::timeout(ONLINE_TIMEOUT, endpoint.online())
        .await
        .map_err(|_| anyerr!("no relay socket to the object after {ONLINE_TIMEOUT:?}"))?;
    tokio::spawn(accept_loop(endpoint.clone()));
    println!("ready {}", endpoint.id().to_z32());

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await.anyerr()? {
        let words: Vec<&str> = line.split(' ').collect();
        let t0 = Instant::now();

        match words.as_slice() {
            ["echo", text, rest @ ..] => {
                let alpn = rest.first().copied().unwrap_or(ECHO);
                let res = async {
                    let conn = endpoint.connect(addr.clone(), alpn.as_bytes()).await?;
                    let (mut send, mut recv) = conn.open_bi().await.anyerr()?;
                    send.write_all(text.as_bytes()).await.anyerr()?;
                    send.finish().anyerr()?;
                    let reply = recv.read_to_end(1 << 20).await.anyerr()?;
                    conn.close(0u32.into(), b"bye!");
                    n0_error::Ok(String::from_utf8_lossy(&reply).into_owned())
                }
                .await;
                match res {
                    Ok(r) => println!("echo: {r} ms={}", t0.elapsed().as_millis()),
                    Err(e) => println!("echo failed: {e}"),
                }
            }
            ["send", text, alpn] => {
                let res = async {
                    let conn = endpoint.connect(addr.clone(), alpn.as_bytes()).await?;
                    let (mut send, _recv) = conn.open_bi().await.anyerr()?;
                    send.write_all(text.as_bytes()).await.anyerr()?;
                    send.finish().anyerr()?;
                    n0_error::Ok(conn.closed().await)
                }
                .await;
                report("send", t0, res);
            }
            [cmd @ ("idle" | "silent"), rest @ ..] => {
                let alpn = rest.first().copied().unwrap_or(ECHO);
                let silent = *cmd == "silent";
                let res = async {
                    let conn = endpoint.connect(addr.clone(), alpn.as_bytes()).await?;
                    let _stream = if silent {
                        let (mut send, recv) = conn.open_bi().await.anyerr()?;
                        // A stream is opened at the peer only when data arrives on it.
                        send.write_all(b"x").await.anyerr()?;
                        Some((send, recv))
                    } else {
                        None
                    };
                    n0_error::Ok(conn.closed().await)
                }
                .await;
                report(cmd, t0, res);
            }
            ["reset", alpn] => {
                let res = async {
                    let conn = endpoint.connect(addr.clone(), alpn.as_bytes()).await?;
                    let (mut send, recv) = conn.open_bi().await.anyerr()?;
                    send.write_all(b"x").await.anyerr()?;
                    send.reset(7u32.into()).anyerr()?;
                    n0_error::Ok(closed_keeping(&conn, recv).await)
                }
                .await;
                report("reset", t0, res);
            }
            ["stop", alpn] => {
                let res = async {
                    let conn = endpoint.connect(addr.clone(), alpn.as_bytes()).await?;
                    let (mut send, mut recv) = conn.open_bi().await.anyerr()?;
                    send.write_all(b"x").await.anyerr()?;
                    send.finish().anyerr()?;
                    recv.stop(9u32.into()).anyerr()?;
                    n0_error::Ok(conn.closed().await)
                }
                .await;
                report("stop", t0, res);
            }
            ["quit"] => break,
            _ => println!("unknown command"),
        }
    }
    endpoint.close().await;
    Ok(())
}

/// Waits for `conn` to close while holding `keep`, so that dropping it does not end the
/// stream from this side.
async fn closed_keeping<T>(conn: &Connection, keep: T) -> iroh::endpoint::ConnectionError {
    let reason = conn.closed().await;
    drop(keep);
    reason
}

/// Prints `closed: <reason> after_ms=<ms>`, or `<cmd> failed: <error>`.
fn report(cmd: &str, t0: Instant, res: Result<iroh::endpoint::ConnectionError>) {
    match res {
        Ok(reason) => println!("closed: {reason} after_ms={}", t0.elapsed().as_millis()),
        Err(e) => println!("{cmd} failed: {e}"),
    }
}

/// Answers dial-backs from the object.
async fn accept_loop(endpoint: Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(async move {
            let t0 = Instant::now();
            let res = async {
                let conn = incoming.await.anyerr()?;
                let (mut send, mut recv) = conn.accept_bi().await.anyerr()?;
                let data = recv.read_to_end(1 << 16).await.anyerr()?;
                send.write_all(b"ok:").await.anyerr()?;
                send.write_all(&data).await.anyerr()?;
                send.finish().anyerr()?;
                n0_error::Ok(conn.closed().await)
            }
            .await;
            match res {
                Ok(reason) => println!(
                    "back: closed: {reason} after_ms={}",
                    t0.elapsed().as_millis()
                ),
                Err(e) => println!("back failed: {e}"),
            }
        });
    }
}
