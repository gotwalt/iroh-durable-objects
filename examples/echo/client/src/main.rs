//! A stock iroh peer for the echo app: the connecting side of iroh 1.4.0 `examples/echo.rs`,
//! dialing an endpoint by its EndpointId.
//!
//! ```text
//! iroh-do-echo-client <E> [TEXT]                   # production: E is found by default discovery
//! iroh-do-echo-client <E> [TEXT] --local [PORT]    # local workerd: relay http://<E>.localhost:PORT/
//! iroh-do-echo-client <E> [TEXT] --relay URL       # an explicit relay URL
//! ```
//!
//! `E` is z-base-32, as iroh-do prints it (design note 004). The endpoint uses
//! `presets::N0` with default features, like any iroh application. With `--local` or
//! `--relay` the address carries the relay URL, because nothing is published for
//! `localhost`.

use std::time::Instant;

use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayUrl,
    endpoint::{Connection, presets},
};
use n0_error::{Result, StdResultExt, anyerr};

/// The ALPN of iroh's echo example, as `iroh-do-echo` serves it.
const ALPN: &[u8] = b"iroh-example/echo/0";

const USAGE: &str = "usage: iroh-do-echo-client <E> [TEXT] [--local [PORT] | --relay URL]";

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut id = None;
    let mut text = None;
    let mut relay: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--relay" => relay = Some(args.next().ok_or_else(|| anyerr!("{USAGE}"))?),
            "--local" => {
                let port = args.next().unwrap_or_else(|| "8787".to_string());
                relay = Some(format!("local:{port}"));
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ if id.is_none() => id = Some(a),
            _ if text.is_none() => text = Some(a),
            _ => return Err(anyerr!("{USAGE}")),
        }
    }
    let id = id.ok_or_else(|| anyerr!("{USAGE}"))?;
    let id = EndpointId::from_z32(&id)
        .map_err(|e| anyerr!("{id:?} is not a z-base-32 EndpointId: {e}"))?;
    let mut addr = EndpointAddr::new(id);
    if let Some(r) = relay {
        let url = match r.strip_prefix("local:") {
            Some(port) => format!("http://{}.localhost:{port}/", id.to_z32()),
            None => r,
        };
        let url: RelayUrl = url
            .parse()
            .map_err(|e| anyerr!("invalid relay URL {url:?}: {e}"))?;
        addr = addr.with_relay_url(url);
    }
    let text = text.unwrap_or_else(|| "Hello, world!".to_string());

    let endpoint = Endpoint::bind(presets::N0).await?;
    let t0 = Instant::now();
    let conn = endpoint.connect(addr, ALPN).await?;
    let connected = t0.elapsed();
    let reply = echo(&conn, text.as_bytes()).await?;
    let total = t0.elapsed();
    conn.close(0u32.into(), b"bye!");
    endpoint.close().await;

    println!("{}", String::from_utf8_lossy(&reply));
    eprintln!(
        "connected in {} ms, echoed {} bytes in {} ms",
        connected.as_millis(),
        reply.len(),
        total.as_millis()
    );
    if reply != text.as_bytes() {
        return Err(anyerr!("reply differs from what was sent"));
    }
    Ok(())
}

/// The exchange of iroh's example: one bidirectional stream, written and finished, read to
/// the end.
async fn echo(conn: &Connection, msg: &[u8]) -> Result<Vec<u8>> {
    let (mut send, mut recv) = conn.open_bi().await.anyerr()?;
    send.write_all(msg).await.anyerr()?;
    send.finish().anyerr()?;
    recv.read_to_end(1 << 20).await.anyerr()
}
