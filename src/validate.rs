//! Luau source validation module.
//!
//! Runs built-in lint checks on `.luau` files without requiring external tools.
//! Optionally shells out to `selene` if available on PATH.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidationIssue {
    pub path: String,
    pub line: usize,
    pub severity: String,
    pub message: String,
    pub rule: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidationReport {
    pub files_checked: usize,
    pub errors: usize,
    pub warnings: usize,
    pub issues: Vec<ValidationIssue>,
    pub clean: bool,
}

// ---------------------------------------------------------------------------
// Rule names
// ---------------------------------------------------------------------------

const RULE_STRICT_MODE: &str = "strict-mode";
const RULE_CROSS_BOUNDARY_REQUIRE: &str = "cross-boundary-require";
const RULE_DEPRECATED_API: &str = "deprecated-api";
const RULE_LARGE_FILE: &str = "large-file";
const RULE_TAB_INDENT: &str = "tab-indent";
const RULE_INSTANCE_NEW_HOT_PATH: &str = "instance-new-hot-path";
const RULE_NCG_UNTYPED_PARAM: &str = "ncg-untyped-param";
const RULE_NCG_CLOSURE_IN_LOOP: &str = "ncg-closure-in-loop";
const RULE_NCG_PATTERN_IN_HOT_PATH: &str = "ncg-pattern-in-hot-path";
const RULE_PERF_DYNAMIC_ARRAY: &str = "perf-dynamic-array";
const RULE_PERF_UNFROZEN_CONSTANT: &str = "perf-unfrozen-constant";
const RULE_PERF_MISSING_NATIVE: &str = "perf-missing-native";
const RULE_PERF_PCALL_IN_NATIVE: &str = "perf-pcall-in-native";

/// Large file threshold (lines).
const LARGE_FILE_LINES: usize = 500;

/// Function name fragments that indicate a hot-path context.
const HOT_PATH_FN_NAMES: &[&str] = &[
    "Update",
    "Heartbeat",
    "Step",
    "Render",
    "RenderStepped",
    "PreSimulation",
    "PostSimulation",
];

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Validate all `.luau` files under the given include roots.
pub fn validate_source(root: &Path, includes: &[String]) -> Result<ValidationReport> {
    let resolved = crate::resolve_includes(includes);
    let mut issues: Vec<ValidationIssue> = Vec::new();
    let mut files_checked: usize = 0;

    for inc in &resolved {
        let inc_path = root.join(inc);
        if !inc_path.exists() {
            continue;
        }
        walk_and_validate(&inc_path, root, &mut files_checked, &mut issues)?;
    }

    let errors = issues.iter().filter(|i| i.severity == "error").count();
    let warnings = issues.iter().filter(|i| i.severity == "warning").count();
    let clean = errors == 0 && warnings == 0;

    Ok(ValidationReport {
        files_checked,
        errors,
        warnings,
        issues,
        clean,
    })
}

/// Validate a single file given its content and relative path.
/// Useful for validating patched files without a full tree walk.
pub fn validate_file_content(rel_path: &str, content: &str) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    if rel_path.ends_with(".luau") || rel_path.ends_with(".lua") {
        let lines: Vec<&str> = content.lines().collect();
        check_strict_mode(rel_path, &lines, &mut issues);
        check_cross_boundary_require(rel_path, &lines, &mut issues);
        check_deprecated_api(rel_path, &lines, &mut issues);
        check_large_file(rel_path, &lines, &mut issues);
        check_tab_indent(rel_path, &lines, &mut issues);
        check_instance_new_hot_path(rel_path, &lines, &mut issues);
        check_ncg_untyped_param(rel_path, &lines, &mut issues);
        check_ncg_closure_in_loop(rel_path, &lines, &mut issues);
        check_ncg_pattern_in_hot_path(rel_path, &lines, &mut issues);
        check_perf_dynamic_array(rel_path, &lines, &mut issues);
        check_perf_unfrozen_constant(rel_path, &lines, &mut issues);
        check_perf_missing_native(rel_path, &lines, &mut issues);
        check_perf_pcall_in_native(rel_path, &lines, &mut issues);
    }
    issues
}

/// Try running `selene` on the given root. Returns issues if selene is found,
/// or `None` if selene is not available on PATH.
pub fn run_selene(root: &Path, includes: &[String]) -> Option<Vec<String>> {
    let selene_path = which_selene()?;

    let resolved = crate::resolve_includes(includes);
    let mut skipped_studio_plugin = false;
    let paths: Vec<String> = resolved
        .iter()
        .filter_map(|inc| {
            if should_skip_selene_include(inc) {
                skipped_studio_plugin = true;
                return None;
            }
            Some(root.join(inc).to_string_lossy().into_owned())
        })
        .filter(|p| Path::new(p).exists())
        .collect();

    if paths.is_empty() {
        if skipped_studio_plugin {
            return Some(vec![selene_skip_note()]);
        }
        return Some(Vec::new());
    }

    let output = Command::new(&selene_path)
        .args(&paths)
        .current_dir(root)
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut lines: Vec<String> = Vec::new();
    for line in stdout.lines().chain(stderr.lines()) {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            lines.push(trimmed.to_string());
        }
    }

    if skipped_studio_plugin {
        lines.push(selene_skip_note());
    }

    Some(lines)
}

