#![warn(missing_docs)]

//! Outline module for local-develop-server (lds).
//!
//! ## Architecture
//!
//! Thin wrapper around `outline-mcp-rmcp::OutlineMcpServer`. Delegates
//! `list_tools` / `call_tool` / `list_resources` / `read_resource` to the
//! upstream MCP server while rewriting tool names with an `outline_`
//! prefix so they slot into lds's flat namespace without colliding with
//! other lds tools.
//!
//! ## Design
//!
//! - Shelf directory is resolved by the caller (typically `main.rs`);
//!   the module itself is location-agnostic. Default in lds is
//!   `$HOME/.config/outline-mcp/books` (matches standalone outline-mcp
//!   binary — Book SoT is global across projects, not per-session).
//! - The `outline_` tool-name prefix is applied in `list_tools` and
//!   stripped in `call_tool`; upstream tool identity (`shelf`,
//!   `node_create`, `toc`, …) is preserved on the wire between lds and
//!   `OutlineMcpServer`.
//! - Resource URIs (`outline://guides/*`) are forwarded verbatim — no
//!   prefix rewriting needed because the scheme already namespaces them.

/// [`OutlineModule`] wrapping the upstream `OutlineMcpServer`.
pub mod module;

pub use module::OutlineModule;
pub use outline_mcp_rmcp;
