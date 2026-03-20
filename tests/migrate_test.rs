use std::fs;
use tempfile::TempDir;

#[test]
fn migrate_converts_wally_toml() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("wally.toml"),
        r#"
[package]
name = "acme/widget"
version = "1.2.3"
realm = "shared"
description = "A cool widget"
license = "MIT"
authors = ["Alice"]

[dependencies]
roact = "roblox/roact@^17.0.0"

[server-dependencies]
datastore = "acme/datastore@^1.0.0"
"#,
    )
    .unwrap();

    let report = vertigo_sync::migrate::run_migrate(tmp.path()).unwrap();

    assert!(report.wally_migrated);
    let content = fs::read_to_string(tmp.path().join("vsync.toml")).unwrap();
    assert!(content.contains("acme/widget"), "should contain package name");
    assert!(content.contains("roact"), "should contain dependency");
    assert!(content.contains("datastore"), "should contain server dependency");
}

#[test]
fn migrate_converts_stylua_toml() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("stylua.toml"),
        r#"
indent_type = "Tabs"
indent_width = 4
column_width = 100
quote_style = "ForceSingle"
"#,
    )
    .unwrap();

    let report = vertigo_sync::migrate::run_migrate(tmp.path()).unwrap();

    assert!(report.stylua_migrated);
    let content = fs::read_to_string(tmp.path().join("vsync.toml")).unwrap();
    assert!(content.contains("tabs"), "indent_type should be mapped to 'tabs'");
    assert!(content.contains("100"), "column_width should map to line-width");
    assert!(content.contains("single"), "quote_style should be mapped to 'single'");
}

#[test]
fn migrate_converts_selene_toml() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("selene.toml"),
        r#"
std = "roblox"

[lints]
unused_variable = "warn"
"#,
    )
    .unwrap();

    let report = vertigo_sync::migrate::run_migrate(tmp.path()).unwrap();

    assert!(report.selene_migrated);
    let content = fs::read_to_string(tmp.path().join("vsync.toml")).unwrap();
    assert!(content.contains("[lint]"), "should have lint section");
    assert!(content.contains("roblox"), "should carry over std hint");
}

#[test]
fn migrate_skips_if_vsync_toml_exists() {
    let tmp = TempDir::new().unwrap();
    let vsync_path = tmp.path().join("vsync.toml");
    fs::write(&vsync_path, "# my custom config\n").unwrap();

    // Also write a wally.toml to prove it's not consumed.
    fs::write(
        tmp.path().join("wally.toml"),
        r#"
[package]
name = "acme/widget"
version = "1.0.0"
"#,
    )
    .unwrap();

    let report = vertigo_sync::migrate::run_migrate(tmp.path()).unwrap();

    assert!(!report.wally_migrated);
    assert!(!report.selene_migrated);
    assert!(!report.stylua_migrated);

    let content = fs::read_to_string(&vsync_path).unwrap();
    assert_eq!(content, "# my custom config\n", "vsync.toml should not be overwritten");
}