// ---------------------------------------------------------------------------
// Individual checks
// ---------------------------------------------------------------------------

fn check_strict_mode(path: &str, lines: &[&str], issues: &mut Vec<ValidationIssue>) {
    if lines.is_empty() || lines[0].trim() != "--!strict" {
        issues.push(ValidationIssue {
            path: path.to_string(),
            line: 1,
            severity: "error".to_string(),
            message: "missing `--!strict` on line 1".to_string(),
            rule: RULE_STRICT_MODE.to_string(),
        });
    }
}

fn check_cross_boundary_require(path: &str, lines: &[&str], issues: &mut Vec<ValidationIssue>) {
    let is_client = path.contains("Client/") || path.contains("Client\\");
    if !is_client {
        return;
    }

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        // Skip comments.
        if trimmed.starts_with("--") {
            continue;
        }
        if trimmed.contains("require(game.ServerScriptService")
            || trimmed.contains("require(game:GetService(\"ServerScriptService\")")
            || trimmed.contains("require(game:GetService('ServerScriptService')")
        {
            issues.push(ValidationIssue {
                path: path.to_string(),
                line: idx + 1,
                severity: "error".to_string(),
                message: "client code requires ServerScriptService (cross-boundary)".to_string(),
                rule: RULE_CROSS_BOUNDARY_REQUIRE.to_string(),
            });
        }
    }
}

fn check_deprecated_api(path: &str, lines: &[&str], issues: &mut Vec<ValidationIssue>) {
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        // Skip comments.
        if trimmed.starts_with("--") {
            continue;
        }

        // Flag tick() usage — should be os.clock().
        // Match standalone tick() calls, not words containing "tick" like "sticky".
        if contains_word_call(trimmed, "tick") {
            issues.push(ValidationIssue {
                path: path.to_string(),
                line: idx + 1,
                severity: "warning".to_string(),
                message: "use `os.clock()` instead of `tick()`".to_string(),
                rule: RULE_DEPRECATED_API.to_string(),
            });
        }

        // Flag bare wait() — should be task.wait().
        // Match standalone wait() but not task.wait().
        if contains_bare_wait(trimmed) {
            issues.push(ValidationIssue {
                path: path.to_string(),
                line: idx + 1,
                severity: "warning".to_string(),
                message: "use `task.wait()` instead of `wait()`".to_string(),
                rule: RULE_DEPRECATED_API.to_string(),
            });
        }
    }
}

fn check_large_file(path: &str, lines: &[&str], issues: &mut Vec<ValidationIssue>) {
    if lines.len() > LARGE_FILE_LINES {
        issues.push(ValidationIssue {
            path: path.to_string(),
            line: 0,
            severity: "warning".to_string(),
            message: format!(
                "file has {} lines (threshold: {})",
                lines.len(),
                LARGE_FILE_LINES
            ),
            rule: RULE_LARGE_FILE.to_string(),
        });
    }
}

fn check_tab_indent(path: &str, lines: &[&str], issues: &mut Vec<ValidationIssue>) {
    let mut space_indent_lines = 0usize;
    let mut first_space_line = 0usize;

    for (idx, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        // Check if the line starts with spaces (not tabs) for indentation.
        let first_char = line.as_bytes().first();
        if first_char == Some(&b' ') {
            // Could be alignment or actual indentation. Count lines with >=2 leading spaces
            // that are not continuation/alignment (heuristic: starts with 2+ spaces).
            let leading_spaces = line.len() - line.trim_start_matches(' ').len();
            if leading_spaces >= 2 {
                if space_indent_lines == 0 {
                    first_space_line = idx + 1;
                }
                space_indent_lines += 1;
            }
        }
    }

    // Only flag if a significant portion of lines use spaces (>5 lines, to avoid false positives
    // on occasional alignment).
    if space_indent_lines > 5 {
        issues.push(ValidationIssue {
            path: path.to_string(),
            line: first_space_line,
            severity: "warning".to_string(),
            message: format!(
                "{} lines use space indentation (project uses tabs per stylua.toml)",
                space_indent_lines
            ),
            rule: RULE_TAB_INDENT.to_string(),
        });
    }
}

