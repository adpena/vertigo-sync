use vertigo_sync::config::{DependencySpec, VsyncConfig};

#[test]
fn parse_minimal_config() {
    let input = r#"
[package]
name = "my-game"
version = "1.0.0"
realm = "shared"
"#;

    let config: VsyncConfig = toml::from_str(input).expect("failed to parse minimal config");
    assert_eq!(config.package.name, "my-game");
    assert_eq!(config.package.version, "1.0.0");
    assert_eq!(config.package.realm, "shared");
    assert!(config.dependencies.is_empty());
    assert!(config.scripts.is_empty());
}

#[test]
fn parse_full_config() {
    let input = r#"
[package]
name = "my-game"
version = "2.0.0"
realm = "server"
description = "A game"
license = "MIT"
authors = ["Alice", "Bob"]
packages-dir = "Packages"

[registries]
default = "https://registry.example.com"

[dependencies]
roact = "roblox/roact@^17.0.0"

[server-dependencies]
data-store = "roblox/data-store@^1.0.0"

[dev-dependencies]
test-ez = "roblox/test-ez@^0.4.0"

[peer-dependencies]
react = "roblox/react@^18.0.0"

[lint]
unused-variable = "warn"

[format]
indent-type = "tabs"
indent-width = 4
line-width = 120
quote-style = "double"
call-parentheses = "always"
collapse-simple-statement = "never"

[scripts]
build = "vsync build"
test = "vsync test"

[workspace]
members = ["packages/core", "packages/ui"]
"#;

    let config: VsyncConfig = toml::from_str(input).expect("failed to parse full config");

    assert_eq!(config.package.name, "my-game");
    assert_eq!(config.package.version, "2.0.0");
    assert_eq!(config.package.authors.len(), 2);
    assert_eq!(config.package.packages_dir.as_deref(), Some("Packages"));

    assert_eq!(config.registries.len(), 1);
    assert_eq!(config.dependencies.len(), 1);
    assert_eq!(config.server_dependencies.len(), 1);
    assert_eq!(config.dev_dependencies.len(), 1);
    assert_eq!(config.peer_dependencies.len(), 1);
    assert_eq!(config.lint.len(), 1);
    assert_eq!(config.scripts.len(), 2);
    assert_eq!(config.workspace.members.len(), 2);

    assert_eq!(config.format.indent_type.as_deref(), Some("tabs"));
    assert_eq!(config.format.indent_width, Some(4));
    assert_eq!(config.format.line_width, Some(120));
    assert_eq!(config.format.quote_style.as_deref(), Some("double"));
    assert_eq!(config.format.call_parentheses.as_deref(), Some("always"));
    assert_eq!(
        config.format.collapse_simple_statement.as_deref(),
        Some("never")
    );
}

#[test]
fn empty_config_uses_defaults() {
    let config: VsyncConfig = toml::from_str("").expect("failed to parse empty config");
    assert_eq!(config, VsyncConfig::default());
}

#[test]
fn dependency_spec_variants() {
    // Simple string — parsed via a wrapper table since bare TOML values need a key.
    #[derive(serde::Deserialize)]
    struct Wrap {
        dep: DependencySpec,
    }
    let w: Wrap = toml::from_str(r#"dep = "roblox/roact@^17.0.0""#).expect("simple dep");
    assert_eq!(w.dep, DependencySpec::Simple("roblox/roact@^17.0.0".into()));

    // Git with rev — use a [dependencies] table so the inline table is valid TOML.
    let git_toml = r#"
[dependencies]
my-lib = { git = "https://github.com/example/repo.git", rev = "abc123" }
"#;
    let cfg: VsyncConfig = toml::from_str(git_toml).expect("git dep");
    match cfg.dependencies.get("my-lib").unwrap() {
        DependencySpec::Git { git, rev, branch, tag } => {
            assert_eq!(git, "https://github.com/example/repo.git");
            assert_eq!(rev.as_deref(), Some("abc123"));
            assert!(branch.is_none());
            assert!(tag.is_none());
        }
        other => panic!("expected Git variant, got {:?}", other),
    }

    // Path
    let path_toml = r#"
[dependencies]
shared = { path = "../libs/shared" }
"#;
    let cfg: VsyncConfig = toml::from_str(path_toml).expect("path dep");
    match cfg.dependencies.get("shared").unwrap() {
        DependencySpec::Path { path } => assert_eq!(path, "../libs/shared"),
        other => panic!("expected Path variant, got {:?}", other),
    }

    // Registry
    let reg_toml = r#"
[dependencies]
my-lib = { registry = "custom", name = "my-lib" }
"#;
    let cfg: VsyncConfig = toml::from_str(reg_toml).expect("registry dep");
    match cfg.dependencies.get("my-lib").unwrap() {
        DependencySpec::Registry { registry, name } => {
            assert_eq!(registry, "custom");
            assert_eq!(name, "my-lib");
        }
        other => panic!("expected Registry variant, got {:?}", other),
    }
}
