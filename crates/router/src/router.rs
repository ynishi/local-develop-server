//! `McpRouter`: registry of [`RouteClient`]s keyed by route name, plus
//! `<route>://<tool>` URI dispatch.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{CallToolResult, Tool};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::client::RouteClient;
use crate::config::RouteConfig;
use crate::error::RouterError;

/// Reserved URI scheme for the `lds` server's own tool surface.
///
/// `mcp_call` URIs beginning with this scheme are rejected before any route
/// lookup is attempted, so a session cannot be pointed at itself and spawn a
/// subprocess of its own `lds` server.
pub const LDS_SELF_SCHEME: &str = "lds";

/// Registry of upstream MCP routes for a single `lds` session.
///
/// Cheaply `Clone`-able: internally backed by `Arc<RwLock<HashMap<...>>>`,
/// so callers can clone a handle, drop an outer lock guard, and operate on
/// the router without holding that outer lock across an `.await` (this is
/// the pattern `crates/lds/src/main.rs`'s `mcp_call` tool fn is designed to
/// use against `Inner`'s own `Arc<RwLock<Inner>>`).
///
/// # Concurrency
/// Each registered [`RouteClient`] is stored behind its own `Arc`, so the
/// registry's read guard is held only across the `HashMap` lookup + `Arc`
/// clone — never across the `.await` of a proxied `call_tool`. [`McpRouter::call`]
/// clones the target route's `Arc<RouteClient>` and drops the read guard
/// before awaiting the upstream call, per Rust Architecture Baseline's
/// `Arc<Mutex<T>>`/`RwLock<T>` 系原則 (Outline `rust` book §4-1, K-4).
///
/// Dropping the last `McpRouter` handle drops the underlying registry,
/// which in turn drops every registered [`RouteClient`] whose `Arc` strong
/// count reaches zero — cascading that route's `Drop`-time subprocess
/// cleanup.
#[derive(Clone, Default)]
pub struct McpRouter {
    routes: Arc<RwLock<HashMap<String, Arc<RouteClient>>>>,
}

