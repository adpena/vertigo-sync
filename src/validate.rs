//! Luau source validation module.
//!
//! Runs built-in lint checks on `.luau` files without requiring external tools.
//! Optionally shells out to `selene` if available on PATH.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

use crate::GlobIgnoreSet;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginSafetyIssue {
    pub severity: String,
    pub rule: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginFunctionRiskFinding {
    pub name: String,
    pub start_line: usize,
    pub end_line: usize,
    pub parameter_count: usize,
    pub local_binding_count: usize,
    pub nested_closure_count: usize,
    pub line_span: usize,
    pub risk_score: usize,
    pub hard_fail: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginSafetyReport {
    pub path: String,
    pub compile_tool_available: bool,
    pub compile_ok: bool,
    pub analyze_tool_available: bool,
    pub analyze_ok: bool,
    pub top_level_symbol_count: usize,
    pub top_level_symbol_budget: usize,
    pub function_risk_findings: Vec<PluginFunctionRiskFinding>,
    pub warnings: Vec<PluginSafetyIssue>,
    pub errors: Vec<PluginSafetyIssue>,
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
const RULE_PLUGIN_COMPILE: &str = "plugin-compile";
const RULE_PLUGIN_ANALYZE: &str = "plugin-analyze";
const RULE_PLUGIN_TOOL_MISSING: &str = "plugin-tool-missing";
const RULE_PLUGIN_TOP_LEVEL_BUDGET: &str = "plugin-top-level-budget";
const RULE_PLUGIN_FUNCTION_RISK: &str = "plugin-function-risk";

/// Large file threshold (lines).
const LARGE_FILE_LINES: usize = 500;
const PLUGIN_TOP_LEVEL_SYMBOL_BUDGET: usize = 189;
const PLUGIN_FUNCTION_RISK_HARD_FAIL: usize = 90;

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
    validate_source_with_ignores(root, includes, &[])
}

pub fn validate_source_with_ignores(
    root: &Path,
    includes: &[String],
    ignore_globs: &[String],
) -> Result<ValidationReport> {
    let resolved = crate::resolve_includes(includes);
    let mut issues: Vec<ValidationIssue> = Vec::new();
    let mut files_checked: usize = 0;
    let ignores = GlobIgnoreSet::new(ignore_globs);

    for inc in &resolved {
        let inc_path = root.join(inc);
        if !inc_path.exists() {
            continue;
        }
        walk_and_validate(&inc_path, root, &ignores, &mut files_checked, &mut issues)?;
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

pub fn validate_plugin_source_text(path: &str, content: &str) -> Result<PluginSafetyReport> {
    let mut warnings = Vec::new();
    let mut errors = Vec::new();

    let compile = run_luau_tool("luau-compile", &[], content)?;
    if !compile.available {
        warnings.push(PluginSafetyIssue {
            severity: "warning".to_string(),
            rule: RULE_PLUGIN_TOOL_MISSING.to_string(),
            message: "luau-compile is not available on PATH; compile validation skipped"
                .to_string(),
        });
    } else if !compile.success {
        errors.push(PluginSafetyIssue {
            severity: "error".to_string(),
            rule: RULE_PLUGIN_COMPILE.to_string(),
            message: format!(
                "luau-compile failed for generated plugin: {}",
                first_non_empty_line(&compile.output).unwrap_or("unknown compiler failure")
            ),
        });
    }

    let analyze = run_luau_tool("luau-analyze", &["--formatter=plain"], content)?;
    if !analyze.available {
        warnings.push(PluginSafetyIssue {
            severity: "warning".to_string(),
            rule: RULE_PLUGIN_TOOL_MISSING.to_string(),
            message: "luau-analyze is not available on PATH; analyzer validation skipped"
                .to_string(),
        });
    } else if !analyze.success {
        errors.push(PluginSafetyIssue {
            severity: "error".to_string(),
            rule: RULE_PLUGIN_ANALYZE.to_string(),
            message: format!(
                "luau-analyze failed for generated plugin: {}",
                first_non_empty_line(&analyze.output).unwrap_or("unknown analyzer failure")
            ),
        });
    }

    let top_level_symbol_count = count_plugin_top_level_symbols(content);
    if top_level_symbol_count > PLUGIN_TOP_LEVEL_SYMBOL_BUDGET {
        errors.push(PluginSafetyIssue {
            severity: "error".to_string(),
            rule: RULE_PLUGIN_TOP_LEVEL_BUDGET.to_string(),
            message: format!(
                "generated plugin has {top_level_symbol_count} top-level symbols; budget is {}",
                PLUGIN_TOP_LEVEL_SYMBOL_BUDGET
            ),
        });
    }

    let function_risk_findings = plugin_function_risk_findings(content);
    for finding in &function_risk_findings {
        if finding.hard_fail {
            errors.push(PluginSafetyIssue {
                severity: "error".to_string(),
                rule: RULE_PLUGIN_FUNCTION_RISK.to_string(),
                message: format!(
                    "function `{}` lines {}-{} risk score {} exceeds plugin safety threshold {}",
                    finding.name,
                    finding.start_line,
                    finding.end_line,
                    finding.risk_score,
                    PLUGIN_FUNCTION_RISK_HARD_FAIL
                ),
            });
        }
    }

    let clean = errors.is_empty();

    Ok(PluginSafetyReport {
        path: path.to_string(),
        compile_tool_available: compile.available,
        compile_ok: compile.available && compile.success,
        analyze_tool_available: analyze.available,
        analyze_ok: analyze.available && analyze.success,
        top_level_symbol_count,
        top_level_symbol_budget: PLUGIN_TOP_LEVEL_SYMBOL_BUDGET,
        function_risk_findings,
        warnings,
        errors,
        clean,
    })
}

// ---------------------------------------------------------------------------
// Auto-fix support
// ---------------------------------------------------------------------------

/// Auto-fix known issues across all `.luau`/`.lua` files under the given
/// include roots. Returns the number of files that were modified.
///
/// Fixable rules:
/// - `wait-deprecated`  -> replace bare `wait(` with `task.wait(`
/// - `spawn-deprecated` -> replace bare `spawn(` with `task.spawn(`
/// - `delay-deprecated` -> replace bare `delay(` with `task.delay(`
/// - `strict-mode`      -> prepend `--!strict\n` to `.luau` files missing it
pub fn auto_fix_source_tree(
    root: &Path,
    includes: &[String],
    ignore_globs: &[String],
) -> Result<usize> {
    let resolved = crate::resolve_includes(includes);
    let ignores = GlobIgnoreSet::new(ignore_globs);
    let mut fixed_count = 0usize;

    for inc in &resolved {
        let inc_path = root.join(inc);
        if !inc_path.exists() {
            continue;
        }
        auto_fix_walk(&inc_path, root, &ignores, &mut fixed_count)?;
    }

    Ok(fixed_count)
}

fn auto_fix_walk(
    dir: &Path,
    root: &Path,
    ignores: &GlobIgnoreSet,
    fixed_count: &mut usize,
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
            auto_fix_walk(&entry.path(), root, ignores, fixed_count)?;
            continue;
        }
        if !ft.is_file() {
            continue;
        }

        let path = entry.path();
        let is_luau = name_str.ends_with(".luau");
        let is_lua = name_str.ends_with(".lua");
        if !is_luau && !is_lua {
            continue;
        }

        let rel = path.strip_prefix(root).unwrap_or(&path);
        let rel_str = rel.to_string_lossy();
        if ignores.is_ignored(&rel_str) {
            continue;
        }

        let original = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let mut content = original.clone();

        // Fix deprecated bare calls
        content = fix_deprecated_bare_call(&content, "wait");
        content = fix_deprecated_bare_call(&content, "spawn");
        content = fix_deprecated_bare_call(&content, "delay");

        // Fix missing --!strict for .luau files
        if is_luau {
            let first_line = content.lines().next().unwrap_or("");
            if first_line.trim() != "--!strict" {
                content = format!("--!strict\n{content}");
            }
        }

        if content != original {
            std::fs::write(&path, &content)
                .with_context(|| format!("cannot write {}", path.display()))?;
            *fixed_count += 1;
        }
    }

    Ok(())
}

/// Replace bare `wait(` / `spawn(` / `delay(` calls with `task.wait(` etc.
/// Only replaces occurrences that are NOT preceded by `.` or a word character
/// (to avoid replacing `task.wait(` or `obj.wait(`).
fn fix_deprecated_bare_call(source: &str, func_name: &str) -> String {
    use regex::Regex;
    use std::sync::LazyLock;

    // Build a regex that matches bare calls: start-of-line or non-word/non-dot
    // character followed by the function name and `(`.
    // We capture the prefix so we can preserve it.
    static FIX_WAIT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)(^|[^.\w])wait\s*\(").unwrap());
    static FIX_SPAWN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)(^|[^.\w])spawn\s*\(").unwrap());
    static FIX_DELAY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)(^|[^.\w])delay\s*\(").unwrap());

    let re = match func_name {
        "wait" => &*FIX_WAIT,
        "spawn" => &*FIX_SPAWN,
        "delay" => &*FIX_DELAY,
        _ => return source.to_string(),
    };

    let replacement = format!("${{1}}task.{func_name}(");
    re.replace_all(source, replacement.as_str()).into_owned()
}

// ---------------------------------------------------------------------------
// Individual checks
// ---------------------------------------------------------------------------

fn check_strict_mode(path: &str, lines: &[&str], issues: &mut Vec<ValidationIssue>) {
    if !path.ends_with(".luau") {
        return;
    }
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

#[derive(Debug)]
struct ToolRunResult {
    available: bool,
    success: bool,
    output: String,
}

#[derive(Debug, Clone)]
struct FunctionFrame {
    name: String,
    start_line: usize,
    depth: isize,
    parameter_count: usize,
    local_binding_count: usize,
    nested_closure_count: usize,
}

fn which_tool(name: &str) -> Option<String> {
    Command::new("which").arg(name).output().ok().and_then(|o| {
        if o.status.success() {
            let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !path.is_empty() { Some(path) } else { None }
        } else {
            None
        }
    })
}

fn run_luau_tool(tool_name: &str, args: &[&str], content: &str) -> Result<ToolRunResult> {
    let Some(tool_path) = which_tool(tool_name) else {
        return Ok(ToolRunResult {
            available: false,
            success: false,
            output: String::new(),
        });
    };

    let temp_path = std::env::temp_dir().join(format!(
        "vertigo-sync-plugin-{}-{}.luau",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&temp_path, content)
        .with_context(|| format!("failed to write temp file for {tool_name}"))?;

    let output = Command::new(&tool_path)
        .args(args)
        .arg(&temp_path)
        .output()
        .with_context(|| format!("failed to run {tool_name}"))?;

    let _ = std::fs::remove_file(&temp_path);

    let combined = format!(
        "{}{}{}",
        String::from_utf8_lossy(&output.stdout),
        if output.stdout.is_empty() || output.stderr.is_empty() {
            ""
        } else {
            "\n"
        },
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(ToolRunResult {
        available: true,
        success: output.status.success(),
        output: combined.trim().to_string(),
    })
}

fn first_non_empty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// Find selene on PATH.
fn which_selene() -> Option<String> {
    // Check common aftman install locations first.
    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("LOCALAPPDATA") {
            let path = format!("{appdata}\\aftman\\bin\\selene.exe");
            if Path::new(&path).is_file() {
                return Some(path);
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(home) = std::env::var("HOME") {
            let path = format!("{home}/.aftman/bin/selene");
            if Path::new(&path).is_file() {
                return Some(path);
            }
        }
    }

    // Fall back to PATH lookup via `which` (Unix) or `where` (Windows).
    #[cfg(target_os = "windows")]
    let which_cmd = "where";
    #[cfg(not(target_os = "windows"))]
    let which_cmd = "which";

    Command::new(which_cmd)
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

fn count_plugin_top_level_symbols(source: &str) -> usize {
    let mut count = 0usize;
    let mut depth = 0isize;

    for line in source.lines() {
        let trimmed = strip_line_comment(line).trim();
        if trimmed.is_empty() {
            continue;
        }

        let is_top_level = depth == 0;
        if is_top_level {
            if trimmed.starts_with("type ") || trimmed.starts_with("local function ") {
                count += 1;
            } else if let Some(local_count) = top_level_local_binding_count(trimmed) {
                count += local_count;
            }
        }

        depth += block_depth_delta(trimmed);
        if depth < 0 {
            depth = 0;
        }
    }

    count
}

fn plugin_function_risk_findings(source: &str) -> Vec<PluginFunctionRiskFinding> {
    let mut findings = Vec::new();
    let mut stack: Vec<FunctionFrame> = Vec::new();

    for (index, raw_line) in source.lines().enumerate() {
        let line_no = index + 1;
        let trimmed = strip_line_comment(raw_line).trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some((name, parameter_count)) = parse_function_start(trimmed, line_no) {
            if let Some(parent) = stack.last_mut() {
                parent.nested_closure_count += 1;
            }

            let mut frame = FunctionFrame {
                name,
                start_line: line_no,
                depth: 0,
                parameter_count,
                local_binding_count: 0,
                nested_closure_count: 0,
            };
            frame.depth += block_depth_delta(trimmed);
            if frame.depth <= 0 {
                frame.depth = 1;
            }
            stack.push(frame);
            continue;
        }

        if let Some(frame) = stack.last_mut() {
            frame.local_binding_count += local_binding_count(trimmed);
            frame.depth += block_depth_delta(trimmed);
        }

        while stack.last().is_some_and(|frame| frame.depth <= 0) {
            let frame = stack.pop().expect("frame exists");
            let line_span = line_no.saturating_sub(frame.start_line) + 1;
            let risk_score = frame.parameter_count
                + frame.local_binding_count
                + (frame.nested_closure_count * 12)
                + (line_span / 20);
            findings.push(PluginFunctionRiskFinding {
                name: frame.name,
                start_line: frame.start_line,
                end_line: line_no,
                parameter_count: frame.parameter_count,
                local_binding_count: frame.local_binding_count,
                nested_closure_count: frame.nested_closure_count,
                line_span,
                risk_score,
                hard_fail: risk_score >= PLUGIN_FUNCTION_RISK_HARD_FAIL,
            });
        }
    }

    findings.sort_by(|a, b| b.risk_score.cmp(&a.risk_score));
    findings
}

fn parse_function_start(line: &str, line_no: usize) -> Option<(String, usize)> {
    let name = if let Some(rest) = line.strip_prefix("local function ") {
        rest.split('(').next()?.trim().to_string()
    } else if let Some(rest) = line.strip_prefix("function ") {
        rest.split('(').next()?.trim().to_string()
    } else if let Some((lhs, rhs)) = line.split_once('=') {
        let rhs_trimmed = rhs.trim_start();
        if rhs_trimmed.starts_with("function(") || rhs_trimmed.starts_with("function (") {
            lhs.trim()
                .strip_prefix("local ")
                .unwrap_or(lhs.trim())
                .to_string()
        } else {
            return None;
        }
    } else {
        return None;
    };

    let parameter_count = line
        .split_once('(')
        .and_then(|(_, tail)| tail.split_once(')'))
        .map(|(params, _)| count_parameters(params))
        .unwrap_or(0);

    Some((
        if name.is_empty() {
            format!("<anonymous@{line_no}>")
        } else {
            name
        },
        parameter_count,
    ))
}

fn count_parameters(params: &str) -> usize {
    params
        .split(',')
        .map(str::trim)
        .filter(|param| !param.is_empty())
        .count()
}

fn local_binding_count(line: &str) -> usize {
    if line.starts_with("local function ") {
        return 0;
    }
    top_level_local_binding_count(line).unwrap_or(0)
}

fn top_level_local_binding_count(line: &str) -> Option<usize> {
    if !line.starts_with("local ") || line.starts_with("local function ") {
        return None;
    }

    let body = &line["local ".len()..];
    let stop = body.find('=').unwrap_or(body.len());
    let names = &body[..stop];
    Some(
        names
            .split(',')
            .map(str::trim)
            .map(|name| name.split(':').next().unwrap_or("").trim())
            .filter(|name| !name.is_empty())
            .count(),
    )
}

fn strip_line_comment(line: &str) -> &str {
    if let Some((prefix, _)) = line.split_once("--") {
        prefix
    } else {
        line
    }
}

fn block_depth_delta(line: &str) -> isize {
    let mut delta = 0isize;

    if contains_standalone_keyword(line, "function") {
        delta += count_keyword_occurrences(line, "function") as isize;
    }

    if opens_if_block(line) {
        delta += 1;
    }
    if opens_for_block(line) {
        delta += 1;
    }
    if opens_while_block(line) {
        delta += 1;
    }
    if opens_repeat_block(line) {
        delta += 1;
    }
    if opens_do_block(line) {
        delta += 1;
    }

    delta -= count_keyword_occurrences(line, "end") as isize;
    delta -= count_keyword_occurrences(line, "until") as isize;
    delta
}

fn count_keyword_occurrences(line: &str, keyword: &str) -> usize {
    let mut count = 0usize;
    let mut start = 0usize;
    while let Some(pos) = line[start..].find(keyword) {
        let abs = start + pos;
        if is_keyword_boundary(line, abs, keyword.len()) {
            count += 1;
        }
        start = abs + keyword.len();
    }
    count
}

fn contains_standalone_keyword(line: &str, keyword: &str) -> bool {
    count_keyword_occurrences(line, keyword) > 0
}

fn is_keyword_boundary(line: &str, start: usize, len: usize) -> bool {
    let before = line[..start].chars().next_back();
    let after = line[start + len..].chars().next();
    let before_ok = before.is_none_or(|c| !is_identifier_char(c));
    let after_ok = after.is_none_or(|c| !is_identifier_char(c));
    before_ok && after_ok
}

fn is_identifier_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn opens_if_block(line: &str) -> bool {
    line.starts_with("if ") && line.contains(" then")
}

fn opens_for_block(line: &str) -> bool {
    line.starts_with("for ") && line.contains(" do")
}

fn opens_while_block(line: &str) -> bool {
    line.starts_with("while ") && line.contains(" do")
}

fn opens_repeat_block(line: &str) -> bool {
    line.starts_with("repeat")
}

fn opens_do_block(line: &str) -> bool {
    line == "do"
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
    ignores: &GlobIgnoreSet,
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
            walk_and_validate(&entry.path(), root, ignores, files_checked, issues)?;
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
        if ignores.is_ignored(&rel) {
            continue;
        }

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
        let issues = validate_file_content("src/Server/Small.luau", content);
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
    fn strict_mode_not_required_for_lua_files() {
        let issues = validate_file_content("src/Server/Foo.lua", "local x = 1\nreturn x\n");
        assert!(
            !issues.iter().any(|i| i.rule == RULE_STRICT_MODE),
            "plain .lua files should not be forced into --!strict"
        );
    }

    #[test]
    fn validate_source_honors_ignore_globs() {
        let root = tempdir().expect("tempdir");
        let src = root.path().join("src");
        fs::create_dir_all(src.join("generated")).expect("mkdir generated");
        fs::write(src.join("Good.luau"), "--!strict\nreturn {}\n").expect("write good");
        fs::write(src.join("generated/Legacy.lua"), "return 1\n").expect("write ignored");

        let includes = vec!["src".to_string()];
        let ignores = vec!["src/generated/**".to_string()];
        let report =
            validate_source_with_ignores(root.path(), &includes, &ignores).expect("validate");
        assert!(
            report.clean,
            "ignored generated paths should not contribute errors"
        );
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

    #[test]
    fn plugin_safety_counts_top_level_symbols() {
        let content = "\
--!strict
local A = 1
local B, C = 2, 3
local function helper()
\treturn A + B + C
end

return helper
";
        assert_eq!(count_plugin_top_level_symbols(content), 4);
    }

    #[test]
    fn plugin_safety_flags_register_heavy_function() {
        let content = "\
--!strict
local function tooHeavy(a, b, c, d, e, f, g, h)
\tlocal l01, l02, l03, l04, l05 = 1, 2, 3, 4, 5
\tlocal l06, l07, l08, l09, l10 = 6, 7, 8, 9, 10
\tlocal l11, l12, l13, l14, l15 = 11, 12, 13, 14, 15
\tlocal l16, l17, l18, l19, l20 = 16, 17, 18, 19, 20
\tlocal l21, l22, l23, l24, l25 = 21, 22, 23, 24, 25
\tlocal l26, l27, l28, l29, l30 = 26, 27, 28, 29, 30
\tlocal l31, l32, l33, l34, l35 = 31, 32, 33, 34, 35
\tlocal l36, l37, l38, l39, l40 = 36, 37, 38, 39, 40
\tlocal l41, l42, l43, l44, l45 = 41, 42, 43, 44, 45
\tlocal l46, l47, l48, l49, l50 = 46, 47, 48, 49, 50
\tlocal l51, l52, l53, l54, l55 = 51, 52, 53, 54, 55
\tlocal l56, l57, l58, l59, l60 = 56, 57, 58, 59, 60
\tlocal function nestedOne()
\t\treturn l01 + l02 + l03
\tend
\tlocal function nestedTwo()
\t\treturn nestedOne() + l20
\tend
\treturn nestedTwo() + a + b + c + d + e + f + g + h
end

return tooHeavy
";
        let findings = plugin_function_risk_findings(content);
        assert!(
            findings
                .iter()
                .any(|finding| finding.name == "tooHeavy" && finding.risk_score > 0),
            "expected heavy function to produce a non-zero risk score"
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.name == "tooHeavy" && finding.hard_fail),
            "expected heavy function to exceed the hard-fail threshold"
        );
    }

    #[test]
    fn plugin_safety_accepts_small_function() {
        let content = "\
--!strict
local function small(x)
\tlocal doubled = x * 2
\treturn doubled + 1
end

return small
";
        let findings = plugin_function_risk_findings(content);
        assert!(
            findings.iter().all(|finding| !finding.hard_fail),
            "small helper functions should stay below the hard-fail threshold"
        );
    }
}
