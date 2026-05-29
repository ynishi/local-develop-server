use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use lds_core::log_store::HasId;
use lds_core::{Session, SessionConfig};
use lds_recipe::{RecipeModule, list_global_plugins};

fn make_session_with_justfile(dir: &std::path::Path) -> Arc<Session> {
    Arc::new(
        Session::new(SessionConfig {
            root: dir.to_path_buf(),
            timeout_secs: Some(10),
            max_output: None,
            global_recipe_dirs: Vec::new(),
        })
        .unwrap(),
    )
}

fn write_test_justfile(dir: &std::path::Path) {
    let content = r#"
[group('allow-agent')]
echo msg="hello":
    @echo "{{msg}}"

[group('allow-agent')]
fail:
    @exit 1

private_recipe:
    @echo "hidden"
"#;
    std::fs::write(dir.join("justfile"), content).unwrap();
}

#[tokio::test]
async fn list_filters_by_allow_agent() {
    let tmp = tempfile::tempdir().unwrap();
    write_test_justfile(tmp.path());
    let session = make_session_with_justfile(tmp.path());
    let recipe = RecipeModule::new(session);

    let list = recipe.list().await.unwrap();
    let names: Vec<&str> = list.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"echo"));
    assert!(names.contains(&"fail"));
    assert!(!names.contains(&"private_recipe"));
}

#[tokio::test]
async fn run_recipe_with_args() {
    let tmp = tempfile::tempdir().unwrap();
    write_test_justfile(tmp.path());
    let session = make_session_with_justfile(tmp.path());
    let recipe = RecipeModule::new(session);

    let output = recipe
        .run("echo", &["world"], &HashMap::new(), None)
        .await
        .unwrap();
    assert_eq!(output.exit_code, 0);
    assert!(output.stdout.contains("world"));
    assert!(!output.id.is_empty());
    assert!(output.started_at > 0);
}

#[tokio::test]
async fn run_recipe_with_content() {
    let tmp = tempfile::tempdir().unwrap();
    let content = r#"
[group('allow-agent')]
show_body:
    @echo "$TASK_MCP_CONTENT_BODY"
"#;
    std::fs::write(tmp.path().join("justfile"), content).unwrap();
    let session = make_session_with_justfile(tmp.path());
    let recipe = RecipeModule::new(session);

    let mut content_map = HashMap::new();
    content_map.insert("BODY".to_string(), "test content".to_string());
    let output = recipe
        .run("show_body", &[], &content_map, None)
        .await
        .unwrap();
    assert_eq!(output.exit_code, 0);
    assert!(output.stdout.contains("test content"));
}

#[tokio::test]
async fn content_key_validation_rejects_invalid() {
    let tmp = tempfile::tempdir().unwrap();
    write_test_justfile(tmp.path());
    let session = make_session_with_justfile(tmp.path());
    let recipe = RecipeModule::new(session);

    let mut bad_content = HashMap::new();
    bad_content.insert("bad-key".to_string(), "value".to_string());
    let err = recipe.run("echo", &[], &bad_content, None).await;
    assert!(err.is_err());
    assert!(err.unwrap_err().to_string().contains("invalid content key"));
}

#[tokio::test]
async fn arg_validation_rejects_newline() {
    let tmp = tempfile::tempdir().unwrap();
    write_test_justfile(tmp.path());
    let session = make_session_with_justfile(tmp.path());
    let recipe = RecipeModule::new(session);

    let err = recipe
        .run("echo", &["line1\nline2"], &HashMap::new(), None)
        .await;
    assert!(err.is_err());
    assert!(err.unwrap_err().to_string().contains("dangerous argument"));
}

#[tokio::test]
async fn logs_store_records_execution() {
    let tmp = tempfile::tempdir().unwrap();
    write_test_justfile(tmp.path());
    let session = make_session_with_justfile(tmp.path());
    let recipe = RecipeModule::new(session);

    let output = recipe
        .run("echo", &["test"], &HashMap::new(), None)
        .await
        .unwrap();

    let recent = recipe.logs().recent(10);
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].id(), output.id());

    let fetched = recipe.logs().get(output.id()).unwrap();
    assert_eq!(fetched.exit_code, 0);
    assert!(fetched.stdout.contains("test"));
}

#[tokio::test]
async fn list_plugins_filters_by_lds_plugin_group() {
    let tmp = tempfile::tempdir().unwrap();
    let content = r#"
# A plugin tool
[group('lds-plugin')]
my-plugin path=".":
    @echo "plugin {{path}}"

[group('allow-agent')]
my-task:
    @echo "task"

[group('lds-plugin')]
[group('allow-agent')]
mixed:
    @echo "mixed"

private_recipe:
    @echo "hidden"
"#;
    std::fs::write(tmp.path().join("justfile"), content).unwrap();
    let session = make_session_with_justfile(tmp.path());
    let recipe = RecipeModule::new(session);

    let plugins = recipe.list_plugins().await.unwrap();
    let names: Vec<&str> = plugins.iter().map(|p| p.name.as_str()).collect();
    assert!(names.contains(&"my-plugin"));
    assert!(names.contains(&"mixed"));
    assert!(!names.contains(&"my-task"));
    assert!(!names.contains(&"private_recipe"));

    let my_plugin = plugins.iter().find(|p| p.name == "my-plugin").unwrap();
    assert_eq!(my_plugin.description, "A plugin tool");
    assert_eq!(my_plugin.parameters.len(), 1);
    assert_eq!(my_plugin.parameters[0].name, "path");
}

