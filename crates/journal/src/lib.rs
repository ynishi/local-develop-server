#![warn(missing_docs)]

//! Journal module for local-develop-server (lds).
//!
//! ## Architecture
//!
//! Thin wrapper around `journal-mcp-rmcp::JournalMcpServer`. Delegates
//! `list_tools` / `call_tool` / `list_resources` / `read_resource` to the
//! upstream MCP server. Unlike `lds-outline`, no name-prefix rewriting is
//! needed — journal-mcp's 18 tools are already namespaced with the
//! `journal_` prefix in-crate (`journal_open_chapter`,
//! `journal_append_section`, …), so the wrapper just detects that prefix
//! on incoming tool calls and forwards verbatim.
//!
//! ## Design
//!
//! - Session root is passed at construction (typically
//!   `session.root().to_path_buf()` from `crates/lds/src/main.rs`); the
//!   module itself is location-agnostic.
//! - File projection (rendering the journal to a Markdown file) is
//!   opt-in: the caller passes `Some(path)` in `RunConfig::file_projection`
//!   to attach it at startup. Runtime attach/detach continues to work
//!   through the `journal_projection_*` MCP tools.
//! - Resource URIs (`journal-mcp://*` if any) are forwarded verbatim
//!   through the ServerHandler.

/// [`JournalModule`] wrapping the upstream `JournalMcpServer`.
pub mod module;

pub use journal_mcp_rmcp;
pub use module::JournalModule;
