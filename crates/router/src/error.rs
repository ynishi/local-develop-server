//! Error type for the `router` crate.

use thiserror::Error;

/// Errors returned by the `router` crate's configuration, registry, and
/// subprocess-lifecycle operations.
///
/// `thiserror` permits at most one `#[from] std::io::Error` conversion per
/// enum, so [`RouterError::Spawn`] is the crate's sole vehicle for I/O
/// failures (both subprocess spawn failures and, less commonly, `routes.toml`
/// read failures other than "file not found" — see
/// [`crate::RouteConfig::load`]).
#[derive(Debug, Error)]
pub enum RouterError {
    /// No route is registered under the given name.
    #[error("route not found: {0}")]
    RouteNotFound(String),

    /// The requested URI uses the reserved `lds://` scheme, which would
    /// route back into the calling `lds` server itself.
    #[error("self-loop blocked: uri {0} uses reserved scheme lds://")]
    SelfLoop(String),

    /// The URI could not be parsed into a `<route>://<tool>` shape.
    #[error("invalid uri: {0}")]
    InvalidUri(String),

    /// The upstream subprocess did not respond within the route's
    /// configured per-call timeout.
    #[error("route timeout ({0}s): {1}")]
    Timeout(u64, String),

    /// The upstream MCP server returned an error, or the JSON-RPC exchange
    /// itself failed, for a proxied call.
    #[error("upstream mcp error: {0}")]
    Upstream(String),

    /// Spawning the upstream subprocess failed, or a `routes.toml` file
    /// could not be read for a reason other than "not found".
    #[error("subprocess spawn failed: {0}")]
    Spawn(#[from] std::io::Error),

    /// A `routes.toml` file exists but is not valid TOML.
    #[error("config parse error: {0}")]
    Config(#[from] toml::de::Error),

    /// More `[[export]]` entries were declared than the configured limit.
    #[error("export limit exceeded: {0} > 16")]
    ExportLimitExceeded(usize),

    /// Two `[[export]]` entries resolved to the same public tool prefix.
    #[error("export prefix collision: {0}")]
    ExportCollision(String),
}
