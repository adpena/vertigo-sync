//! Configurable lint system for Luau/Lua source files.
//!
//! This module is additive — it does NOT replace `validate.rs`. Users control
//! which rules are active via the `[lint]` table in `vsync.toml`.

pub mod rules;

use std::collections::BTreeMap;
use std::path::Path;

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

type RuleFn = fn(&str, &str) -> Vec<LintIssue>;

/// All built-in rules with their identifiers and check functions.
const BUILTIN_RULES: &[(&str, RuleFn)] = &[
    ("unused-variable", rules::check_unused_variable),
    ("global-shadow", rules::check_global_shadow),
    ("wait-deprecated", rules::check_wait_deprecated),
    ("spawn-deprecated", rules::check_spawn_deprecated),
    ("delay-deprecated", rules::check_delay_deprecated),
    ("empty-block", rules::check_empty_block),
    ("unreachable-code", rules::check_unreachable_code),
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

        let mut rule_issues = check_fn(source, file);

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
pub fn lint_source_tree(
    root: &Path,
    includes: &[String],
    rule_config: &BTreeMap<String, String>,
) -> Vec<LintIssue> {
    let mut issues = Vec::new();

    for include in includes {
        let dir = root.join(include);
        if !dir.is_dir() {
            continue;
        }
        collect_lint_issues(&dir, root, rule_config, &mut issues);
    }

    issues
}

// TODO: extract shared walk_lua_files utility (see also fmt.rs)
fn collect_lint_issues(
    dir: &Path,
    root: &Path,
    rule_config: &BTreeMap<String, String>,
    issues: &mut Vec<LintIssue>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_lint_issues(&path, root, rule_config, issues);
        } else if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext == "lua" || ext == "luau" {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path);
                match std::fs::read_to_string(&path) {
                    Ok(source) => {
                        let display_path = rel.display().to_string();
                        issues.extend(lint_source(&source, &display_path, rule_config));
                    }
                    Err(e) => {
                        issues.push(LintIssue {
                            rule: "io-error".to_string(),
                            severity: LintSeverity::Warning,
                            message: format!("could not read file: {e}"),
                            line: 0,
                            column: 0,
                            file: rel.to_string_lossy().to_string(),
                        });
                    }
                }
            }
        }
    }
}