#[tokio::test]
async fn failed_recipe_recorded_in_logs() {
    let tmp = tempfile::tempdir().unwrap();
    write_test_justfile(tmp.path());
    let session = make_session_with_justfile(tmp.path());
    let recipe = RecipeModule::new(session);

    let output = recipe
        .run("fail", &[], &HashMap::new(), None)
        .await
        .unwrap();
    assert_ne!(output.exit_code, 0);

    let recent = recipe.logs().recent(10);
    assert_eq!(recent.len(), 1);
    assert_ne!(recent[0].exit_code, 0);
}

// ── Precedence / multi-global-dir regression tests (crux §2) ──────────────

/// Write a justfile with a single allow-agent recipe whose body echoes `tag`.
/// Used to distinguish which dir's recipe won the name collision.
fn write_tagged_justfile(dir: &std::path::Path, recipe_name: &str, tag: &str) {
    let content = format!("[group('allow-agent')]\n{recipe_name}:\n    @echo \"{tag}\"\n");
    std::fs::write(dir.join("justfile"), content).unwrap();
}

/// Write a justfile with a single lds-plugin recipe that echoes `tag`.
fn write_tagged_plugin_justfile(dir: &std::path::Path, plugin_name: &str, tag: &str) {
    let content = format!("[group('lds-plugin')]\n{plugin_name}:\n    @echo \"{tag}\"\n");
    std::fs::write(dir.join("justfile"), content).unwrap();
}

/// `resolve_chain` preserves the order of `global_recipe_dirs` entries.
///
/// Given dirs [A, B], the chain should have A before B, and both before Project.
/// This verifies crux §2: Vec<PathBuf> insertion order is not collapsed.
#[tokio::test]
async fn resolve_chain_preserves_global_dirs_order() {
    let project = tempfile::tempdir().unwrap();
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();

    write_tagged_justfile(project.path(), "greet", "project");
    write_tagged_justfile(dir_a.path(), "greet", "dir_a");
    write_tagged_justfile(dir_b.path(), "greet", "dir_b");

    let session = Arc::new(
        Session::new(SessionConfig {
            root: project.path().to_path_buf(),
            timeout_secs: Some(10),
            max_output: None,
            global_recipe_dirs: vec![dir_a.path().to_path_buf(), dir_b.path().to_path_buf()],
        })
        .unwrap(),
    );
    let recipe = RecipeModule::new(session);
    let chain = recipe.resolve_chain();

    // Find indices of dir_a, dir_b, and project in the chain.
    let pos_a = chain
        .iter()
        .position(|(_, p)| p.starts_with(dir_a.path()))
        .expect("dir_a should appear in resolve chain");
    let pos_b = chain
        .iter()
        .position(|(_, p)| p.starts_with(dir_b.path()))
        .expect("dir_b should appear in resolve chain");
    let pos_project = chain
        .iter()
        .position(|(_, p)| p.starts_with(project.path()))
        .expect("project should appear in resolve chain");

    // Verify ordering: dir_a < dir_b < project (lower index = lower priority).
    assert!(
        pos_a < pos_b,
        "dir_a (index {pos_a}) must precede dir_b (index {pos_b})"
    );
    assert!(
        pos_b < pos_project,
        "dir_b (index {pos_b}) must precede project (index {pos_project})"
    );
}

/// Same-name recipe in a later global dir overrides the earlier one (post-wins).
///
/// Precedence order: default < dir_a < dir_b < project. The last entry in
/// the resolve chain that defines "greet" should win. Here dir_b is last among
/// globals, so its recipe should win over dir_a.
#[tokio::test]
async fn later_global_dir_overrides_earlier_on_collision() {
    let project = tempfile::tempdir().unwrap();
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();

    // dir_a has "greet" → "from_a"; dir_b has "greet" → "from_b"
    write_tagged_justfile(dir_a.path(), "greet", "from_a");
    write_tagged_justfile(dir_b.path(), "greet", "from_b");
    // project has a distinct recipe so the project justfile is included
    let project_content = "[group('allow-agent')]\nproject_only:\n    @echo \"project\"\n";
    std::fs::write(project.path().join("justfile"), project_content).unwrap();

    let session = Arc::new(
        Session::new(SessionConfig {
            root: project.path().to_path_buf(),
            timeout_secs: Some(10),
            max_output: None,
            global_recipe_dirs: vec![dir_a.path().to_path_buf(), dir_b.path().to_path_buf()],
        })
        .unwrap(),
    );
    let recipe = RecipeModule::new(session);
    let list = recipe.list().await.unwrap();

    // "greet" should come from dir_b (later entry wins over dir_a).
    let greet = list
        .iter()
        .find(|r| r.name == "greet")
        .expect("greet must be listed");
    assert!(
        greet.source.source_path.starts_with(dir_b.path()),
        "greet should be sourced from dir_b (last global), got {:?}",
        greet.source.source_path
    );
}

/// `list_global_plugins` scans dirs in order and later dir overrides on collision.
#[tokio::test]
async fn list_global_plugins_later_dir_overrides() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();

    write_tagged_plugin_justfile(dir_a.path(), "my-plugin", "from_a");
    write_tagged_plugin_justfile(dir_b.path(), "my-plugin", "from_b");

    let dirs: Vec<PathBuf> = vec![dir_a.path().to_path_buf(), dir_b.path().to_path_buf()];
    let plugins = list_global_plugins(&dirs).await.unwrap();

    // "my-plugin" must appear exactly once; it must come from dir_b (later wins).
    let matches: Vec<_> = plugins.iter().filter(|p| p.name == "my-plugin").collect();
    assert_eq!(matches.len(), 1, "my-plugin should appear exactly once");
    assert!(
        matches[0].source.source_path.starts_with(dir_b.path()),
        "my-plugin should be sourced from dir_b (last entry), got {:?}",
        matches[0].source.source_path
    );
}
