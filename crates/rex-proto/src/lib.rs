//! The Rex client protocol: control and data connections over a unix socket or an ssh bridge.

pub mod transport;
pub mod ws;

/// The newest Rex control API revision this client speaks.
pub const API_REVISION: u32 = 19;