fn check_instance_new_hot_path(path: &str, lines: &[&str], issues: &mut Vec<ValidationIssue>) {
    // Track whether we're inside a function whose name contains a hot-path keyword.
    let mut in_hot_fn = false;
    let mut hot_fn_depth = 0i32;
    let mut current_depth = 0i32;

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("--") {
            continue;
        }

        // Track function entry.
        if trimmed.starts_with("function ") || trimmed.contains("= function(") {
            if !in_hot_fn && is_hot_path_function(trimmed) {
                in_hot_fn = true;
                hot_fn_depth = current_depth;
            }
            current_depth += 1;
        }

        // Track end keywords (simplified — counts end/do/if/for/while blocks).
        if trimmed == "end"
            || trimmed == "end)"
            || trimmed.starts_with("end,")
            || trimmed.starts_with("end)")
        {
            current_depth -= 1;
            if in_hot_fn && current_depth <= hot_fn_depth {
                in_hot_fn = false;
            }
        }

        // Flag Instance.new in hot path.
        if in_hot_fn && trimmed.contains("Instance.new(") {
            issues.push(ValidationIssue {
                path: path.to_string(),
                line: idx + 1,
                severity: "warning".to_string(),
                message: "Instance.new() in hot-path function (performance anti-pattern)"
                    .to_string(),
                rule: RULE_INSTANCE_NEW_HOT_PATH.to_string(),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// NCG / Performance checks
// ---------------------------------------------------------------------------

/// Returns `true` if the file-level directives indicate native codegen is active.
fn file_has_native(lines: &[&str]) -> bool {
    for line in lines.iter().take(10) {
        let trimmed = line.trim();
        if trimmed == "--!native" || trimmed == "@native" {
            return true;
        }
    }
    false
}

/// Check for function parameters without type annotations in @native / --!native files.
fn check_ncg_untyped_param(path: &str, lines: &[&str], issues: &mut Vec<ValidationIssue>) {
    if !file_has_native(lines) {
        return;
    }

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("--") {
            continue;
        }

        // Match function definitions with parameter lists.
        let fn_pos = if let Some(p) = trimmed.find("function") {
            p
        } else {
            continue;
        };

        // Find the opening paren after "function".
        let after_fn = fn_pos + "function".len();
        let rest = &trimmed[after_fn..];
        let open_paren = if let Some(p) = rest.find('(') {
            after_fn + p
        } else {
            continue;
        };
        let close_paren = if let Some(p) = trimmed[open_paren..].find(')') {
            open_paren + p
        } else {
            continue;
        };

        let params_str = &trimmed[open_paren + 1..close_paren];
        if params_str.trim().is_empty() {
            continue;
        }

        // Check each parameter for a type annotation.
        let params: Vec<&str> = params_str.split(',').collect();
        let mut has_untyped = false;
        for param in &params {
            let p = param.trim();
            if p == "..." || p == "self" || p.is_empty() {
                continue;
            }
            if !p.contains(':') {
                has_untyped = true;
                break;
            }
        }

        if has_untyped {
            issues.push(ValidationIssue {
                path: path.to_string(),
                line: idx + 1,
                severity: "warning".to_string(),
                message: format!("add type annotations for NCG optimization: ({params_str})"),
                rule: RULE_NCG_UNTYPED_PARAM.to_string(),
            });
        }
    }
}

/// Detect closure creation (`function(`) inside for/while loops.
fn check_ncg_closure_in_loop(path: &str, lines: &[&str], issues: &mut Vec<ValidationIssue>) {
    let mut loop_depth: i32 = 0;

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("--") {
            continue;
        }

        // Detect loop openers.
        if (trimmed.starts_with("for ") || trimmed.starts_with("while ")) && trimmed.ends_with("do")
        {
            loop_depth += 1;
        }

        // Detect closures inside loops.
        if loop_depth > 0 && trimmed.contains("function(") {
            // Avoid flagging the loop line itself if it is the for/while definition.
            if !(trimmed.starts_with("for ") || trimmed.starts_with("while ")) {
                issues.push(ValidationIssue {
                    path: path.to_string(),
                    line: idx + 1,
                    severity: "warning".to_string(),
                    message: "extract closure to module-level function to avoid NCG bailout"
                        .to_string(),
                    rule: RULE_NCG_CLOSURE_IN_LOOP.to_string(),
                });
            }
        }

        // Track block ends.
        if (trimmed == "end"
            || trimmed == "end)"
            || trimmed.starts_with("end,")
            || trimmed.starts_with("end)"))
            && loop_depth > 0
        {
            loop_depth -= 1;
        }
    }
}

/// String pattern operations in @native functions.
const PATTERN_OPS: &[&str] = &["gmatch(", "gsub(", ":match(", "string.match("];

fn check_ncg_pattern_in_hot_path(path: &str, lines: &[&str], issues: &mut Vec<ValidationIssue>) {
    if !file_has_native(lines) {
        return;
    }

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("--") {
            continue;
        }

        for op in PATTERN_OPS {
            if trimmed.contains(op) {
                issues.push(ValidationIssue {
                    path: path.to_string(),
                    line: idx + 1,
                    severity: "warning".to_string(),
                    message: "use string.split() instead of pattern ops for NCG compatibility"
                        .to_string(),
                    rule: RULE_NCG_PATTERN_IN_HOT_PATH.to_string(),
                });
                break;
            }
        }
    }
}

/// Detect `local t = {}` followed by `table.insert(t, ...)` in a loop.
fn check_perf_dynamic_array(path: &str, lines: &[&str], issues: &mut Vec<ValidationIssue>) {
    // Collect names of variables assigned `{}`.
    let mut empty_table_vars: Vec<(String, usize)> = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("--") {
            continue;
        }

        // Detect `local NAME = {}`.
        if let Some(rest) = trimmed.strip_prefix("local ")
            && let Some(eq_pos) = rest.find('=')
        {
            let name = rest[..eq_pos].trim();
            let value = rest[eq_pos + 1..].trim();
            if value == "{}" && !name.contains(',') && !name.contains(':') {
                empty_table_vars.push((name.to_string(), idx));
            }
        }
    }

    // Now look for table.insert(NAME, ...) inside loops.
    let mut loop_depth: i32 = 0;

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("--") {
            continue;
        }

        if (trimmed.starts_with("for ") || trimmed.starts_with("while ")) && trimmed.ends_with("do")
        {
            loop_depth += 1;
        }

        if (trimmed == "end"
            || trimmed == "end)"
            || trimmed.starts_with("end,")
            || trimmed.starts_with("end)"))
            && loop_depth > 0
        {
            loop_depth -= 1;
        }

        if loop_depth > 0 && trimmed.contains("table.insert(") {
            // Check if the first arg to table.insert matches an empty-table var declared before.
            if let Some(args_start) = trimmed.find("table.insert(") {
                let after = &trimmed[args_start + "table.insert(".len()..];
                let first_arg = after.split([',', ')']).next().unwrap_or("").trim();
                for (var_name, _decl_line) in &empty_table_vars {
                    if first_arg == var_name {
                        issues.push(ValidationIssue {
                            path: path.to_string(),
                            line: idx + 1,
                            severity: "info".to_string(),
                            message: format!(
                                "use table.create(N) for pre-sized arrays instead of {{}} + table.insert for `{var_name}`"
                            ),
                            rule: RULE_PERF_DYNAMIC_ARRAY.to_string(),
                        });
                        break;
                    }
                }
            }
        }
    }
}

