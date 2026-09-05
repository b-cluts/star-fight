//! Wire protocol shared by client and server.

pub mod codec;
pub mod messages;

/// Bumped on any incompatible message change; the server cleanly rejects
/// mismatched clients with an "update required" error.
pub const PROTOCOL_VERSION: u32 = 2;
pub mod tls;
