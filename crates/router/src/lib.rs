//! `router`: MCP proxy routing for `lds` sessions.
//!
//! ## Architecture
//!
//! This crate is deliberately split out from the main `lds` binary crate so
//! that the `rmcp` client-side feature set (`client`, `transport-child-process`)
//! stays contained to routing code rather than bleeding into the whole
//! workspace's build graph. It owns three responsibilities:
//!
//! 1. **Configuration** ([`RouteConfig`]) — parsing and merging
//!    `routes.toml` files (user-global overridden by project-local).
//! 2. **Subprocess lifecycle** ([`RouteClient`]) — lazily spawning one
//!    upstream MCP server per route and proxying `call_tool` requests to it
//!    over stdio, enforcing a per-route timeout.
//! 3. **Registry** ([`McpRouter`]) — a cheaply cloneable, concurrency-safe
//!    map from route name to [`RouteClient`], plus `<route>://<tool>` URI
//!    dispatch with an early reject for the reserved `lds://` self-loop
//!    scheme.
//!
//! Re-exposure of upstream tools under this crate's own MCP surface
//! (`[[export]]` declarations) is a separate concern added by a later
//! subtask; this crate currently exposes only the routing primitives above.
#![warn(missing_docs)]

mod client;
mod config;
mod error;
mod router;

pub use client::RouteClient;
pub use config::RouteConfig;
pub use error::RouterError;
pub use router::McpRouter;
