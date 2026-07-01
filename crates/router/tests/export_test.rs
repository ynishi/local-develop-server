//! Integration tests for `ExportRegistry` against a live (mock) upstream
//! MCP subprocess: `[[export]]`-declared tools materialize with a prefixed
//! public name, and `refresh` re-picks upstream schema changes.

use lds_router::{ExportConfig, ExportRegistry, McpRouter, RouteConfig};
use serde_json::json;

fn mock_upstream_bin() -> String {
    std::env::var("CARGO_BIN_EXE_lds_router_mock_upstream").unwrap_or_else(|_| {
        format!(
            "{}/../../target/debug/lds_router_mock_upstream",
            env!("CARGO_MANIFEST_DIR")
        )
    })
}

fn sample_route(name: &str, tools_file: &std::path::Path) -> RouteConfig {
    RouteConfig {
        name: name.to_string(),
        command: mock_upstream_bin(),
        args: vec![
            "--tools-file".to_string(),
            tools_file.to_string_lossy().to_string(),
        ],
        env: Default::default(),
        timeout_secs: 30,
    }
}

fn write_tools_file(path: &std::path::Path, tools: &serde_json::Value) {
    std::fs::write(path, tools.to_string()).expect("write mock upstream tools file"); // justification: writing known-good JSON to a fresh tempdir path in a test
}

/// Acceptance Criteria 2: `export_registry_materializes_declared_tools_with_prefix`.
#[tokio::test]
async fn export_registry_materializes_declared_tools_with_prefix() {
    let dir = tempfile::tempdir().expect("tempdir"); // justification: tempdir creation mirrors crates/router/src/config.rs test pattern
    let tools_file = dir.path().join("tools.json");
    write_tools_file(
        &tools_file,
        &json!([
            {
                "name": "greet",
                "description": "says hello",
                "input_schema": {
                    "type": "object",
                    "properties": { "name": { "type": "string" } },
                    "required": ["name"]
                }
            },
            {
                "name": "unrelated_tool",
                "description": "not declared for export"
            }
        ]),
    );

    let router = McpRouter::from_configs(vec![sample_route("outline", &tools_file)]);
    let registry = ExportRegistry::from_declarations(vec![ExportConfig {
        route: "outline".to_string(),
        tools: vec!["greet".to_string()],
        prefix: None,
    }]);

    registry.refresh(&router, &[]).await.unwrap(); // justification: config.toml-shaped declaration and a live mock upstream are both known-good in this test

    let tools = registry.list_tools().await;
    assert_eq!(tools.len(), 1, "only the declared tool should be exported");
    let exported = &tools[0];
    assert_eq!(exported.name.as_ref(), "outline_greet");
    assert_eq!(
        exported.input_schema.get("required"),
        Some(&json!(["name"]))
    );

    // The non-declared upstream tool must not leak through under any name.
    assert!(!tools.iter().any(|t| t.name.as_ref().contains("unrelated")));

    // The public name resolves back to the correct (route, upstream tool).
    let resolved = registry.resolve("outline_greet").await;
    assert_eq!(resolved, Some(("outline".to_string(), "greet".to_string())));
}

