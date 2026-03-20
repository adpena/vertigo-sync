use vertigo_sync::package::registry::{IndexEntry, parse_version_req};

#[test]
fn parse_version_req_valid() {
    let (scope, name, ver) = parse_version_req("roblox/roact@^17.0.0").unwrap();
    assert_eq!(scope, "roblox");
    assert_eq!(name, "roact");
    assert_eq!(ver, "^17.0.0");
}

#[test]
fn parse_version_req_missing_at() {
    let result = parse_version_req("roblox/roact");
    assert!(result.is_err(), "should fail when '@' is missing");
}

#[test]
fn parse_index_entry() {
    let json = r#"{
        "name": "roblox/roact",
        "version": "17.0.1",
        "realm": "shared",
        "dependencies": { "roblox/react-lua": "^0.1.0" },
        "server-dependencies": {},
        "description": "A declarative UI library for Roblox"
    }"#;
    let entry: IndexEntry = serde_json::from_str(json).unwrap();
    assert_eq!(entry.name, "roblox/roact");
    assert_eq!(entry.version, "17.0.1");
    assert_eq!(entry.realm, "shared");
    assert_eq!(entry.description, "A declarative UI library for Roblox");
    assert_eq!(entry.dependencies.get("roblox/react-lua").unwrap(), "^0.1.0");
}
