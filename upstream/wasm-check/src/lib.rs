//! Compiles the restart-state builder hooks for `wasm32-unknown-unknown`.

use std::sync::Arc;

use iroh::endpoint::Builder;

/// A builder using both restart-state hooks (never bound here; this is a build check).
pub fn restart_state_builder(reset_key: [u8; 32]) -> Builder {
    Builder::empty()
        .crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .stateless_reset_key(reset_key)
        .tls_server_session_store(rustls::server::ServerSessionMemoryCache::new(64))
}
