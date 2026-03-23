use vertigo_sync::package::lockfile::{LockedPackage, Lockfile};

#[test]
fn lockfile_roundtrip() {
    let mut lf = Lockfile::new();
    lf.packages.push(LockedPackage {
        name: "roblox/roact".to_string(),
        version: "17.0.1".to_string(),
        realm: "shared".to_string(),
        checksum: "abc123def456".to_string(),
        source: "wally".to_string(),
        dependencies: vec!["roblox/react-lua@0.1.0".to_string()],
    });

    let serialized = lf.to_string();
    let parsed = Lockfile::parse(&serialized).expect("should parse roundtripped lockfile");

    assert_eq!(parsed.lockfile_version, 1);
    assert_eq!(parsed.packages.len(), 1);
    assert_eq!(parsed.packages[0].name, "roblox/roact");
    assert_eq!(parsed.packages[0].version, "17.0.1");
    assert_eq!(parsed.packages[0].realm, "shared");
    assert_eq!(parsed.packages[0].checksum, "abc123def456");
    assert_eq!(parsed.packages[0].source, "wally");
    assert_eq!(
        parsed.packages[0].dependencies,
        vec!["roblox/react-lua@0.1.0"]
    );
    assert_eq!(lf, parsed);
}

#[test]
fn lockfile_rejects_future_version() {
    let content = r#"
lockfile-version = 99

[[packages]]
name = "x/y"
version = "1.0.0"
realm = "shared"
checksum = "abc"
source = "wally"
"#;
    let err = Lockfile::parse(content).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("upgrade vsync"),
        "expected error to mention 'upgrade vsync', got: {msg}"
    );
}
