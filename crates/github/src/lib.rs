//! GitHub.com provider backed by the authenticated GitHub CLI.
//!
//! All invocations use typed process arguments and, for mutation requests,
//! JSON on standard input. This crate deliberately owns no credential material:
//! `gh` performs authentication and the desktop only receives diagnostics.

mod client;
mod pull_request;
mod review;

pub use client::*;
pub use pull_request::*;
pub use review::*;
