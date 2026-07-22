//! The narrow transport contract shared by the desktop process, its forwarding
//! CLI, and the SSH companion.  This crate deliberately contains no facility
//! for arbitrary command execution or filesystem access.

mod agent;
mod auth;
mod frame;
mod local;
mod paths;
mod reverse;

pub use agent::*;
pub use auth::*;
pub use frame::*;
pub use local::*;
pub use paths::*;
pub use reverse::*;

/// The only currently supported wire version.  Version 3 intentionally moved
/// SSH comparison payloads from eager patch/source transfer to a manifest plus
/// immutable, capture-addressed source windows.  New incompatible messages
/// must increment this instead of relying on best-effort decoding.
/// Version 4 transports exact source bytes (including CRLF/final-newline
/// state) rather than reconstructed logical lines. It deliberately does not
/// negotiate v3 because a v3 decoder cannot validate the byte-exact window.
pub const PROTOCOL_VERSION: u16 = 4;

/// A deliberately conservative ceiling for any one local or remote message.
/// This bounds the framed (possibly compressed) payload before allocation.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Bounds decompression independently from the compressed envelope so a peer
/// cannot turn a tiny compressed frame into an unbounded allocation.
pub const MAX_UNCOMPRESSED_FRAME_BYTES: usize = 32 * 1024 * 1024;
/// Payloads below this size are clearer and cheaper to leave as plain CBOR.
pub const FRAME_COMPRESSION_THRESHOLD_BYTES: usize = 64 * 1024;