/// Acceptance Criteria 2: `export_refresh_repicks_upstream_schema_changes`.
#[tokio::test]
async fn export_refresh_repicks_upstream_schema_changes() {
    let dir = tempfile::tempdir().expect("tempdir"); // justification: tempdir creation mirrors crates/router/src/config.rs test pattern
    let tools_file = dir.path().join("tools.json");
    write_tools_file(
        &tools_file,
        &json!([
            {
                "name": "greet",
                "description": "says hello",
                "input_schema": {
                    "type": "object",
                    "properties": { "name": { "type": "string" } }
                }
            }
        ]),
    );

    let router = McpRouter::from_configs(vec![sample_route("outline", &tools_file)]);
    let registry = ExportRegistry::from_declarations(vec![ExportConfig {
        route: "outline".to_string(),
        tools: vec!["greet".to_string()],
        prefix: None,
    }]);

    registry.refresh(&router, &[]).await.unwrap(); // justification: known-good declaration + live mock upstream
    let before = registry.list_tools().await;
    assert_eq!(before.len(), 1);
    let before_properties = before[0].input_schema.get("properties").unwrap(); // justification: the test-written schema above always includes a "properties" key
    assert!(
        before_properties["loud"].is_null(),
        "the pre-refresh schema must not yet have the `loud` property"
    );

    // Rewrite the upstream's advertised schema for the *same* already-spawned
    // subprocess: `RouteClient` lazily spawns once and reuses the handle, so
    // this exercises a live schema change, not a fresh process picking up a
    // new file.
    write_tools_file(
        &tools_file,
        &json!([
            {
                "name": "greet",
                "description": "says hello loudly",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "loud": { "type": "boolean" }
                    }
                }
            }
        ]),
    );

    registry.refresh(&router, &[]).await.unwrap(); // justification: known-good declaration + live mock upstream
    let after = registry.list_tools().await;
    assert_eq!(after.len(), 1);
    assert_eq!(
        after[0].description.as_deref(),
        Some("says hello loudly"),
        "refresh must pick up the upstream's new description"
    );
    let after_properties = after[0].input_schema.get("properties").unwrap(); // justification: the test-written schema above always includes a "properties" key
    assert!(
        !after_properties["loud"].is_null(),
        "refresh must pick up the upstream's new `loud` schema property"
    );
}

/// Boundary: declaring more tools than the registry's limit is a hard
/// failure (`RouterError::ExportLimitExceeded`), even though every
/// individual `list_tools`/lookup step succeeded.
#[tokio::test]
async fn refresh_over_limit_returns_export_limit_exceeded() {
    let dir = tempfile::tempdir().expect("tempdir"); // justification: tempdir creation mirrors crates/router/src/config.rs test pattern
    let tools_file = dir.path().join("tools.json");
    let tool_names: Vec<String> = (0..3).map(|i| format!("tool_{i}")).collect();
    let tools_json: Vec<serde_json::Value> = tool_names
        .iter()
        .map(|name| json!({ "name": name, "description": "" }))
        .collect();
    write_tools_file(&tools_file, &json!(tools_json));

    let router = McpRouter::from_configs(vec![sample_route("many", &tools_file)]);
    let registry = ExportRegistry::from_declarations(vec![ExportConfig {
        route: "many".to_string(),
        tools: tool_names,
        prefix: None,
    }])
    .with_max_exports(2);

    let result = registry.refresh(&router, &[]).await;
    assert!(matches!(
        result,
        Err(lds_router::RouterError::ExportLimitExceeded(3))
    ));
}

/// Boundary: two declarations that resolve to the same public tool name
/// (here: an explicit `prefix` collision) is a hard failure
/// (`RouterError::ExportCollision`).
#[tokio::test]
async fn refresh_prefix_collision_returns_export_collision() {
    let dir = tempfile::tempdir().expect("tempdir"); // justification: tempdir creation mirrors crates/router/src/config.rs test pattern
    let tools_file_a = dir.path().join("tools_a.json");
    let tools_file_b = dir.path().join("tools_b.json");
    write_tools_file(
        &tools_file_a,
        &json!([{ "name": "greet", "description": "a" }]),
    );
    write_tools_file(
        &tools_file_b,
        &json!([{ "name": "greet", "description": "b" }]),
    );

    let router = McpRouter::from_configs(vec![
        sample_route("route_a", &tools_file_a),
        sample_route("route_b", &tools_file_b),
    ]);
    let registry = ExportRegistry::from_declarations(vec![
        ExportConfig {
            route: "route_a".to_string(),
            tools: vec!["greet".to_string()],
            prefix: Some("shared_".to_string()),
        },
        ExportConfig {
            route: "route_b".to_string(),
            tools: vec!["greet".to_string()],
            prefix: Some("shared_".to_string()),
        },
    ]);

    let result = registry.refresh(&router, &[]).await;
    assert!(matches!(
        result,
        Err(lds_router::RouterError::ExportCollision(name)) if name == "shared_greet"
    ));
}
