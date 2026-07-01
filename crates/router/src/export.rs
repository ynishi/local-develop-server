//! `ExportRegistry`: materializes `[[export]]`-declared upstream tools under
//! a `<prefix><tool>` public name on this session's own tool surface.
//!
//! Unlike [`crate::McpRouter`]'s routes (reachable only via the explicit
//! `<route>://<tool>` proxy URI), an exported tool appears directly in the
//! session's `list_tools` response and can be called by its prefixed name
//! without a caller ever knowing it is being proxied.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::Tool;
use tokio::sync::RwLock;

use crate::config::ExportConfig;
use crate::error::RouterError;
use crate::router::McpRouter;

/// Default maximum number of tools a session's `[[export]]` declarations may
/// materialize in total, across every declaration.
///
/// v0.6.0 hardcodes this; a `config.toml`-level override (e.g. a per-session
/// `max` field) is a natural follow-up if 16 proves too small in practice.
pub const DEFAULT_EXPORT_LIMIT: usize = 16;

/// One materialized export: the upstream `(route, tool)` pair an exported
/// tool's public name dispatches to, plus the `Tool` advertised to callers.
#[derive(Clone)]
struct ExportedTool {
    route: String,
    upstream_tool: String,
    tool: Tool,
}

/// Registry of `[[export]]`-declared upstream tools, materialized into a
/// prefixed public tool surface.
///
/// Cheaply `Clone`-able: internally backed by `Arc<RwLock<...>>`, mirroring
/// [`crate::McpRouter`]'s handle-clone pattern so callers (the `lds` binary
/// crate's `mcp_export_refresh` tool handler) can clone a handle, drop an
/// outer lock guard, and `.await` a re-fetch of upstream tool schemas
/// without holding that outer lock across the network round trip.
///
/// # Concurrency
/// [`ExportRegistry::refresh`] builds the entire new materialized tool set
/// in a local `HashMap` *before* acquiring the internal write lock; the lock
/// is held only for the final swap, never across an upstream `list_tools`
/// `.await` (Outline `rust` book §4-1, K-4). [`ExportRegistry::list_tools`]
/// and [`ExportRegistry::resolve`] acquire only a read lock, so a
/// long-running `refresh` never blocks concurrent tool-call dispatch from
/// observing the previous (still-valid) snapshot.
#[derive(Clone)]
pub struct ExportRegistry {
    declarations: Arc<Vec<ExportConfig>>,
    max_exports: usize,
    tools: Arc<RwLock<HashMap<String, ExportedTool>>>,
}

