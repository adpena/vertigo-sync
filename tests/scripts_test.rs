use std::collections::BTreeMap;
use vertigo_sync::scripts::resolve_script;

#[test]
fn resolves_defined_script() {
    let mut scripts = BTreeMap::new();
    scripts.insert("build".to_string(), "echo building".to_string());
    scripts.insert("test".to_string(), "echo testing".to_string());

    let result = resolve_script("build", &scripts);
    assert_eq!(result, Some("echo building".to_string()));
}

#[test]
fn returns_none_for_undefined_script() {
    let scripts = BTreeMap::new();
    let result = resolve_script("missing", &scripts);
    assert_eq!(result, None);
}