/// Detect module-level UPPER_CASE constant tables without table.freeze.
fn check_perf_unfrozen_constant(path: &str, lines: &[&str], issues: &mut Vec<ValidationIssue>) {
    // Gather module-level constant table names (all-uppercase with underscores).
    let mut fn_depth: i32 = 0;
    let mut constant_tables: Vec<(String, usize)> = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("--") {
            continue;
        }

        // Track function depth to identify module-level scope.
        if trimmed.starts_with("function ") || trimmed.contains("= function(") {
            fn_depth += 1;
        }
        if (trimmed == "end"
            || trimmed == "end)"
            || trimmed.starts_with("end,")
            || trimmed.starts_with("end)"))
            && fn_depth > 0
        {
            fn_depth -= 1;
        }

        if fn_depth > 0 {
            continue;
        }

        // Look for `local UPPER_NAME = {` at module level.
        if let Some(rest) = trimmed.strip_prefix("local ")
            && let Some(eq_pos) = rest.find('=')
        {
            let name = rest[..eq_pos].trim();
            let value = rest[eq_pos + 1..].trim();
            // Check name is UPPER_CASE (at least 2 chars, all uppercase/underscore/digit).
            if name.len() >= 2
                && name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
                && name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && value.starts_with('{')
            {
                constant_tables.push((name.to_string(), idx));
            }
        }
    }

    if constant_tables.is_empty() {
        return;
    }

    // Check if table.freeze(NAME) appears anywhere in the file.
    let content_joined: String = lines.join("\n");
    for (name, decl_line) in &constant_tables {
        let freeze_pattern = format!("table.freeze({name})");
        if !content_joined.contains(&freeze_pattern) {
            issues.push(ValidationIssue {
                path: path.to_string(),
                line: decl_line + 1,
                severity: "info".to_string(),
                message: format!(
                    "use table.freeze() for constant table `{name}` to enable NCG inlining"
                ),
                rule: RULE_PERF_UNFROZEN_CONSTANT.to_string(),
            });
        }
    }
}

/// Math/vector operation fragments that indicate compute-heavy code.
const MATH_VECTOR_OPS: &[&str] = &[
    "math.sqrt",
    "math.sin",
    "math.cos",
    "math.atan2",
    "math.abs",
    "math.floor",
    "math.ceil",
    "math.clamp",
    "math.lerp",
    "math.exp",
    "math.log",
    "math.pow",
    "math.min",
    "math.max",
    "vector.dot",
    "vector.cross",
    "vector.magnitude",
    "vector.normalize",
    "Vector3.new",
    "CFrame.new",
];

/// Threshold for number of math/vector ops before suggesting @native.
const NATIVE_SUGGESTION_THRESHOLD: usize = 5;

/// Detect compute-heavy functions lacking @native.
fn check_perf_missing_native(path: &str, lines: &[&str], issues: &mut Vec<ValidationIssue>) {
    // If the file is already --!native, the whole file is native — skip.
    if file_has_native(lines) {
        return;
    }

    // Track per-function math/vector operation counts.
    struct FnInfo {
        name_line: usize,
        math_ops: usize,
        has_native_attr: bool,
    }

    let mut fn_stack: Vec<FnInfo> = Vec::new();
    let mut depth: i32 = 0;

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        // Check for @native attribute on line immediately before a function.
        let is_native_attr = trimmed == "@native";

        if trimmed.starts_with("--") && !is_native_attr {
            continue;
        }

        // Function start.
        if trimmed.starts_with("function ") || trimmed.contains("= function(") {
            let has_attr = if idx > 0 {
                lines[idx - 1].trim() == "@native"
            } else {
                false
            };
            fn_stack.push(FnInfo {
                name_line: idx,
                math_ops: 0,
                has_native_attr: has_attr,
            });
            depth += 1;
            continue;
        }

        // Count math/vector ops inside current function.
        if !fn_stack.is_empty() {
            for op in MATH_VECTOR_OPS {
                if trimmed.contains(op)
                    && let Some(info) = fn_stack.last_mut()
                {
                    info.math_ops += 1;
                }
            }
        }

        // Function end.
        if trimmed == "end"
            || trimmed == "end)"
            || trimmed.starts_with("end,")
            || trimmed.starts_with("end)")
        {
            depth -= 1;
            if let Some(info) = fn_stack.pop()
                && info.math_ops >= NATIVE_SUGGESTION_THRESHOLD
                && !info.has_native_attr
            {
                issues.push(ValidationIssue {
                    path: path.to_string(),
                    line: info.name_line + 1,
                    severity: "info".to_string(),
                    message: format!(
                        "consider adding @native for NCG optimization ({} math/vector ops)",
                        info.math_ops
                    ),
                    rule: RULE_PERF_MISSING_NATIVE.to_string(),
                });
            }
        }
    }

    // Suppress unused variable warning.
    let _ = depth;
}