impl ExportRegistry {
    /// Build a registry from parsed `[[export]]` declarations.
    ///
    /// No tools are materialized yet — call [`ExportRegistry::refresh`] to
    /// fetch upstream tool schemas and populate the public tool surface.
    /// Uses [`DEFAULT_EXPORT_LIMIT`] as the materialized-tool-count ceiling.
    pub fn from_declarations(declarations: Vec<ExportConfig>) -> Self {
        Self {
            declarations: Arc::new(declarations),
            max_exports: DEFAULT_EXPORT_LIMIT,
            tools: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Override the materialized-tool-count ceiling (default
    /// [`DEFAULT_EXPORT_LIMIT`]).
    ///
    /// v0.6.0 has no `config.toml`-level way to set this in production; it
    /// exists as a builder method so tests can exercise
    /// [`RouterError::ExportLimitExceeded`] without declaring 17 real
    /// tools, and so a future config-driven override has a landing spot.
    pub fn with_max_exports(mut self, max_exports: usize) -> Self {
        self.max_exports = max_exports;
        self
    }

    /// Re-fetch the upstream tool list for every declared route via
    /// `router`, filter to each declaration's `tools` names, prefix them,
    /// and atomically replace the materialized tool set.
    ///
    /// A declaration whose route fails to respond to `list_tools` (route
    /// not registered, subprocess spawn failure, upstream error) is logged
    /// as a warning and skipped — it contributes no tools to this refresh
    /// rather than failing it outright, so one misbehaving upstream route
    /// does not prevent an otherwise-healthy session from starting. A
    /// declared tool name absent from its route's live tool list is
    /// likewise logged and skipped.
    ///
    /// By contrast, [`RouterError::ExportLimitExceeded`] and
    /// [`RouterError::ExportCollision`] are hard failures: both indicate a
    /// `config.toml` configuration that cannot be resolved into an
    /// unambiguous tool surface, so callers (`session_start` and
    /// `mcp_export_refresh` in the `lds` binary crate) must propagate the
    /// error rather than silently drop exports.
    ///
    /// # Concurrency
    /// See the struct-level doc comment. Additionally, the per-declaration
    /// upstream `list_tools` round trips are issued concurrently via
    /// [`futures::future::join_all`] — one slow or unreachable route no
    /// longer delays every other declaration's fetch. The subsequent
    /// matching/collision/limit-check pass is strictly sequential and walks
    /// `self.declarations` in its original (config file) order, so the
    /// materialized result is deterministic regardless of which upstream
    /// responded first.
    ///
    /// # Errors
    /// - [`RouterError::ExportLimitExceeded`] if the materialized tool count
    ///   exceeds this registry's configured limit.
    /// - [`RouterError::ExportCollision`] if two declarations, or a
    ///   declaration and an entry in `static_tool_names`, resolve to the
    ///   same public tool name.
    pub async fn refresh(
        &self,
        router: &McpRouter,
        static_tool_names: &[String],
    ) -> Result<(), RouterError> {
        // Fan out: one upstream `list_tools` call per declaration, run
        // concurrently. `join_all` preserves input order in its output
        // `Vec`, so zipping it back against `self.declarations` below stays
        // index-aligned even though completion order is unconstrained.
        let upstream_results: Vec<Result<Vec<Tool>, RouterError>> = futures::future::join_all(
            self.declarations
                .iter()
                .map(|decl| router.list_upstream_tools(&decl.route)),
        )
        .await;

        let mut materialized: HashMap<String, ExportedTool> = HashMap::new();
        for (decl, upstream_result) in self.declarations.iter().zip(upstream_results) {
            let upstream_tools = match upstream_result {
                Ok(tools) => tools,
                Err(e) => {
                    tracing::warn!(
                        route = %decl.route,
                        error = %e,
                        "export declaration's route failed to list upstream tools; skipping"
                    );
                    continue;
                }
            };
            let prefix = decl.effective_prefix();
            for tool_name in &decl.tools {
                let Some(upstream) = upstream_tools
                    .iter()
                    .find(|t| t.name.as_ref() == tool_name.as_str())
                else {
                    tracing::warn!(
                        route = %decl.route,
                        tool = %tool_name,
                        "declared export tool not found in upstream tool list; skipping"
                    );
                    continue;
                };
                let public_name = format!("{prefix}{tool_name}");
                if materialized.contains_key(&public_name)
                    || static_tool_names.iter().any(|n| n == &public_name)
                {
                    return Err(RouterError::ExportCollision(public_name));
                }
                let mut tool = upstream.clone();
                tool.name = public_name.clone().into();
                materialized.insert(
                    public_name,
                    ExportedTool {
                        route: decl.route.clone(),
                        upstream_tool: tool_name.clone(),
                        tool,
                    },
                );
            }
        }
        if materialized.len() > self.max_exports {
            return Err(RouterError::ExportLimitExceeded(materialized.len()));
        }
        let mut guard = self.tools.write().await;
        *guard = materialized;
        Ok(())
    }

    /// List all currently materialized export tools (unordered).
    pub async fn list_tools(&self) -> Vec<Tool> {
        let guard = self.tools.read().await;
        guard.values().map(|e| e.tool.clone()).collect()
    }

    /// If `name` matches a materialized export tool's public name, return
    /// the `(route, upstream_tool)` pair it should be dispatched to.
    pub async fn resolve(&self, name: &str) -> Option<(String, String)> {
        let guard = self.tools.read().await;
        guard
            .get(name)
            .map(|e| (e.route.clone(), e.upstream_tool.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config(route: &str, tools: &[&str], prefix: Option<&str>) -> ExportConfig {
        ExportConfig {
            route: route.to_string(),
            tools: tools.iter().map(|s| s.to_string()).collect(),
            prefix: prefix.map(|s| s.to_string()),
        }
    }

    /// Boundary: a route absent from `router` is skipped (warn + continue)
    /// rather than failing the whole refresh, and `list_tools` reflects the
    /// empty result.
    #[tokio::test]
    async fn refresh_skips_declaration_for_unregistered_route() {
        let router = McpRouter::new();
        let registry =
            ExportRegistry::from_declarations(vec![sample_config("missing", &["tool_a"], None)]);

        registry.refresh(&router, &[]).await.unwrap(); // justification: unregistered routes are skipped, not errored — refresh cannot fail on this path

        assert!(registry.list_tools().await.is_empty());
        assert!(registry.resolve("missing_tool_a").await.is_none());
    }

    /// Boundary: an empty declaration set never exceeds the limit and never
    /// materializes anything.
    #[tokio::test]
    async fn refresh_with_no_declarations_is_a_no_op() {
        let router = McpRouter::new();
        let registry = ExportRegistry::from_declarations(Vec::new());

        registry.refresh(&router, &[]).await.unwrap(); // justification: no declarations means no upstream calls; refresh cannot fail on this path

        assert!(registry.list_tools().await.is_empty());
    }
}
