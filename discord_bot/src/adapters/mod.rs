//! Adapters translate between the outside world and the application ports.
//! They depend on the core; the core never depends on them.

pub mod inbound;
pub mod outbound;
