//! `router`: MCP proxy routing for `lds` sessions.
//!
//! ## Architecture
//!
//! This crate is deliberately split out from the main `lds` binary crate so
//! that the `rmcp` client-side feature set (`client`, `transport-child-process`)
//! stays contained to routing code rather than bleeding into the whole
//! workspace's build graph. It owns four responsibilities:
//!
//! 1. **Configuration** ([`RouteConfig`], [`ExportConfig`]) — parsing and
//!    merging `routes.toml` files (user-global overridden by project-local).
//! 2. **Subprocess lifecycle** ([`RouteClient`]) — lazily spawning one
//!    upstream MCP server per route and proxying `call_tool`/`list_tools`
//!    requests to it over stdio, enforcing a per-route timeout on calls.
//! 3. **Registry** ([`McpRouter`]) — a cheaply cloneable, concurrency-safe
//!    map from route name to [`RouteClient`], plus `<route>://<tool>` URI
//!    dispatch with an early reject for the reserved `lds://` self-loop
//!    scheme.
//! 4. **Export registry** ([`ExportRegistry`]) — re-exposing a declared
//!    subset of a route's upstream tools directly on this session's own
//!    tool surface, under a `<prefix><tool>` public name, so callers can
//!    invoke them without knowing they are proxied.
#![warn(missing_docs)]

mod client;
mod config;
mod error;
mod export;
mod router;

pub use client::RouteClient;
pub use config::{ExportConfig, RouteConfig};
pub use error::RouterError;
pub use export::{DEFAULT_EXPORT_LIMIT, ExportRegistry};
pub use router::McpRouter;