impl McpRouter {
    /// Build an empty router (no routes registered).
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a router pre-populated with `routes` (typically the result of
    /// [`crate::RouteConfig::load`]).
    pub fn from_configs(routes: Vec<RouteConfig>) -> Self {
        let map = routes
            .into_iter()
            .map(|route| (route.name.clone(), Arc::new(RouteClient::new(route))))
            .collect();
        Self {
            routes: Arc::new(RwLock::new(map)),
        }
    }

    /// Register (or replace) a route in the router registry.
    ///
    /// # Concurrency
    /// Acquires the internal `RwLock` write lock for the duration of the HashMap
    /// insert only; no `.await` is held across the lock (per Rust Architecture
    /// Baseline `Arc<Mutex<T>>` 系原則の RwLock 版方針). Cancelling the caller's
    /// future while queued for the write lock causes this task to lose its place
    /// in tokio's FIFO fairness queue (see `tokio::sync::RwLock::write` contract);
    /// re-registration must be retried by the caller.
    ///
    /// If `route.name` already exists, the previous `Arc<RouteClient>` is
    /// dropped from the registry; the underlying `RouteClient` (and its
    /// subprocess cascade kill via `Drop for RouteClient`) is only cleaned up
    /// once every other outstanding `Arc` clone (e.g. an in-flight
    /// [`McpRouter::call`]) has also dropped its handle. `Send + Sync`
    /// because `RouteClient` holds only `Send + Sync` primitives
    /// (`tokio::process::Child`, `rmcp::service::RunningService`).
    pub async fn register(&self, route: RouteConfig) -> Result<(), RouterError> {
        let mut guard = self.routes.write().await;
        guard.insert(route.name.clone(), Arc::new(RouteClient::new(route)));
        Ok(())
    }

    /// Remove a registered route by name.
    ///
    /// Dropping the removed [`RouteClient`] cascades its subprocess cleanup
    /// via `Drop`. Removing a name that is not registered is not an error.
    pub async fn remove(&self, name: &str) -> Result<(), RouterError> {
        let mut guard = self.routes.write().await;
        guard.remove(name);
        Ok(())
    }

    /// List the names of all currently registered routes (unordered).
    pub async fn list_routes(&self) -> Vec<String> {
        let guard = self.routes.read().await;
        guard.keys().cloned().collect()
    }

    /// Proxy a tool call to a specific route by name.
    ///
    /// # Concurrency
    /// The registry's read guard is held only across the `HashMap` lookup +
    /// `Arc<RouteClient>` clone; it is dropped before the upstream
    /// `call_tool` `.await`, so a slow or hung upstream call never blocks
    /// concurrent [`McpRouter::register`]/[`McpRouter::remove`] callers
    /// waiting on the write lock (Outline `rust` book §4-1, K-4).
    ///
    /// # Errors
    /// [`RouterError::RouteNotFound`] if `route` is not registered.
    pub async fn call(
        &self,
        route: &str,
        tool: &str,
        args: Value,
    ) -> Result<CallToolResult, RouterError> {
        let guard = self.routes.read().await;
        let client = guard
            .get(route)
            .cloned()
            .ok_or_else(|| RouterError::RouteNotFound(route.to_string()))?;
        drop(guard);
        client.call_tool(tool, args).await
    }

    /// Fetch the upstream tool list for a specific route by name.
    ///
    /// Used by [`crate::ExportRegistry::refresh`] to re-fetch the schemas of
    /// a route's declared exported tools; not itself routed through a
    /// `<route>://<tool>` URI since it has no `<tool>` component.
    ///
    /// # Concurrency
    /// Mirrors [`McpRouter::call`]: the registry's read guard is held only
    /// across the `HashMap` lookup + `Arc<RouteClient>` clone, dropped
    /// before the upstream `list_tools` `.await` (Outline `rust` book §4-1,
    /// K-4).
    ///
    /// # Errors
    /// [`RouterError::RouteNotFound`] if `route` is not registered.
    pub async fn list_upstream_tools(&self, route: &str) -> Result<Vec<Tool>, RouterError> {
        let guard = self.routes.read().await;
        let client = guard
            .get(route)
            .cloned()
            .ok_or_else(|| RouterError::RouteNotFound(route.to_string()))?;
        drop(guard);
        client.list_tools().await
    }

    /// Proxy a tool call addressed by a `<route>://<tool>` URI.
    ///
    /// Rejects the reserved `lds://` self-loop scheme before attempting a
    /// registry lookup (so a session cannot spawn a subprocess of itself).
    ///
    /// # Errors
    /// - [`RouterError::InvalidUri`] if `uri` is not `<route>://<tool>` shaped.
    /// - [`RouterError::SelfLoop`] if the parsed route uses the `lds://` scheme.
    /// - [`RouterError::RouteNotFound`] if the parsed route is not registered.
    pub async fn call_uri(&self, uri: &str, args: Value) -> Result<CallToolResult, RouterError> {
        let (route, tool) = parse_uri(uri)?;
        if route == LDS_SELF_SCHEME {
            return Err(RouterError::SelfLoop(uri.to_string()));
        }
        self.call(route, tool, args).await
    }
}

impl Drop for McpRouter {
    fn drop(&mut self) {
        if Arc::strong_count(&self.routes) == 1 {
            tracing::debug!("last McpRouter handle dropped; cascading RouteClient cleanup");
        }
    }
}

/// Split a `<route>://<tool>` URI into its `(route, tool)` components.
fn parse_uri(uri: &str) -> Result<(&str, &str), RouterError> {
    uri.split_once("://")
        .filter(|(route, tool)| !route.is_empty() && !tool.is_empty())
        .ok_or_else(|| RouterError::InvalidUri(uri.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_route(name: &str, command: &str) -> RouteConfig {
        RouteConfig {
            name: name.to_string(),
            command: command.to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            timeout_secs: 30,
        }
    }

    /// Acceptance Criteria 2: `router_rejects_self_loop_lds_scheme`.
    #[tokio::test]
    async fn router_rejects_self_loop_lds_scheme() {
        let router = McpRouter::new();
        let result = router.call_uri("lds://session_info", Value::Null).await;
        match result {
            Err(RouterError::SelfLoop(uri)) => {
                assert_eq!(uri, "lds://session_info");
            }
            other => panic!("expected SelfLoop error, got {other:?}"),
        }
    }

    /// Boundary: a non-`lds://` URI that is not `<route>://<tool>` shaped
    /// (e.g. missing the tool component) is rejected as invalid, not
    /// silently routed.
    #[tokio::test]
    async fn router_rejects_malformed_uri() {
        let router = McpRouter::new();
        let result = router.call_uri("outline", Value::Null).await;
        assert!(matches!(result, Err(RouterError::InvalidUri(_))));
    }

    /// Acceptance Criteria 2: `router_register_replaces_on_duplicate_name`.
    #[tokio::test]
    async fn router_register_replaces_on_duplicate_name() {
        let router = McpRouter::new();
        router
            .register(sample_route("echo", "cmd-a"))
            .await
            .unwrap(); // justification: McpRouter::register is infallible (Ok(()) unconditionally)
        router
            .register(sample_route("echo", "cmd-b"))
            .await
            .unwrap(); // justification: McpRouter::register is infallible (Ok(()) unconditionally)

        let routes = router.list_routes().await;
        assert_eq!(routes, vec!["echo".to_string()]);

        let guard = router.routes.read().await;
        let client = guard.get("echo").expect("echo route should be registered");
        assert_eq!(client.command(), "cmd-b");
    }

    /// Boundary: calling an unregistered route returns `RouteNotFound`.
    #[tokio::test]
    async fn router_call_unregistered_route_returns_not_found() {
        let router = McpRouter::new();
        let result = router.call("missing", "some_tool", Value::Null).await;
        assert!(matches!(result, Err(RouterError::RouteNotFound(name)) if name == "missing"));
    }

    /// Boundary: fetching the upstream tool list for an unregistered route
    /// returns `RouteNotFound` without attempting to spawn anything.
    #[tokio::test]
    async fn router_list_upstream_tools_unregistered_route_returns_not_found() {
        let router = McpRouter::new();
        let result = router.list_upstream_tools("missing").await;
        assert!(matches!(result, Err(RouterError::RouteNotFound(name)) if name == "missing"));
    }
}
