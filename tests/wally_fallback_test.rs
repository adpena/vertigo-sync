use std::fs;
use tempfile::tempdir;
use vertigo_sync::config::{DependencySpec, VsyncConfig, load_config_with_fallback};

#[test]
fn falls_back_to_wally_toml_when_no_vsync_toml() {
    let dir = tempdir().unwrap();
    let wally = r#"
[package]
name = "acme/widget"
version = "1.0.0"
realm = "shared"
description = "A cool widget"
license = "MIT"
authors = ["Alice"]

[dependencies]
Roact = "roblox/roact@^17.0.0"

[server-dependencies]
DataStoreService = "roblox/data-store@^1.0.0"

[dev-dependencies]
TestEZ = "roblox/testez@^0.4.0"
"#;
    fs::write(dir.path().join("wally.toml"), wally).unwrap();

    let config = load_config_with_fallback(dir.path()).unwrap();

    assert_eq!(config.package.name, "acme/widget");
    assert_eq!(config.package.version, "1.0.0");
    assert_eq!(config.package.realm, "shared");
    assert_eq!(config.package.description, "A cool widget");
    assert_eq!(config.package.license, "MIT");
    assert_eq!(config.package.authors, vec!["Alice".to_string()]);

    assert_eq!(
        config.dependencies.get("Roact"),
        Some(&DependencySpec::Simple("roblox/roact@^17.0.0".to_string()))
    );
    assert_eq!(
        config.server_dependencies.get("DataStoreService"),
        Some(&DependencySpec::Simple("roblox/data-store@^1.0.0".to_string()))
    );
    assert_eq!(
        config.dev_dependencies.get("TestEZ"),
        Some(&DependencySpec::Simple("roblox/testez@^0.4.0".to_string()))
    );
}

#[test]
fn vsync_toml_takes_precedence_over_wally_toml() {
    let dir = tempdir().unwrap();

    let vsync = r#"
[package]
name = "vsync-project"
version = "2.0.0"
"#;
    fs::write(dir.path().join("vsync.toml"), vsync).unwrap();

    let wally = r#"
[package]
name = "wally-project"
version = "1.0.0"
"#;
    fs::write(dir.path().join("wally.toml"), wally).unwrap();

    let config = load_config_with_fallback(dir.path()).unwrap();

    assert_eq!(config.package.name, "vsync-project");
    assert_eq!(config.package.version, "2.0.0");
}

#[test]
fn no_config_files_returns_default() {
    let dir = tempdir().unwrap();

    let config = load_config_with_fallback(dir.path()).unwrap();

    assert_eq!(config, VsyncConfig::default());
}
