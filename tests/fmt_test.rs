//! Integration tests for `vertigo_sync::fmt`.

use vertigo_sync::config::FormatConfig;
use vertigo_sync::fmt;

#[test]
fn format_source_default_config() {
    let input = "local   x   =    1";
    let output = fmt::format_source(input, &FormatConfig::default()).unwrap();
    assert_eq!(output, "local x = 1\n");
}

#[test]
fn format_source_with_indent_spaces() {
    let input = "if true then\nx = 1\nend\n";
    let config = FormatConfig {
        indent_type: Some("spaces".into()),
        indent_width: Some(2),
        ..Default::default()
    };
    let output = fmt::format_source(input, &config).unwrap();
    assert!(
        output.contains("  x = 1"),
        "expected 2-space indent, got: {output}"
    );
}

#[test]
fn format_source_with_tabs() {
    let input = "if true then\nx = 1\nend\n";
    let config = FormatConfig {
        indent_type: Some("tabs".into()),
        ..Default::default()
    };
    let output = fmt::format_source(input, &config).unwrap();
    assert!(
        output.contains("\tx = 1"),
        "expected tab indent, got: {output}"
    );
}

#[test]
fn check_source_detects_unformatted() {
    let messy = "local    x=    1";
    assert!(fmt::check_source(messy, &FormatConfig::default()).unwrap());
}

#[test]
fn check_source_returns_false_when_clean() {
    let clean = "local x = 1\n";
    assert!(!fmt::check_source(clean, &FormatConfig::default()).unwrap());
}

#[test]
fn format_file_writes_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.luau");
    std::fs::write(&path, "local   y   =    2").unwrap();

    let changed = fmt::format_file(&path, &FormatConfig::default()).unwrap();
    assert!(changed);

    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(content, "local y = 2\n");
}

#[test]
fn format_file_noop_when_already_formatted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clean.luau");
    std::fs::write(&path, "local y = 2\n").unwrap();

    let changed = fmt::format_file(&path, &FormatConfig::default()).unwrap();
    assert!(!changed);
}

#[test]
fn collect_lua_files_finds_luau_and_lua() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.luau"), "-- a").unwrap();
    std::fs::write(root.join("b.lua"), "-- b").unwrap();
    std::fs::write(root.join("c.txt"), "not lua").unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/d.luau"), "-- d").unwrap();

    let files = fmt::collect_lua_files(root).unwrap();
    assert_eq!(files.len(), 3, "expected 3 lua files, got: {files:?}");
}

#[test]
fn collect_lua_files_skips_ignored_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("node_modules")).unwrap();
    std::fs::write(root.join("node_modules/x.luau"), "-- skip").unwrap();
    std::fs::write(root.join("keep.luau"), "-- keep").unwrap();

    let files = fmt::collect_lua_files(root).unwrap();
    assert_eq!(files.len(), 1);
    assert!(files[0].ends_with("keep.luau"));
}

#[test]
fn format_source_quote_style_single() {
    // StyLua should convert double quotes to single when ForceSingle is set.
    let input = "local s = \"hello\"\n";
    let config = FormatConfig {
        quote_style: Some("single".into()),
        ..Default::default()
    };
    let output = fmt::format_source(input, &config).unwrap();
    assert!(
        output.contains("'hello'"),
        "expected single quotes, got: {output}"
    );
}

#[test]
fn format_source_handles_luau_types() {
    // Ensure Luau-specific syntax (type annotations) parses and formats.
    let input = "local   x  :   number  =   1";
    let output = fmt::format_source(input, &FormatConfig::default()).unwrap();
    assert!(output.contains("local x: number = 1"));
}
