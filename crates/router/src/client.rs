//! Lifecycle management for a single upstream MCP route's subprocess.
//!
//! A [`RouteClient`] does not spawn its subprocess until the first
//! [`RouteClient::call_tool`] call (lazy spawn). All calls to a given
//! `RouteClient` — including the one that triggers the spawn — serialize on
//! an internal `tokio::sync::Mutex`; this is intentional, not a race: a
//! single route's upstream subprocess speaks one stdio stream, so
//! concurrent calls to the same route are expected to queue rather than
//! race.

use std::collections::HashMap;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::config::RouteConfig;
use crate::error::RouterError;

/// A lazily-spawned subprocess client for one upstream MCP route.
///
/// Holds only `Send + Sync` primitives (`tokio::sync::Mutex`,
/// `rmcp::service::RunningService`, which itself owns a
/// `tokio::process::Child` internally), so `RouteClient` is `Send + Sync`
/// without any manual unsafe impl. The `rmcp` client type is never exposed
/// on this struct's public API.
pub struct RouteClient {
    name: String,
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    timeout_secs: u64,
    service: Mutex<Option<RunningService<RoleClient, ()>>>,
}

impl RouteClient {
    /// Build a `RouteClient` from a parsed route declaration.
    ///
    /// This does not spawn a subprocess; spawning is deferred to the first
    /// [`RouteClient::call_tool`] call.
    pub fn new(route: RouteConfig) -> Self {
        Self {
            name: route.name,
            command: route.command,
            args: route.args,
            env: route.env,
            timeout_secs: route.timeout_secs,
            service: Mutex::new(None),
        }
    }

    /// The route's unique name (the `<route>` component of `<route>://<tool>`
    /// URIs).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The subprocess command configured for this route.
    ///
    /// Exposed for observability (e.g. a future `mcp_route_list` tool); not
    /// used internally beyond [`RouteClient::spawn`].
    pub fn command(&self) -> &str {
        &self.command
    }

    /// The command-line arguments configured for this route.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Proxy a single tool call to the upstream MCP subprocess this client owns.
    ///
    /// # Concurrency
    /// Lazily spawns the upstream subprocess on first call (race between
    /// concurrent first-callers is resolved by the owning `McpRouter`'s
    /// `HashMap::entry` + write lock, not by this method). Wrapped by
    /// `tokio::time::timeout(Duration::from_secs(timeout_secs), ...)` at the
    /// call site; per the timeout contract, if this future does not yield
    /// during execution it can exceed the configured timeout without erroring.
    /// Cancel-safe with respect to timeout (dropping the timeout future performs
    /// no additional cleanup), but **not** cancel-safe with respect to the
    /// underlying subprocess call state: a timed-out call leaves the upstream
    /// subprocess's in-flight request undefined (upstream may still process it).
    /// Does not panic; all failure paths return `RouterError`.
    pub async fn call_tool(&self, tool: &str, args: Value) -> Result<CallToolResult, RouterError> {
        let mut guard = self.service.lock().await;
        if guard.is_none() {
            let spawned = self.spawn().await?;
            *guard = Some(spawned);
        }
        let Some(service) = guard.as_ref() else {
            // Unreachable in practice: the branch above always populates
            // `guard` on the success path, and a spawn failure already
            // returned via `?` above. Kept as a typed error rather than
            // `.expect()`/`.unwrap()`.
            return Err(RouterError::Upstream(format!(
                "route {} lost its subprocess handle unexpectedly",
                self.name
            )));
        };

        let params = build_call_params(tool, args);
        match tokio::time::timeout(
            Duration::from_secs(self.timeout_secs),
            service.call_tool(params),
        )
        .await
        {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(e)) => {
                tracing::warn!(route = %self.name, tool, error = %e, "upstream mcp call failed");
                Err(RouterError::Upstream(e.to_string()))
            }
            Err(_elapsed) => {
                tracing::warn!(
                    route = %self.name,
                    tool,
                    timeout_secs = self.timeout_secs,
                    "route call timed out"
                );
                Err(RouterError::Timeout(self.timeout_secs, tool.to_string()))
            }
        }
    }

    /// Spawn the upstream subprocess and complete the MCP client handshake.
    async fn spawn(&self) -> Result<RunningService<RoleClient, ()>, RouterError> {
        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args);
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        let transport = TokioChildProcess::new(cmd)?;
        let service = ().serve(transport).await.map_err(|e| RouterError::Upstream(e.to_string()))?;
        Ok(service)
    }
}

/// Build MCP `call_tool` request params from a tool name and a JSON `args`
/// value; a non-object `args` (e.g. `Value::Null`) is treated as "no
/// arguments".
fn build_call_params(tool: &str, args: Value) -> CallToolRequestParams {
    let params = CallToolRequestParams::new(tool.to_string());
    match args {
        Value::Object(map) => params.with_arguments(map),
        _ => params,
    }
}

impl Drop for RouteClient {
    fn drop(&mut self) {
        let Ok(mut guard) = self.service.try_lock() else {
            // An in-flight `call_tool` holds the lock; that call retains
            // ownership of the `RunningService` for its own lifetime, and
            // rmcp's own drop guards (`RunningService`, `ChildWithCleanup`)
            // still fire once it completes. Nothing more to do here.
            return;
        };
        let Some(service) = guard.take() else {
            // Never spawned: no subprocess to clean up.
            return;
        };
        let route_name = self.name.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    if let Err(e) = service.cancel().await {
                        tracing::warn!(
                            route = %route_name,
                            error = %e,
                            "failed to cleanly cancel route client during drop"
                        );
                    }
                });
            }
            Err(_) => {
                // No Tokio runtime available at drop time (e.g. dropped
                // outside any async context). `RunningService`'s own Drop
                // impl still performs best-effort subprocess cleanup.
                drop(service);
            }
        }
    }
}
