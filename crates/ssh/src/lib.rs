//! Desktop-side SSH companion client, bootstrap planning, and managed reverse
//! forwarding. This crate has no Tauri dependency so the desktop service can
//! own its connection state and expose only typed review operations to Svelte.

mod bootstrap;
mod client;
mod reverse;

pub use bootstrap::*;
pub use client::*;
pub use reverse::*;