/// Detect pcall/xpcall inside @native functions or --!native files.
fn check_perf_pcall_in_native(path: &str, lines: &[&str], issues: &mut Vec<ValidationIssue>) {
    let file_native = file_has_native(lines);
    let mut in_native_fn = false;
    let mut native_fn_depth: i32 = 0;
    let mut current_depth: i32 = 0;

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("--") && trimmed != "--!native" {
            continue;
        }

        // Function start.
        if trimmed.starts_with("function ") || trimmed.contains("= function(") {
            if !in_native_fn {
                let has_attr = if idx > 0 {
                    lines[idx - 1].trim() == "@native"
                } else {
                    false
                };
                if file_native || has_attr {
                    in_native_fn = true;
                    native_fn_depth = current_depth;
                }
            }
            current_depth += 1;
        }

        // Function end.
        if trimmed == "end"
            || trimmed == "end)"
            || trimmed.starts_with("end,")
            || trimmed.starts_with("end)")
        {
            current_depth -= 1;
            if in_native_fn && current_depth <= native_fn_depth {
                in_native_fn = false;
            }
        }

        // Flag pcall/xpcall in native context.
        if in_native_fn
            && (contains_word_call(trimmed, "pcall") || contains_word_call(trimmed, "xpcall"))
        {
            issues.push(ValidationIssue {
                    path: path.to_string(),
                    line: idx + 1,
                    severity: "warning".to_string(),
                    message:
                        "pcall creates a barrier instruction; move error handling outside @native function"
                            .to_string(),
                    rule: RULE_PERF_PCALL_IN_NATIVE.to_string(),
                });
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if a line contains a standalone `tick()` call (not part of another word).
fn contains_word_call(line: &str, word: &str) -> bool {
    let mut search_from = 0;
    while let Some(pos) = line[search_from..].find(word) {
        let abs_pos = search_from + pos;
        let after = abs_pos + word.len();

        // Check character before: must not be alphanumeric, underscore, or dot.
        let before_ok = if abs_pos == 0 {
            true
        } else {
            let ch = line.as_bytes()[abs_pos - 1];
            !ch.is_ascii_alphanumeric() && ch != b'_' && ch != b'.'
        };

        // Check character after: must be '(' for a function call.
        let after_ok = line.as_bytes().get(after) == Some(&b'(');

        if before_ok && after_ok {
            return true;
        }

        search_from = abs_pos + 1;
        if search_from >= line.len() {
            break;
        }
    }
    false
}

/// Check if a line contains bare `wait(` not preceded by `.` (i.e., not `task.wait(`).
fn contains_bare_wait(line: &str) -> bool {
    let mut search_from = 0;
    while let Some(pos) = line[search_from..].find("wait(") {
        let abs_pos = search_from + pos;

        // Must not be preceded by a dot (task.wait, coroutine.wait, etc.).
        let before_ok = if abs_pos == 0 {
            true
        } else {
            let ch = line.as_bytes()[abs_pos - 1];
            !ch.is_ascii_alphanumeric() && ch != b'_' && ch != b'.'
        };

        if before_ok {
            return true;
        }

        search_from = abs_pos + 1;
        if search_from >= line.len() {
            break;
        }
    }
    false
}

/// Check if a function definition line contains a hot-path name.
fn is_hot_path_function(line: &str) -> bool {
    HOT_PATH_FN_NAMES.iter().any(|name| line.contains(name))
}

/// Find selene on PATH.
fn which_selene() -> Option<String> {
    // Check common locations first.
    for path in &[std::env::var("HOME")
        .map(|h| format!("{h}/.aftman/bin/selene"))
        .unwrap_or_default()]
    {
        if !path.is_empty() && Path::new(path).is_file() {
            return Some(path.clone());
        }
    }

    // Fall back to PATH lookup via `which`.
    Command::new("which")
        .arg("selene")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !path.is_empty() { Some(path) } else { None }
            } else {
                None
            }
        })
}

fn should_skip_selene_include(include: &str) -> bool {
    let normalized = include.replace('\\', "/");
    let normalized = normalized.trim_start_matches("./");
    normalized == "studio-plugin" || normalized.starts_with("studio-plugin/")
}

