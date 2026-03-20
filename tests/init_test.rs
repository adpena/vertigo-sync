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
    assert!(content.contains("# vsync.toml"), "should have header comment");
    assert!(content.contains("# Documentation:"), "should have docs link");
    assert!(content.contains("name = \"test-project\""), "should contain project name");
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
    assert_eq!(content, "# my custom config\n", "vsync.toml should not be overwritten");
}
