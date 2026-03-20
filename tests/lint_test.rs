use std::collections::BTreeMap;
use vertigo_sync::lint::{LintSeverity, lint_source};

#[test]
fn detects_unused_variable() {
    let source = r#"
local foo = 42
print("hello")
"#;
    let issues = lint_source(source, "test.lua", &BTreeMap::new());
    let unused: Vec<_> = issues
        .iter()
        .filter(|i| i.rule == "unused-variable")
        .collect();
    assert!(
        !unused.is_empty(),
        "expected unused-variable issue for `foo`"
    );
    assert!(unused[0].message.contains("foo"));
}

#[test]
fn no_false_positive_on_used_variable() {
    let source = r#"
local foo = 42
print(foo)
"#;
    let issues = lint_source(source, "test.lua", &BTreeMap::new());
    let unused: Vec<_> = issues
        .iter()
        .filter(|i| i.rule == "unused-variable")
        .collect();
    assert!(
        unused.is_empty(),
        "expected no unused-variable issue, but got: {unused:?}"
    );
}

#[test]
fn detects_global_shadow() {
    let source = r#"
local game = {}
print(game)
"#;
    let issues = lint_source(source, "test.lua", &BTreeMap::new());
    let shadow: Vec<_> = issues
        .iter()
        .filter(|i| i.rule == "global-shadow")
        .collect();
    assert!(
        !shadow.is_empty(),
        "expected global-shadow issue for `game`"
    );
    assert!(shadow[0].message.contains("game"));
}

#[test]
fn detects_deprecated_wait() {
    let source = r#"
wait(1)
"#;
    let issues = lint_source(source, "test.lua", &BTreeMap::new());
    let wait_issues: Vec<_> = issues
        .iter()
        .filter(|i| i.rule == "wait-deprecated")
        .collect();
    assert!(
        !wait_issues.is_empty(),
        "expected wait-deprecated issue"
    );
}

#[test]
fn respects_rule_config_off() {
    let source = r#"
local foo = 42
print("hello")
"#;
    let mut config = BTreeMap::new();
    config.insert("unused-variable".to_string(), "off".to_string());

    let issues = lint_source(source, "test.lua", &config);
    let unused: Vec<_> = issues
        .iter()
        .filter(|i| i.rule == "unused-variable")
        .collect();
    assert!(
        unused.is_empty(),
        "expected no unused-variable issue when rule is off, but got: {unused:?}"
    );
}

#[test]
fn respects_severity_escalation_to_error() {
    let source = r#"
local foo = 42
print("hello")
"#;
    let mut config = BTreeMap::new();
    config.insert("unused-variable".to_string(), "error".to_string());

    let issues = lint_source(source, "test.lua", &config);
    let unused: Vec<_> = issues
        .iter()
        .filter(|i| i.rule == "unused-variable")
        .collect();
    assert!(!unused.is_empty());
    assert_eq!(unused[0].severity, LintSeverity::Error);
}

#[test]
fn detects_empty_block() {
    let source = "if true then\nend\n";
    let issues = lint_source(source, "test.lua", &BTreeMap::new());
    let empty: Vec<_> = issues
        .iter()
        .filter(|i| i.rule == "empty-block")
        .collect();
    assert!(!empty.is_empty(), "expected empty-block issue");
}

#[test]
fn detects_unreachable_code() {
    let source = r#"
local function foo()
    return 1
    print("unreachable")
end
"#;
    let issues = lint_source(source, "test.lua", &BTreeMap::new());
    let unreachable: Vec<_> = issues
        .iter()
        .filter(|i| i.rule == "unreachable-code")
        .collect();
    assert!(
        !unreachable.is_empty(),
        "expected unreachable-code issue"
    );
}

#[test]
fn no_unreachable_before_end() {
    let source = r#"
local function foo()
    return 1
end
"#;
    let issues = lint_source(source, "test.lua", &BTreeMap::new());
    let unreachable: Vec<_> = issues
        .iter()
        .filter(|i| i.rule == "unreachable-code")
        .collect();
    assert!(
        unreachable.is_empty(),
        "return followed by `end` should not be flagged"
    );
}

#[test]
fn skips_underscore_prefixed_variables() {
    let source = r#"
local _unused = 42
print("hello")
"#;
    let issues = lint_source(source, "test.lua", &BTreeMap::new());
    let unused: Vec<_> = issues
        .iter()
        .filter(|i| i.rule == "unused-variable")
        .collect();
    assert!(
        unused.is_empty(),
        "underscore-prefixed variables should be ignored"
    );
}
