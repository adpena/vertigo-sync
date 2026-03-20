use std::fs;
use tempfile::TempDir;
use vertigo_sync::config::{load_config, load_config_with_fallback};

#[test]
fn load_config_returns_none_when_missing() {
    let dir = TempDir::new().unwrap();
    let result = load_config(dir.path()).unwrap();
    assert!(result.is_none());
}

#[test]
fn load_config_parses_existing_file() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("vsync.toml"),
        r#"
[package]
name = "test/project"
version = "0.1.0"
realm = "shared"

[lint]
unused-variable = "warn"
"#,
    )
    .unwrap();
    let result = load_config(dir.path()).unwrap();
    assert!(result.is_some());
    let config = result.unwrap();
    assert_eq!(config.package.name, "test/project");
}

#[test]
fn load_config_returns_error_on_invalid_toml() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("vsync.toml"), "not valid [[[toml").unwrap();
    assert!(load_config(dir.path()).is_err());
}

#[test]
fn fallback_returns_default_when_no_config() {
    let dir = TempDir::new().unwrap();
    let config = load_config_with_fallback(dir.path()).unwrap();
    assert!(config.dependencies.is_empty());
}
