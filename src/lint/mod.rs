//! Configurable lint system for Luau/Lua source files.
//!
//! This module is additive — it does NOT replace `validate.rs`. Users control
//! which rules are active via the `[lint]` table in `vsync.toml`.

pub mod rules;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single lint finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintIssue {
    pub rule: String,
    pub severity: LintSeverity,
    pub message: String,
    pub line: usize,
    pub column: usize,
    pub file: String,
}

/// Severity level of a lint issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintSeverity {
    Error,
    Warning,
}

impl std::fmt::Display for LintSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LintSeverity::Error => write!(f, "error"),
            LintSeverity::Warning => write!(f, "warning"),
        }
    }
}

// ---------------------------------------------------------------------------
// Rule registry
// ---------------------------------------------------------------------------

type RuleFn = fn(&str, &str, &[bool]) -> Vec<LintIssue>;

/// All built-in rules with their identifiers and check functions.
const BUILTIN_RULES: &[(&str, RuleFn)] = &[
    ("unused-variable", rules::check_unused_variable),
    ("global-shadow", rules::check_global_shadow),
    ("wait-deprecated", rules::check_wait_deprecated),
    ("spawn-deprecated", rules::check_spawn_deprecated),
    ("delay-deprecated", rules::check_delay_deprecated),
    ("empty-block", rules::check_empty_block),
    ("unreachable-code", rules::check_unreachable_code),
    ("function-length", rules::check_function_length),
    ("nesting-depth", rules::check_nesting_depth),
    (
        "cyclomatic-complexity",
        rules::check_cyclomatic_complexity,
    ),
    (
        "parentheses-condition",
        rules::check_parentheses_condition,
    ),
    ("comparison-order", rules::check_comparison_order),
];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Lint a single source string.
///
/// `rule_config` maps rule names to `"off"`, `"warn"`, or `"error"`.
/// Rules not mentioned in the config default to `"warn"`.
pub fn lint_source(
    source: &str,
    file: &str,
    rule_config: &BTreeMap<String, String>,
) -> Vec<LintIssue> {
    let comment_map = rules::build_comment_map(source);
    let mut issues = Vec::new();

    for &(name, check_fn) in BUILTIN_RULES {
        let config_value = rule_config
            .get(name)
            .map(|s| s.as_str())
            .unwrap_or("warn");

        if config_value == "off" {
            continue;
        }

        let severity_override = match config_value {
            "error" => Some(LintSeverity::Error),
            "warn" => Some(LintSeverity::Warning),
            _ => None, // unknown config value — keep rule default
        };

        let mut rule_issues = check_fn(source, file, &comment_map);

        if let Some(sev) = severity_override {
            for issue in &mut rule_issues {
                issue.severity = sev;
            }
        }

        issues.extend(rule_issues);
    }

    issues
}

/// Lint all `.lua` and `.luau` files under the given include roots.
///
/// `root` is the project root directory. `includes` are relative directory
/// names within the root (e.g. `["src"]`).
///
/// File I/O and linting are parallelised across files using rayon.
pub fn lint_source_tree(
    root: &Path,
    includes: &[String],
    rule_config: &BTreeMap<String, String>,
) -> Vec<LintIssue> {
    lint_source_tree_with_ignores(root, includes, rule_config, &[])
}

/// Lint all `.lua` and `.luau` files under the given include roots,
/// skipping any file whose relative path matches one of the `ignore_patterns`.
pub fn lint_source_tree_with_ignores(
    root: &Path,
    includes: &[String],
    rule_config: &BTreeMap<String, String>,
    ignore_patterns: &[glob::Pattern],
) -> Vec<LintIssue> {
    // 1. Collect all file paths sequentially.
    let mut all_files: Vec<PathBuf> = Vec::new();
    for include in includes {
        let dir = root.join(include);
        if dir.is_dir() {
            collect_file_paths(&dir, &mut all_files);
        }
    }

    // Filter out files matching ignore patterns.
    if !ignore_patterns.is_empty() {
        all_files.retain(|path| {
            let rel = path.strip_prefix(root).unwrap_or(path);
            let rel_str = rel.to_string_lossy();
            !ignore_patterns.iter().any(|p| p.matches(&rel_str))
        });
    }

    // 2. Read + lint each file in parallel.
    all_files
        .par_iter()
        .flat_map(|path| {
            let rel = path.strip_prefix(root).unwrap_or(path);
            let display_path = rel.display().to_string();
            match std::fs::read_to_string(path) {
                Ok(source) => lint_source(&source, &display_path, rule_config),
                Err(e) => vec![LintIssue {
                    rule: "io-error".to_string(),
                    severity: LintSeverity::Warning,
                    message: format!("could not read file: {e}"),
                    line: 0,
                    column: 0,
                    file: display_path,
                }],
            }
        })
        .collect()
}

/// Recursively collect `.lua` and `.luau` file paths, skipping common
/// non-source directories.
fn collect_file_paths(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if matches!(
                name,
                ".git" | "node_modules" | "Packages" | "target" | "dist" | "build"
            ) || name.starts_with('.')
            {
                continue;
            }
            collect_file_paths(&path, out);
        } else if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext == "lua" || ext == "luau" {
                out.push(path);
            }
        }
    }
}