fn selene_skip_note() -> String {
    "[vertigo-sync] selene skipped studio-plugin (Luau @native parsing is unstable in current selene). Built-in validator still enforces strict/perf rules.".to_string()
}

fn walk_and_validate(
    dir: &Path,
    root: &Path,
    files_checked: &mut usize,
    issues: &mut Vec<ValidationIssue>,
) -> Result<()> {
    let read_dir =
        std::fs::read_dir(dir).with_context(|| format!("cannot read dir: {}", dir.display()))?;

    for entry in read_dir {
        let entry = entry?;
        let ft = entry.file_type()?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if ft.is_symlink() {
            continue;
        }

        if ft.is_dir() {
            if matches!(
                name_str.as_ref(),
                ".git" | "node_modules" | "Packages" | "target" | "dist" | "build"
            ) {
                continue;
            }
            walk_and_validate(&entry.path(), root, files_checked, issues)?;
            continue;
        }

        if !ft.is_file() {
            continue;
        }

        let path = entry.path();
        let ext_match = name_str.ends_with(".luau") || name_str.ends_with(".lua");
        if !ext_match {
            continue;
        }

        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue, // Skip non-UTF-8 files (caught by health doctor).
        };

        *files_checked += 1;
        let file_issues = validate_file_content(&rel, &content);
        issues.extend(file_issues);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn strict_mode_pass() {
        let issues = validate_file_content("src/Server/Foo.luau", "--!strict\nlocal x = 1\n");
        assert!(
            !issues.iter().any(|i| i.rule == RULE_STRICT_MODE),
            "should not flag strict mode when present"
        );
    }

    #[test]
    fn strict_mode_missing() {
        let issues = validate_file_content("src/Server/Foo.luau", "local x = 1\n");
        assert!(
            issues.iter().any(|i| i.rule == RULE_STRICT_MODE),
            "should flag missing --!strict"
        );
    }

    #[test]
    fn strict_mode_empty_file() {
        let issues = validate_file_content("src/Server/Empty.luau", "");
        assert!(
            issues.iter().any(|i| i.rule == RULE_STRICT_MODE),
            "should flag empty file missing --!strict"
        );
    }

    #[test]
    fn cross_boundary_require_client() {
        let content = "--!strict\nlocal Svc = require(game.ServerScriptService.Foo)\n";
        let issues = validate_file_content("src/Client/Controllers/Bad.luau", content);
        assert!(
            issues.iter().any(|i| i.rule == RULE_CROSS_BOUNDARY_REQUIRE),
            "should flag cross-boundary require in client code"
        );
    }

    #[test]
    fn cross_boundary_require_server_ok() {
        let content = "--!strict\nlocal Svc = require(game.ServerScriptService.Foo)\n";
        let issues = validate_file_content("src/Server/Services/Ok.luau", content);
        assert!(
            !issues.iter().any(|i| i.rule == RULE_CROSS_BOUNDARY_REQUIRE),
            "should not flag server-side require of ServerScriptService"
        );
    }

    #[test]
    fn cross_boundary_require_comment_ignored() {
        let content = "--!strict\n-- require(game.ServerScriptService.Foo)\n";
        let issues = validate_file_content("src/Client/Controllers/Ok.luau", content);
        assert!(
            !issues.iter().any(|i| i.rule == RULE_CROSS_BOUNDARY_REQUIRE),
            "should not flag commented-out require"
        );
    }

    #[test]
    fn deprecated_tick() {
        let content = "--!strict\nlocal t = tick()\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        let tick_issues: Vec<_> = issues
            .iter()
            .filter(|i| i.rule == RULE_DEPRECATED_API && i.message.contains("tick"))
            .collect();
        assert_eq!(tick_issues.len(), 1, "should flag tick()");
    }

    #[test]
    fn deprecated_tick_not_sticky() {
        let content = "--!strict\nlocal sticky = true\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            !issues
                .iter()
                .any(|i| i.rule == RULE_DEPRECATED_API && i.message.contains("tick")),
            "should not flag 'sticky' as tick()"
        );
    }

    #[test]
    fn deprecated_wait() {
        let content = "--!strict\nwait(1)\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        let wait_issues: Vec<_> = issues
            .iter()
            .filter(|i| i.rule == RULE_DEPRECATED_API && i.message.contains("wait"))
            .collect();
        assert_eq!(wait_issues.len(), 1, "should flag bare wait()");
    }

    #[test]
    fn task_wait_ok() {
        let content = "--!strict\ntask.wait(1)\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            !issues
                .iter()
                .any(|i| i.rule == RULE_DEPRECATED_API && i.message.contains("wait")),
            "should not flag task.wait()"
        );
    }

    #[test]
    fn large_file_warning() {
        let mut content = String::from("--!strict\n");
        for i in 0..510 {
            content.push_str(&format!("local x{i} = {i}\n"));
        }
        let issues = validate_file_content("src/Server/Big.luau", &content);
        assert!(
            issues.iter().any(|i| i.rule == RULE_LARGE_FILE),
            "should warn on large file"
        );
    }

    #[test]
    fn small_file_no_warning() {
        let content = "--!strict\nlocal x = 1\nreturn x\n";
        let issues = validate_file_content("src/Server/Small.luau", &content);
        assert!(
            !issues.iter().any(|i| i.rule == RULE_LARGE_FILE),
            "should not warn on small file"
        );
    }

    #[test]
    fn tab_indent_ok() {
        let content = "--!strict\nfunction foo()\n\tlocal x = 1\n\treturn x\nend\n";
        let issues = validate_file_content("src/Server/Tabs.luau", content);
        assert!(
            !issues.iter().any(|i| i.rule == RULE_TAB_INDENT),
            "should not flag tab-indented code"
        );
    }

    #[test]
    fn space_indent_flagged() {
        let mut content = String::from("--!strict\n");
        for _ in 0..10 {
            content.push_str("  local x = 1\n");
        }
        let issues = validate_file_content("src/Server/Spaces.luau", &content);
        assert!(
            issues.iter().any(|i| i.rule == RULE_TAB_INDENT),
            "should flag space-indented code"
        );
    }

    #[test]
    fn instance_new_in_hot_path() {
        let content =
            "--!strict\nfunction MyModule:Heartbeat(dt)\n\tlocal p = Instance.new(\"Part\")\nend\n";
        let issues = validate_file_content("src/Client/Controllers/Bad.luau", content);
        assert!(
            issues.iter().any(|i| i.rule == RULE_INSTANCE_NEW_HOT_PATH),
            "should flag Instance.new in Heartbeat"
        );
    }

    #[test]
    fn instance_new_outside_hot_path_ok() {
        let content =
            "--!strict\nfunction MyModule:Init()\n\tlocal p = Instance.new(\"Part\")\nend\n";
        let issues = validate_file_content("src/Client/Controllers/Ok.luau", content);
        assert!(
            !issues.iter().any(|i| i.rule == RULE_INSTANCE_NEW_HOT_PATH),
            "should not flag Instance.new in Init"
        );
    }

    #[test]
    fn non_luau_files_skipped() {
        let issues = validate_file_content("src/Server/readme.md", "no strict mode here");
        assert!(issues.is_empty(), "should skip non-luau files");
    }

    #[test]
    fn validate_source_integration() {
        let root = tempdir().expect("tempdir");
        let src = root.path().join("src/Server");
        fs::create_dir_all(&src).expect("mkdir");

        fs::write(src.join("Good.luau"), "--!strict\nreturn {}\n").expect("write good");
        fs::write(src.join("Bad.luau"), "local x = tick()\nreturn x\n").expect("write bad");

        let includes = vec!["src".to_string()];
        let report = validate_source(root.path(), &includes).expect("validate");

        assert_eq!(report.files_checked, 2);
        assert!(!report.clean, "should not be clean with issues");
        assert!(report.errors > 0, "should have errors (missing --!strict)");
        assert!(report.warnings > 0, "should have warnings (tick)");
    }

    #[test]
    fn validate_file_content_returns_empty_for_clean() {
        let content = "--!strict\nlocal x = os.clock()\ntask.wait(0.1)\nreturn x\n";
        let issues = validate_file_content("src/Server/Clean.luau", content);
        assert!(issues.is_empty(), "clean file should have no issues");
    }

    #[test]
    fn contains_word_call_edge_cases() {
        assert!(contains_word_call("tick()", "tick"));
        assert!(contains_word_call("local t = tick()", "tick"));
        assert!(!contains_word_call("sticky()", "tick"));
        assert!(!contains_word_call("os.tick()", "tick"));
        assert!(contains_word_call("x = tick() + 1", "tick"));
    }

    #[test]
    fn contains_bare_wait_edge_cases() {
        assert!(contains_bare_wait("wait(1)"));
        assert!(contains_bare_wait("local x = wait(0.5)"));
        assert!(!contains_bare_wait("task.wait(1)"));
        assert!(!contains_bare_wait("coroutine.wait(1)"));
    }

    // -----------------------------------------------------------------------
    // NCG / Performance lint tests
    // -----------------------------------------------------------------------

    #[test]
    fn ncg_untyped_param_flagged() {
        let content = "--!strict\n--!native\nlocal function foo(x, y)\n\treturn x + y\nend\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            issues.iter().any(|i| i.rule == RULE_NCG_UNTYPED_PARAM),
            "should flag untyped params in @native file"
        );
    }

    #[test]
    fn ncg_untyped_param_typed_ok() {
        let content =
            "--!strict\n--!native\nlocal function foo(x: number, y: number)\n\treturn x + y\nend\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            !issues.iter().any(|i| i.rule == RULE_NCG_UNTYPED_PARAM),
            "should not flag typed params in @native file"
        );
    }

    #[test]
    fn ncg_untyped_param_not_native_ok() {
        let content = "--!strict\nlocal function foo(x, y)\n\treturn x + y\nend\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            !issues.iter().any(|i| i.rule == RULE_NCG_UNTYPED_PARAM),
            "should not flag untyped params in non-native file"
        );
    }

    #[test]
    fn ncg_closure_in_loop_flagged() {
        let content =
            "--!strict\nfor i = 1, 10 do\n\ttask.spawn(function()\n\t\tprint(i)\n\tend)\nend\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            issues.iter().any(|i| i.rule == RULE_NCG_CLOSURE_IN_LOOP),
            "should flag closure inside loop"
        );
    }

    #[test]
    fn ncg_closure_outside_loop_ok() {
        let content = "--!strict\nlocal fn = function()\n\tprint(1)\nend\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            !issues.iter().any(|i| i.rule == RULE_NCG_CLOSURE_IN_LOOP),
            "should not flag closure outside loop"
        );
    }

    #[test]
    fn ncg_pattern_in_native_flagged() {
        let content = "--!strict\n--!native\nfor seg in path:gmatch(\"[^/]+\") do\nend\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            issues
                .iter()
                .any(|i| i.rule == RULE_NCG_PATTERN_IN_HOT_PATH),
            "should flag gmatch in native file"
        );
    }

    #[test]
    fn ncg_pattern_in_non_native_ok() {
        let content = "--!strict\nfor seg in path:gmatch(\"[^/]+\") do\nend\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            !issues
                .iter()
                .any(|i| i.rule == RULE_NCG_PATTERN_IN_HOT_PATH),
            "should not flag gmatch in non-native file"
        );
    }

    #[test]
    fn perf_dynamic_array_flagged() {
        let content =
            "--!strict\nlocal results = {}\nfor i = 1, 100 do\n\ttable.insert(results, i)\nend\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            issues.iter().any(|i| i.rule == RULE_PERF_DYNAMIC_ARRAY),
            "should flag empty table + table.insert in loop"
        );
    }

    #[test]
    fn perf_dynamic_array_outside_loop_ok() {
        let content = "--!strict\nlocal results = {}\ntable.insert(results, 1)\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            !issues.iter().any(|i| i.rule == RULE_PERF_DYNAMIC_ARRAY),
            "should not flag table.insert outside loop"
        );
    }

    #[test]
    fn perf_unfrozen_constant_flagged() {
        let content = "--!strict\nlocal MAX_VALUES = { 10, 20, 30 }\nreturn MAX_VALUES\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            issues.iter().any(|i| i.rule == RULE_PERF_UNFROZEN_CONSTANT),
            "should flag unfrozen constant table"
        );
    }

    #[test]
    fn perf_unfrozen_constant_frozen_ok() {
        let content = "--!strict\nlocal MAX_VALUES = { 10, 20, 30 }\ntable.freeze(MAX_VALUES)\nreturn MAX_VALUES\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            !issues.iter().any(|i| i.rule == RULE_PERF_UNFROZEN_CONSTANT),
            "should not flag frozen constant table"
        );
    }

    #[test]
    fn perf_unfrozen_constant_lowercase_ok() {
        let content = "--!strict\nlocal myTable = { 10, 20, 30 }\nreturn myTable\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            !issues.iter().any(|i| i.rule == RULE_PERF_UNFROZEN_CONSTANT),
            "should not flag lowercase table name"
        );
    }

    #[test]
    fn perf_missing_native_flagged() {
        let content = "--!strict\nfunction heavyMath(x)\n\tlocal a = math.sqrt(x)\n\tlocal b = math.sin(x)\n\tlocal c = math.cos(x)\n\tlocal d = math.abs(x)\n\tlocal e = math.floor(x)\n\treturn a + b + c + d + e\nend\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            issues.iter().any(|i| i.rule == RULE_PERF_MISSING_NATIVE),
            "should suggest @native for compute-heavy function"
        );
    }

    #[test]
    fn perf_missing_native_light_fn_ok() {
        let content = "--!strict\nfunction simple(x)\n\treturn x + 1\nend\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            !issues.iter().any(|i| i.rule == RULE_PERF_MISSING_NATIVE),
            "should not suggest @native for light function"
        );
    }

    #[test]
    fn perf_missing_native_already_native_ok() {
        let content = "--!strict\n--!native\nfunction heavyMath(x)\n\tlocal a = math.sqrt(x)\n\tlocal b = math.sin(x)\n\tlocal c = math.cos(x)\n\tlocal d = math.abs(x)\n\tlocal e = math.floor(x)\n\treturn a + b + c + d + e\nend\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            !issues.iter().any(|i| i.rule == RULE_PERF_MISSING_NATIVE),
            "should not suggest @native when file already has --!native"
        );
    }

    #[test]
    fn perf_pcall_in_native_flagged() {
        let content = "--!strict\n--!native\nfunction doWork()\n\tlocal ok = pcall(riskyFn)\nend\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            issues.iter().any(|i| i.rule == RULE_PERF_PCALL_IN_NATIVE),
            "should flag pcall in native function"
        );
    }

    #[test]
    fn perf_pcall_not_native_ok() {
        let content = "--!strict\nfunction doWork()\n\tlocal ok = pcall(riskyFn)\nend\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            !issues.iter().any(|i| i.rule == RULE_PERF_PCALL_IN_NATIVE),
            "should not flag pcall in non-native function"
        );
    }

    #[test]
    fn perf_pcall_xpcall_in_native_flagged() {
        let content =
            "--!strict\n--!native\nfunction doWork()\n\tlocal ok = xpcall(riskyFn, handler)\nend\n";
        let issues = validate_file_content("src/Server/Foo.luau", content);
        assert!(
            issues.iter().any(|i| i.rule == RULE_PERF_PCALL_IN_NATIVE),
            "should flag xpcall in native function"
        );
    }
}
