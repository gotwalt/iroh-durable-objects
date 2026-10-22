//! Builds the node's endpoint, whose only transport is ingress.

use std::{sync::Arc, time::Duration};

use iroh::Endpoint;
use iroh::endpoint::{BindError, Builder, IdleTimeout, QuicTransportConfig, VarInt};
use iroh_ingress_transport::{IngressTransport, OnlyIngress};

use crate::Config;

/// Connection keep-alive interval long enough to mean "off".
///
/// iroh accepts only a value, not `None` (iroh 1.4.0, src/endpoint/quic.rs:366-369). It must
/// stay below 2^31 ms because n0-future's wasm `Sleep` casts milliseconds to `i32`
/// (n0-future 0.3.2, src/time.rs:76). iroh's separate 5 s per-path keep-alive cannot be
/// turned off, which is why the node closes idle connections itself.
const KEEP_ALIVE_OFF: Duration = Duration::from_secs(24 * 3600);

/// Binds an endpoint with no IP sockets, no relay and no address lookup, `transport` as its
/// only transport, accepting `alpns`.
pub(crate) async fn bind(
    config: &Config,
    transport: IngressTransport,
    alpns: Vec<Vec<u8>>,
) -> Result<Endpoint, BindError> {
    let idle_ms = u32::try_from(config.max_idle_timeout.as_millis()).unwrap_or(u32::MAX);
    let idle = IdleTimeout::from(VarInt::from_u32(idle_ms));
    let transport_config = QuicTransportConfig::builder()
        .max_idle_timeout(Some(idle))
        .keep_alive_interval(KEEP_ALIVE_OFF)
        .build();
    // `empty()` disables relays and address lookup (iroh 1.4.0, src/endpoint.rs:192-222).
    let builder = Builder::empty()
        .secret_key(config.secret_key.clone())
        .alpns(alpns)
        .crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .transport_config(transport_config)
        .add_custom_transport(Arc::new(transport))
        .path_selector(Arc::new(OnlyIngress))
        .clear_relay_transports()
        .clear_address_lookup();
    // Natively `empty()` adds IPv4 and IPv6 sockets. On wasm32 it adds none and the method
    // does not exist.
    #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
    let builder = builder.clear_ip_transports();
    builder.bind().await
}
