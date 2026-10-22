//! The echo app: iroh's own echo example, running in a Durable Object.
//!
//! [`Echo`] is copied unchanged from iroh 1.4.0 `examples/echo.rs`, apart from `pub`. It is an
//! ordinary iroh [`ProtocolHandler`], the kind iroh's `Router` accepts; [`EchoApp`] registers it
//! under the example's ALPN. Nothing else is needed to run it in iroh-do (design note 011).
//!
//! A peer dials the endpoint with [`ALPN`], opens one bidirectional stream, writes bytes and
//! finishes the stream; the endpoint writes the same bytes back and finishes. The peer then
//! closes the connection. `apps/echo-client` is such a peer.
//!
//! The `println!`s of the example print nothing on wasm32-unknown-unknown, which has no
//! standard output.

use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use iroh_do::{App, Ctx, Protocols};

/// The ALPN of iroh's echo example (iroh 1.4.0 `examples/echo.rs`).
pub const ALPN: &[u8] = b"iroh-example/echo/0";

/// The echo app.
#[derive(Debug)]
pub struct EchoApp;

impl App for EchoApp {
    const NAME: &'static str = "echo";

    fn new(_ctx: &Ctx) -> Self {
        EchoApp
    }

    fn protocols(&self, _ctx: &Ctx, p: &mut Protocols) {
        p.accept(ALPN, Echo);
    }

    fn join_hint(id: &str) -> String {
        format!("iroh-do-echo-client {id}")
    }
}

// From here on, iroh 1.4.0 `examples/echo.rs`.

#[derive(Debug, Clone)]
pub struct Echo;

impl ProtocolHandler for Echo {
    /// The `accept` method is called for each incoming connection for our ALPN.
    ///
    /// The returned future runs on a newly spawned tokio task, so it can run as long as
    /// the connection lasts.
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // We can get the remote's endpoint id from the connection.
        let endpoint_id = connection.remote_id();
        println!("accepted connection from {endpoint_id}");

        // Our protocol is a simple request-response protocol, so we expect the
        // connecting peer to open a single bi-directional stream.
        let (mut send, mut recv) = connection.accept_bi().await?;

        // Echo any bytes received back directly.
        // This will keep copying until the sender signals the end of data on the stream.
        let bytes_sent = tokio::io::copy(&mut recv, &mut send).await?;
        println!("Copied over {bytes_sent} byte(s)");

        // By calling `finish` on the send stream we signal that we will not send anything
        // further, which makes the receive stream on the other end terminate.
        send.finish()?;

        // Wait until the remote closes the connection, which it does once it
        // received the response.
        connection.closed().await;

        Ok(())
    }
}
