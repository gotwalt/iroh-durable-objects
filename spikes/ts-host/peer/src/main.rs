//! Test peer for the TypeScript host spike: one stock iroh endpoint, driven over stdin, so
//! that several connections share one relay socket to the object.
//!
//! ```text
//! iroh-do-ts-spike-peer <E> --local PORT
//!   echo TEXT   one echo round trip on a new connection, which the peer then closes
//!   idle        a connection with no streams; prints how the object closes it
//!   silent      a connection with one open stream and no data; prints how it is closed
//!   quit
//! ```

use std::time::Instant;

use iroh::{Endpoint, EndpointAddr, EndpointId, RelayUrl, endpoint::presets};
use n0_error::{Result, StdResultExt, anyerr};
use tokio::io::{AsyncBufReadExt, BufReader};

const ALPN: &[u8] = b"iroh-example/echo/0";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [id, flag, port] = args.as_slice() else {
        return Err(anyerr!("usage: iroh-do-ts-spike-peer <E> --local PORT"));
    };
    if flag != "--local" {
        return Err(anyerr!("usage: iroh-do-ts-spike-peer <E> --local PORT"));
    }
    let id = EndpointId::from_z32(id).map_err(|e| anyerr!("bad id: {e}"))?;
    let url: RelayUrl = format!("http://{}.localhost:{port}/", id.to_z32())
        .parse()
        .anyerr()?;
    let addr = EndpointAddr::new(id).with_relay_url(url);
    let endpoint = Endpoint::bind(presets::N0).await?;
    println!("ready");

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await.anyerr()? {
        let (cmd, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        let t0 = Instant::now();
        match cmd {
            "echo" => {
                let res = async {
                    let conn = endpoint.connect(addr.clone(), ALPN).await?;
                    let (mut send, mut recv) = conn.open_bi().await.anyerr()?;
                    send.write_all(rest.as_bytes()).await.anyerr()?;
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
            "idle" | "silent" => {
                let conn = endpoint.connect(addr.clone(), ALPN).await?;
                let _stream = if cmd == "silent" {
                    let (mut send, recv) = conn.open_bi().await.anyerr()?;
                    // A stream is opened at the peer only when data arrives on it.
                    send.write_all(b"x").await.anyerr()?;
                    Some((send, recv))
                } else {
                    None
                };
                let reason = conn.closed().await;
                println!("closed: {reason} after_ms={}", t0.elapsed().as_millis());
            }
            "quit" => break,
            _ => println!("unknown command"),
        }
    }
    endpoint.close().await;
    Ok(())
}
