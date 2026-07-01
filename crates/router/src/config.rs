//! `config.toml` `[[route]]` / `[[export]]` parsing: user-global +
//! project-local route declarations.
//!
//! `[[route]]` and `[[export]]` blocks live in the same `config.toml` file
//! that `lds_core::config::Config` also reads (`~/.config/lds/config.toml`
//! for the user-global file, `<session_root>/config.toml` for the
//! project-local override). This module never depends on `lds-core`'s
//! `Config` type: it deserializes only the `route`/`export` array-of-tables
//! out of the file and relies on serde's default "ignore unrecognized keys"
//! behavior to skip `Config`-owned sections (`[recipes]`, `[paths]`) — see
//! `lds_core::config`'s module doc comment for the other half of this split.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::error::RouterError;

/// A single upstream MCP route declared in a `config.toml` `[[route]]`
/// block.
///
/// `${LDS_SESSION_ROOT}` occurrences in `args` and `env` values are expanded
/// at load time to the session's root directory (see [`RouteConfig::load`]);
/// `name` and `command` are never expanded.
#[derive(Debug, Clone, Deserialize)]
pub struct RouteConfig {
    /// The route's unique name: the `<route>` component of `<route>://<tool>`
    /// URIs and the registry key in [`crate::McpRouter`].
    pub name: String,
    /// The subprocess command to spawn (resolved via `PATH`).
    pub command: String,
    /// Command-line arguments passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables set on the spawned subprocess.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Per-call timeout, in seconds, enforced by `RouteClient::call_tool`.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

/// Default per-route timeout when `timeout_secs` is absent from a
/// `config.toml` `[[route]]` block.
fn default_timeout_secs() -> u64 {
    30
}

/// A single upstream tool re-publication declared in a `config.toml`
/// `[[export]]` block: republish a subset of `route`'s upstream tools under
/// this session's own tool surface, prefixed to avoid name collisions.
#[derive(Debug, Clone, Deserialize)]
pub struct ExportConfig {
    /// The `[[route]]` name (see [`RouteConfig::name`]) this declaration
    /// re-publishes tools from.
    pub route: String,
    /// Upstream tool names to re-publish, exactly as advertised by the
    /// upstream route (not yet prefixed).
    pub tools: Vec<String>,
    /// Public tool name prefix; defaults to `"<route>_"` (see
    /// [`ExportConfig::effective_prefix`]) when omitted.
    #[serde(default)]
    pub prefix: Option<String>,
}

impl ExportConfig {
    /// The effective public-name prefix: `prefix` if set, else `"<route>_"`.
    pub fn effective_prefix(&self) -> String {
        self.prefix
            .clone()
            .unwrap_or_else(|| format!("{}_", self.route))
    }
}

/// The `[[route]]` / `[[export]]` array-of-tables shape parsed out of a
/// `config.toml` file.
///
/// Has no `#[serde(deny_unknown_fields)]`, so a real `config.toml` — which
/// also carries `[recipes]` / `[paths]` sections owned by
/// `lds_core::config::Config` — deserializes here with those sections
/// silently ignored (see this module's doc comment).
#[derive(Debug, Default, Deserialize)]
struct RoutesFile {
    #[serde(default)]
    route: Vec<RouteConfig>,
    #[serde(default)]
    export: Vec<ExportConfig>,
}

impl RouteConfig {
    /// Load and merge user-global and project-local route declarations.
    ///
    /// A thin wrapper over [`RouteConfig::load_all`] that discards the
    /// `[[export]]` half for callers that only care about routes.
    ///
    /// # Errors / Concurrency
    /// See [`RouteConfig::load_all`].
    pub fn load(
        user_path: &Path,
        project_path: &Path,
        session_root: &Path,
    ) -> Result<Vec<RouteConfig>, RouterError> {
        Self::load_all(user_path, project_path, session_root).map(|(routes, _)| routes)
    }

