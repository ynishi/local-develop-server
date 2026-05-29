use std::collections::HashMap;
use std::sync::Arc;

use lds_core::log_store::HasId;
use lds_core::{Session, SessionConfig};
use lds_recipe::RecipeModule;

fn make_session_with_justfile(dir: &std::path::Path) -> Arc<Session> {
    Arc::new(
        Session::new(SessionConfig {
            root: dir.to_path_buf(),
            timeout_secs: Some(10),
            max_output: None,
            global_recipe_dir: None,
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
