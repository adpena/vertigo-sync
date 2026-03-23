use std::fs;
use tempfile::TempDir;

#[test]
fn init_creates_vsync_toml() {
    let tmp = TempDir::new().unwrap();
    vertigo_sync::init::run_init(tmp.path(), Some("test-project")).unwrap();

    let content = fs::read_to_string(tmp.path().join("vsync.toml")).unwrap();
    assert!(content.contains("[package]"));
    assert!(content.contains("[lint]"));
    assert!(content.contains("[format]"));
    // Verify inline comments are present for DX
    assert!(
        content.contains("# vsync.toml"),
        "should have header comment"
    );
    assert!(
        content.contains("# Documentation:"),
        "should have docs link"
    );
    assert!(
        content.contains("name = \"test-project\""),
        "should contain project name"
    );
}

#[test]
fn init_creates_gitignore() {
    let tmp = TempDir::new().unwrap();
    vertigo_sync::init::run_init(tmp.path(), Some("test-project")).unwrap();

    let content = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
    assert!(content.contains("Packages/"));
    assert!(content.contains("*.rbxl"));
}

#[test]
fn init_creates_vscode_settings() {
    let tmp = TempDir::new().unwrap();
    vertigo_sync::init::run_init(tmp.path(), Some("test-project")).unwrap();

    let content = fs::read_to_string(tmp.path().join(".vscode/settings.json")).unwrap();
    assert!(content.contains("luau-lsp"));
}

#[test]
fn init_creates_ci_workflow() {
    let tmp = TempDir::new().unwrap();
    vertigo_sync::init::run_init(tmp.path(), Some("test-project")).unwrap();

    let content = fs::read_to_string(tmp.path().join(".github/workflows/ci.yml")).unwrap();
    assert!(content.contains("vsync validate"));
}

#[test]
fn init_creates_tests_directory() {
    let tmp = TempDir::new().unwrap();
    vertigo_sync::init::run_init(tmp.path(), Some("test-project")).unwrap();

    assert!(tmp.path().join("tests/init.luau").exists());
}

#[test]
fn init_skips_existing_files() {
    let tmp = TempDir::new().unwrap();
    let toml_path = tmp.path().join("vsync.toml");
    fs::write(&toml_path, "# my custom config\n").unwrap();

    vertigo_sync::init::run_init(tmp.path(), Some("test-project")).unwrap();

    let content = fs::read_to_string(&toml_path).unwrap();
    assert_eq!(
        content, "# my custom config\n",
        "vsync.toml should not be overwritten"
    );
}

// ---------------------------------------------------------------------------
// Template tests
// ---------------------------------------------------------------------------

#[test]
fn library_template_creates_changelog() {
    let tmp = TempDir::new().unwrap();
    vertigo_sync::init::run_init(tmp.path(), Some("my-lib")).unwrap();
    vertigo_sync::init::apply_library_template(tmp.path(), "my-lib").unwrap();

    assert!(
        tmp.path().join("CHANGELOG.md").exists(),
        "library template should create CHANGELOG.md"
    );
    let changelog = fs::read_to_string(tmp.path().join("CHANGELOG.md")).unwrap();
    assert!(changelog.contains("my-lib"));
}

#[test]
fn library_template_creates_release_workflow() {
    let tmp = TempDir::new().unwrap();
    vertigo_sync::init::run_init(tmp.path(), Some("my-lib")).unwrap();
    vertigo_sync::init::apply_library_template(tmp.path(), "my-lib").unwrap();

    let release_path = tmp.path().join(".github/workflows/release.yml");
    assert!(
        release_path.exists(),
        "library template should create release.yml"
    );
    let content = fs::read_to_string(&release_path).unwrap();
    assert!(content.contains("vsync publish"));
}

#[test]
fn library_template_sets_shared_realm() {
    let tmp = TempDir::new().unwrap();
    vertigo_sync::init::run_init(tmp.path(), Some("my-lib")).unwrap();
    vertigo_sync::init::apply_library_template(tmp.path(), "my-lib").unwrap();

    let toml = fs::read_to_string(tmp.path().join("vsync.toml")).unwrap();
    assert!(
        toml.contains("realm = \"shared\""),
        "library template should set realm to shared"
    );
}

#[test]
fn plugin_template_creates_plugin_entry() {
    let tmp = TempDir::new().unwrap();
    vertigo_sync::init::run_init(tmp.path(), Some("my-plugin")).unwrap();
    vertigo_sync::init::apply_plugin_template(tmp.path(), "my-plugin").unwrap();

    let plugin_path = tmp.path().join("src/Plugin/init.server.luau");
    assert!(
        plugin_path.exists(),
        "plugin template should create src/Plugin/init.server.luau"
    );
    let content = fs::read_to_string(&plugin_path).unwrap();
    assert!(content.contains("CreateToolbar"));
    assert!(content.contains("my-plugin"));
}

#[test]
fn plugin_template_sets_server_realm() {
    let tmp = TempDir::new().unwrap();
    vertigo_sync::init::run_init(tmp.path(), Some("my-plugin")).unwrap();
    vertigo_sync::init::apply_plugin_template(tmp.path(), "my-plugin").unwrap();

    let toml = fs::read_to_string(tmp.path().join("vsync.toml")).unwrap();
    assert!(
        toml.contains("realm = \"server\""),
        "plugin template should set realm to server"
    );
}

#[test]
fn plugin_template_updates_project_json() {
    let tmp = TempDir::new().unwrap();
    vertigo_sync::init::run_init(tmp.path(), Some("my-plugin")).unwrap();
    vertigo_sync::init::apply_plugin_template(tmp.path(), "my-plugin").unwrap();

    let project = fs::read_to_string(tmp.path().join("default.project.json")).unwrap();
    assert!(
        project.contains("ServerStorage"),
        "plugin template project should target ServerStorage"
    );
    assert!(
        project.contains("src/Plugin"),
        "plugin template project should reference src/Plugin"
    );
}
