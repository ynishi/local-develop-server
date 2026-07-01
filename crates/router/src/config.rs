//! `routes.toml` parsing: user-global + project-local route declarations.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::error::RouterError;

/// A single upstream MCP route declared in a `routes.toml` `[[route]]`
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

/// Default per-route timeout when `timeout_secs` is absent from
/// `routes.toml`.
fn default_timeout_secs() -> u64 {
    30
}

/// The `[[route]]` array-of-tables shape of a `routes.toml` file.
#[derive(Debug, Default, Deserialize)]
struct RoutesFile {
    #[serde(default)]
    route: Vec<RouteConfig>,
}

impl RouteConfig {
    /// Load and merge user-global and project-local route declarations.
    ///
    /// Reads `user_path` first, then `project_path`; a route in
    /// `project_path` with the same `name` as one from `user_path` replaces
    /// it entirely (project overrides user). A missing file is treated as an
    /// empty route set, not an error — `routes.toml` is optional at both
    /// levels.
    ///
    /// `${LDS_SESSION_ROOT}` occurrences in each route's `args` and `env`
    /// values are expanded to `session_root`'s string representation before
    /// the route is returned.
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
    /// `crates/lds/src/main.rs`'s `build_session_modules`) must wrap the
    /// call in `tokio::task::spawn_blocking` to avoid blocking a tokio
    /// worker thread — no exception applies regardless of file size (Rust
    /// Book §4-1-4 / K-110, K-126, K-134).
    pub fn load(
        user_path: &Path,
        project_path: &Path,
        session_root: &Path,
    ) -> Result<Vec<RouteConfig>, RouterError> {
        let mut routes = Self::load_file(user_path, session_root)?;
        let project_routes = Self::load_file(project_path, session_root)?;
        for route in project_routes {
            match routes.iter_mut().find(|r| r.name == route.name) {
                Some(existing) => *existing = route,
                None => routes.push(route),
            }
        }
        Ok(routes)
    }

    /// Read and parse a single `routes.toml` file, expanding
    /// `${LDS_SESSION_ROOT}` in `args`/`env` values. Returns an empty `Vec`
    /// if `path` does not exist.
    fn load_file(path: &Path, session_root: &Path) -> Result<Vec<RouteConfig>, RouterError> {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(RouterError::Spawn(e)),
        };
        let file: RoutesFile = toml::from_str(&content)?;
        Ok(file
            .route
            .into_iter()
            .map(|route| route.expand_session_root(session_root))
            .collect())
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
        let user_path = dir.path().join("user-routes.toml");
        let project_path = dir.path().join("project-routes.toml"); // left unwritten: missing file is a valid empty route set

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
        .expect("write user routes.toml"); // justification: writing known-good TOML in test, mirrors crates/core/src/config.rs pattern

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
            RouteConfig::load(&user_path, &project_path, Path::new("/tmp/session")).unwrap(); // justification: both routes.toml files are absent by construction; load() cannot fail on this path
        assert!(routes.is_empty());
    }

    /// Boundary: a project route with the same name as a user route replaces
    /// it entirely (project overrides user).
    #[test]
    fn route_config_project_overrides_user_route_by_name() {
        let dir = tempfile::tempdir().expect("tempdir"); // justification: tempdir creation mirrors crates/core/src/config.rs test pattern
        let user_path = dir.path().join("user-routes.toml");
        let project_path = dir.path().join("project-routes.toml");

        std::fs::write(
            &user_path,
            r#"
[[route]]
name = "outline"
command = "user-command"
"#,
        )
        .expect("write user routes.toml"); // justification: writing known-good TOML in test
        std::fs::write(
            &project_path,
            r#"
[[route]]
name = "outline"
command = "project-command"
"#,
        )
        .expect("write project routes.toml"); // justification: writing known-good TOML in test

        let routes =
            RouteConfig::load(&user_path, &project_path, Path::new("/tmp/session")).unwrap(); // justification: both routes.toml files are known-good TOML written above; load() cannot fail on this path
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].command, "project-command");
    }
}
