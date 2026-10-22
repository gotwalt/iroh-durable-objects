//! workers-rs glue (wasm32 only).
//!
//! The Durable Object classes are declared in the deployment's crate by
//! [`worker!`](crate::worker), which delegates them to [`Object`] and [`Registry`] here.

mod endpoint;
mod links;
mod logging;
mod node;
mod pkarr_publish;
mod registry;
mod router;
mod store;
mod timers;

pub use endpoint::Object;
pub use registry::Registry;
pub use router::handle;