    /// Load and merge user-global and project-local `[[route]]` *and*
    /// `[[export]]` declarations out of `config.toml` files.
    ///
    /// `user_path` and `project_path` both point at a `config.toml` (the
    /// same file `lds_core::config::Config` reads for `[recipes]` /
    /// `[paths]` — typically `~/.config/lds/config.toml` and
    /// `<session_root>/config.toml`, respectively). Reads `user_path` first,
    /// then `project_path`; a declaration in `project_path` with the same
    /// key as one from `user_path` replaces it entirely (project overrides
    /// user) — keyed by `name` for routes and by `route` for exports. A
    /// missing file is treated as an empty declaration set, not an error —
    /// `[[route]]`/`[[export]]` are optional at both levels, and so is the
    /// file itself.
    ///
    /// `${LDS_SESSION_ROOT}` occurrences in each route's `args` and `env`
    /// values are expanded to `session_root`'s string representation before
    /// the route is returned. Export declarations have no
    /// `${LDS_SESSION_ROOT}`-eligible fields, so none are expanded.
    ///
    /// # Errors
    ///
    /// - [`RouterError::Config`] if either file exists but is not valid TOML.
    /// - [`RouterError::Spawn`] if a file exists but cannot be read for a
    ///   reason other than "not found" (see [`RouterError`] doc comment for
    ///   why this reuses the spawn-failure variant).
    ///
    /// # Concurrency
    /// Performs synchronous filesystem I/O (`std::fs::read_to_string`) and
    /// is not itself `async`. Callers invoking this from an `async fn` (e.g.
    /// the `lds` binary crate's `wire_router_and_exports`) must wrap the
    /// call in `tokio::task::spawn_blocking` to avoid blocking a tokio
    /// worker thread — no exception applies regardless of file size (Rust
    /// Book §4-1-4 / K-110, K-126, K-134).
    pub fn load_all(
        user_path: &Path,
        project_path: &Path,
        session_root: &Path,
    ) -> Result<(Vec<RouteConfig>, Vec<ExportConfig>), RouterError> {
        let (mut routes, mut exports) = Self::load_file_all(user_path, session_root)?;
        let (project_routes, project_exports) = Self::load_file_all(project_path, session_root)?;
        for route in project_routes {
            match routes.iter_mut().find(|r| r.name == route.name) {
                Some(existing) => *existing = route,
                None => routes.push(route),
            }
        }
        for export in project_exports {
            match exports.iter_mut().find(|e| e.route == export.route) {
                Some(existing) => *existing = export,
                None => exports.push(export),
            }
        }
        Ok((routes, exports))
    }

