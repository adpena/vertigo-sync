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
    assert!(!wait_issues.is_empty(), "expected wait-deprecated issue");
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
    let empty: Vec<_> = issues.iter().filter(|i| i.rule == "empty-block").collect();
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
    assert!(!unreachable.is_empty(), "expected unreachable-code issue");
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

// ---------------------------------------------------------------------------
// function-length tests
// ---------------------------------------------------------------------------

#[test]
fn detects_long_function() {
    // Build a function with 15 lines (above our test threshold of 10)
    let mut lines = vec!["local function longFunc()".to_string()];
    for i in 0..13 {
        lines.push(format!("    local x{i} = {i}"));
    }
    lines.push("end".to_string());
    let source = lines.join("\n");

    let comment_map = vertigo_sync::lint::rules::build_comment_map(&source);
    let issues = vertigo_sync::lint::rules::check_function_length_with_threshold(
        &source,
        "test.lua",
        &comment_map,
        10,
    );
    assert!(
        !issues.is_empty(),
        "expected function-length issue for a 15-line function with threshold 10"
    );
    assert!(issues[0].message.contains("longFunc"));
}

#[test]
fn no_function_length_for_short_function() {
    let source = r#"
local function short()
    return 1
end
"#;
    let comment_map = vertigo_sync::lint::rules::build_comment_map(source);
    let issues = vertigo_sync::lint::rules::check_function_length_with_threshold(
        source,
        "test.lua",
        &comment_map,
        100,
    );
    assert!(
        issues.is_empty(),
        "short function should not trigger function-length"
    );
}

// ---------------------------------------------------------------------------
// nesting-depth tests
// ---------------------------------------------------------------------------

#[test]
fn detects_deep_nesting() {
    let source = r#"
local function deep()
    if true then
        if true then
            if true then
                if true then
                    print("deep")
                end
            end
        end
    end
end
"#;
    let comment_map = vertigo_sync::lint::rules::build_comment_map(source);
    let issues = vertigo_sync::lint::rules::check_nesting_depth_with_threshold(
        source,
        "test.lua",
        &comment_map,
        3,
    );
    assert!(
        !issues.is_empty(),
        "expected nesting-depth issue for 4 levels with threshold 3"
    );
    assert!(issues[0].message.contains("deep"));
}

#[test]
fn no_nesting_depth_for_shallow_function() {
    let source = r#"
local function shallow()
    if true then
        print("ok")
    end
end
"#;
    let comment_map = vertigo_sync::lint::rules::build_comment_map(source);
    let issues = vertigo_sync::lint::rules::check_nesting_depth_with_threshold(
        source,
        "test.lua",
        &comment_map,
        5,
    );
    assert!(
        issues.is_empty(),
        "shallow function should not trigger nesting-depth"
    );
}

// ---------------------------------------------------------------------------
// cyclomatic-complexity tests
// ---------------------------------------------------------------------------

#[test]
fn detects_high_cyclomatic_complexity() {
    let source = r#"
local function complex(x)
    if x == 1 then
        return 1
    elseif x == 2 then
        return 2
    elseif x == 3 then
        return 3
    elseif x == 4 then
        return 4
    elseif x == 5 then
        return 5
    elseif x == 6 and x > 0 then
        return 6
    elseif x == 7 or x == 8 then
        return 7
    elseif x == 9 then
        return 9
    elseif x == 10 then
        return 10
    elseif x == 11 then
        return 11
    end
end
"#;
    let comment_map = vertigo_sync::lint::rules::build_comment_map(source);
    let issues = vertigo_sync::lint::rules::check_cyclomatic_complexity_with_threshold(
        source,
        "test.lua",
        &comment_map,
        5,
    );
    assert!(
        !issues.is_empty(),
        "expected cyclomatic-complexity issue for highly branched function"
    );
    assert!(issues[0].message.contains("complex"));
}

#[test]
fn no_complexity_for_simple_function() {
    let source = r#"
local function simple(x)
    if x then
        return 1
    end
    return 0
end
"#;
    let comment_map = vertigo_sync::lint::rules::build_comment_map(source);
    let issues = vertigo_sync::lint::rules::check_cyclomatic_complexity_with_threshold(
        source,
        "test.lua",
        &comment_map,
        10,
    );
    assert!(
        issues.is_empty(),
        "simple function should not trigger cyclomatic-complexity"
    );
}

// ---------------------------------------------------------------------------
// parentheses-condition tests
// ---------------------------------------------------------------------------

#[test]
fn detects_unnecessary_parentheses_in_if() {
    let source = r#"
if (x > 0) then
    print("positive")
end
"#;
    let issues = lint_source(source, "test.lua", &BTreeMap::new());
    let parens: Vec<_> = issues
        .iter()
        .filter(|i| i.rule == "parentheses-condition")
        .collect();
    assert!(
        !parens.is_empty(),
        "expected parentheses-condition issue for `if (x > 0) then`"
    );
}

#[test]
fn no_false_positive_parentheses_for_function_call() {
    let source = r#"
if foo(x) then
    print("ok")
end
"#;
    let issues = lint_source(source, "test.lua", &BTreeMap::new());
    let parens: Vec<_> = issues
        .iter()
        .filter(|i| i.rule == "parentheses-condition")
        .collect();
    assert!(
        parens.is_empty(),
        "function call in condition should not trigger parentheses-condition, got: {parens:?}"
    );
}

#[test]
fn detects_unnecessary_parentheses_in_while() {
    let source = r#"
while (running) do
    step()
end
"#;
    let issues = lint_source(source, "test.lua", &BTreeMap::new());
    let parens: Vec<_> = issues
        .iter()
        .filter(|i| i.rule == "parentheses-condition")
        .collect();
    assert!(
        !parens.is_empty(),
        "expected parentheses-condition issue for `while (running) do`"
    );
}

// ---------------------------------------------------------------------------
// comparison-order (Yoda condition) tests
// ---------------------------------------------------------------------------

#[test]
fn detects_yoda_condition_nil() {
    let source = r#"
if nil == x then
    print("nil")
end
"#;
    let issues = lint_source(source, "test.lua", &BTreeMap::new());
    let yoda: Vec<_> = issues
        .iter()
        .filter(|i| i.rule == "comparison-order")
        .collect();
    assert!(
        !yoda.is_empty(),
        "expected comparison-order issue for `nil == x`"
    );
}

#[test]
fn detects_yoda_condition_true() {
    let source = r#"
if true == enabled then
    start()
end
"#;
    let issues = lint_source(source, "test.lua", &BTreeMap::new());
    let yoda: Vec<_> = issues
        .iter()
        .filter(|i| i.rule == "comparison-order")
        .collect();
    assert!(
        !yoda.is_empty(),
        "expected comparison-order issue for `true == enabled`"
    );
}

#[test]
fn no_yoda_for_normal_comparison() {
    let source = r#"
if x == nil then
    print("nil")
end
"#;
    let issues = lint_source(source, "test.lua", &BTreeMap::new());
    let yoda: Vec<_> = issues
        .iter()
        .filter(|i| i.rule == "comparison-order")
        .collect();
    assert!(
        yoda.is_empty(),
        "normal comparison order should not trigger comparison-order"
    );
}

#[test]
fn detects_yoda_with_not_equal() {
    let source = r#"
if false ~= flag then
    toggle()
end
"#;
    let issues = lint_source(source, "test.lua", &BTreeMap::new());
    let yoda: Vec<_> = issues
        .iter()
        .filter(|i| i.rule == "comparison-order")
        .collect();
    assert!(
        !yoda.is_empty(),
        "expected comparison-order issue for `false ~= flag`"
    );
}