    /// Read and parse a single `config.toml` file's `[[route]]`/`[[export]]`
    /// sections (ignoring any `[recipes]`/`[paths]` sections it also
    /// carries), expanding `${LDS_SESSION_ROOT}` in route `args`/`env`
    /// values. Returns empty `Vec`s for both halves if `path` does not
    /// exist.
    fn load_file_all(
        path: &Path,
        session_root: &Path,
    ) -> Result<(Vec<RouteConfig>, Vec<ExportConfig>), RouterError> {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Vec::new(), Vec::new()));
            }
            Err(e) => return Err(RouterError::Spawn(e)),
        };
        let file: RoutesFile = toml::from_str(&content)?;
        let routes = file
            .route
            .into_iter()
            .map(|route| route.expand_session_root(session_root))
            .collect();
        Ok((routes, file.export))
    }

    /// Return `self` with `${LDS_SESSION_ROOT}` expanded in `args` and `env`
    /// values.
    fn expand_session_root(mut self, session_root: &Path) -> Self {
        let root = session_root.to_string_lossy();
        for arg in &mut self.args {
            *arg = arg.replace("${LDS_SESSION_ROOT}", root.as_ref());
        }
        for value in self.env.values_mut() {
            *value = value.replace("${LDS_SESSION_ROOT}", root.as_ref());
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Acceptance Criteria 2: `route_config_parses_toml_and_expands_session_root`.
    #[test]
    fn route_config_parses_toml_and_expands_session_root() {
        let dir = tempfile::tempdir().expect("tempdir"); // justification: tempdir creation mirrors crates/core/src/config.rs test pattern
        let user_path = dir.path().join("user-config.toml");
        let project_path = dir.path().join("project-config.toml"); // left unwritten: missing file is a valid empty route set

        std::fs::write(
            &user_path,
            r#"
[[route]]
name = "outline"
command = "outline-mcp"
args = ["--db", "${LDS_SESSION_ROOT}/.outline.db"]
env = { OUTLINE_LOG_LEVEL = "info" }
timeout_secs = 45

[[route]]
name = "mini-app"
command = "mini-app-mcp"
"#,
        )
        .expect("write user config.toml"); // justification: writing known-good TOML in test, mirrors crates/core/src/config.rs pattern

        let session_root = PathBuf::from("/tmp/lds-session-abc");
        let routes = RouteConfig::load(&user_path, &project_path, &session_root)
            .expect("route config should parse");

        assert_eq!(routes.len(), 2);

        let outline = routes
            .iter()
            .find(|r| r.name == "outline")
            .expect("outline route should be present");
        assert_eq!(outline.command, "outline-mcp");
        assert_eq!(
            outline.args,
            vec![
                "--db".to_string(),
                "/tmp/lds-session-abc/.outline.db".to_string()
            ]
        );
        assert_eq!(
            outline.env.get("OUTLINE_LOG_LEVEL"),
            Some(&"info".to_string())
        );
        assert_eq!(outline.timeout_secs, 45);

        let mini_app = routes
            .iter()
            .find(|r| r.name == "mini-app")
            .expect("mini-app route should be present");
        assert_eq!(mini_app.command, "mini-app-mcp");
        assert!(mini_app.args.is_empty());
        assert_eq!(mini_app.timeout_secs, 30, "default timeout should be 30");
    }

    /// Boundary: both files absent yields an empty route set, not an error.
    #[test]
    fn route_config_load_missing_files_returns_empty() {
        let dir = tempfile::tempdir().expect("tempdir"); // justification: tempdir creation mirrors crates/core/src/config.rs test pattern
        let user_path = dir.path().join("nonexistent-user.toml");
        let project_path = dir.path().join("nonexistent-project.toml");

        let routes =
            RouteConfig::load(&user_path, &project_path, Path::new("/tmp/session")).unwrap(); // justification: both config.toml files are absent by construction; load() cannot fail on this path
        assert!(routes.is_empty());
    }

    /// Boundary: a project route with the same name as a user route replaces
    /// it entirely (project overrides user).
    #[test]
    fn route_config_project_overrides_user_route_by_name() {
        let dir = tempfile::tempdir().expect("tempdir"); // justification: tempdir creation mirrors crates/core/src/config.rs test pattern
        let user_path = dir.path().join("user-config.toml");
        let project_path = dir.path().join("project-config.toml");

        std::fs::write(
            &user_path,
            r#"
[[route]]
name = "outline"
command = "user-command"
"#,
        )
        .expect("write user config.toml"); // justification: writing known-good TOML in test
        std::fs::write(
            &project_path,
            r#"
[[route]]
name = "outline"
command = "project-command"
"#,
        )
        .expect("write project config.toml"); // justification: writing known-good TOML in test

        let routes =
            RouteConfig::load(&user_path, &project_path, Path::new("/tmp/session")).unwrap(); // justification: both config.toml files are known-good TOML written above; load() cannot fail on this path
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].command, "project-command");
    }

    /// `[[export]]` blocks parse alongside `[[route]]` blocks, with
    /// `prefix` defaulting to `"<route>_"` when omitted.
    #[test]
    fn load_all_parses_export_blocks_with_default_and_explicit_prefix() {
        let dir = tempfile::tempdir().expect("tempdir"); // justification: tempdir creation mirrors crates/core/src/config.rs test pattern
        let user_path = dir.path().join("user-config.toml");
        let project_path = dir.path().join("project-config.toml"); // left unwritten: missing file is a valid empty declaration set

        std::fs::write(
            &user_path,
            r#"
[[route]]
name = "outline"
command = "outline-mcp"

[[export]]
route = "outline"
tools = ["snapshot_create", "snapshot_list"]

[[export]]
route = "mini-app"
tools = ["create"]
prefix = "ma_"
"#,
        )
        .expect("write user config.toml"); // justification: writing known-good TOML in test

        let (routes, exports) =
            RouteConfig::load_all(&user_path, &project_path, Path::new("/tmp/session")).unwrap(); // justification: user config.toml is known-good TOML written above; load_all() cannot fail on this path
        assert_eq!(routes.len(), 1);
        assert_eq!(exports.len(), 2);

        let outline_export = exports
            .iter()
            .find(|e| e.route == "outline")
            .expect("outline export declaration should be present");
        assert_eq!(
            outline_export.tools,
            vec!["snapshot_create".to_string(), "snapshot_list".to_string()]
        );
        assert_eq!(outline_export.effective_prefix(), "outline_");

        let mini_app_export = exports
            .iter()
            .find(|e| e.route == "mini-app")
            .expect("mini-app export declaration should be present");
        assert_eq!(mini_app_export.effective_prefix(), "ma_");
    }

    /// Boundary: a project export declaration for the same `route` as a
    /// user-global one replaces it entirely (project overrides user).
    #[test]
    fn load_all_project_overrides_user_export_by_route() {
        let dir = tempfile::tempdir().expect("tempdir"); // justification: tempdir creation mirrors crates/core/src/config.rs test pattern
        let user_path = dir.path().join("user-config.toml");
        let project_path = dir.path().join("project-config.toml");

        std::fs::write(
            &user_path,
            r#"
[[export]]
route = "outline"
tools = ["snapshot_create"]
"#,
        )
        .expect("write user config.toml"); // justification: writing known-good TOML in test
        std::fs::write(
            &project_path,
            r#"
[[export]]
route = "outline"
tools = ["snapshot_list"]
"#,
        )
        .expect("write project config.toml"); // justification: writing known-good TOML in test

        let (_, exports) =
            RouteConfig::load_all(&user_path, &project_path, Path::new("/tmp/session")).unwrap(); // justification: both config.toml files are known-good TOML written above; load_all() cannot fail on this path
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].tools, vec!["snapshot_list".to_string()]);
    }

    /// `RouteConfig::load_all` parses `[[route]]`/`[[export]]` out of a real
    /// `config.toml` that also carries `[recipes]`/`[paths]` sections owned
    /// by `lds_core::config::Config` — the other half of the "shared file,
    /// decoupled schemas" split (see this module's doc comment and
    /// `lds_core::config::tests::test_load_ignores_route_and_export_sections`
    /// for the reverse direction).
    #[test]
    fn load_all_ignores_unrelated_config_toml_sections() {
        let dir = tempfile::tempdir().expect("tempdir"); // justification: tempdir creation mirrors crates/core/src/config.rs test pattern
        let user_path = dir.path().join("user-config.toml");
        let project_path = dir.path().join("project-config.toml"); // left unwritten: missing file is a valid empty declaration set

        std::fs::write(
            &user_path,
            r#"
[recipes]
dirs = ["/opt/shared-recipes"]

[paths]
global_justfile = "/etc/lds/justfile"

[[route]]
name = "outline"
command = "outline-mcp"

[[export]]
route = "outline"
tools = ["snapshot_create"]
"#,
        )
        .expect("write user config.toml"); // justification: writing known-good TOML in test

        let (routes, exports) =
            RouteConfig::load_all(&user_path, &project_path, Path::new("/tmp/session")).unwrap(); // justification: user config.toml is known-good TOML written above; load_all() cannot fail on this path
        assert_eq!(
            routes.len(),
            1,
            "[recipes]/[paths] must not interfere with [[route]] parsing"
        );
        assert_eq!(routes[0].name, "outline");
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].route, "outline");
    }
}
