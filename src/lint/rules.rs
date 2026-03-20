//! Built-in lint rules implemented with regex-based heuristics.
//!
//! Each rule function scans source text and returns a vector of [`LintIssue`]s.

use super::{LintIssue, LintSeverity};
use regex::Regex;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Word-boundary helper (avoids compiling a regex per variable)
// ---------------------------------------------------------------------------

/// Check whether `word` appears as a whole-word (surrounded by non-identifier
/// characters) anywhere in `haystack`.
fn contains_word(haystack: &str, word: &str) -> bool {
    // Fast-path: if the literal doesn't appear at all, skip the boundary check.
    if !haystack.contains(word) {
        return false;
    }

    let word_bytes = word.as_bytes();
    let hay_bytes = haystack.as_bytes();
    let wlen = word_bytes.len();

    let mut start = 0;
    while start + wlen <= hay_bytes.len() {
        if let Some(pos) = haystack[start..].find(word) {
            let abs = start + pos;
            let before_ok = abs == 0 || !is_ident_byte(hay_bytes[abs - 1]);
            let after_ok =
                abs + wlen >= hay_bytes.len() || !is_ident_byte(hay_bytes[abs + wlen]);
            if before_ok && after_ok {
                return true;
            }
            start = abs + 1;
        } else {
            break;
        }
    }
    false
}

#[inline]
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

// ---------------------------------------------------------------------------
// Compiled regexes (initialized once)
// ---------------------------------------------------------------------------

static RE_LOCAL_ASSIGN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*local\s+([A-Za-z_][A-Za-z0-9_]*)\s*=").unwrap()
});

// NOTE: RE_GLOBAL_SHADOW was identical to RE_LOCAL_ASSIGN; reuse the same static.

static RE_WAIT_DEPRECATED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)(?:^|[^.\w])wait\s*\(").unwrap()
});

static RE_SPAWN_DEPRECATED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)(?:^|[^.\w])spawn\s*\(").unwrap()
});

static RE_DELAY_DEPRECATED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)(?:^|[^.\w])delay\s*\(").unwrap()
});

static RE_EMPTY_THEN_END: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)\bthen\s*\n\s*end\b").unwrap()
});

static RE_EMPTY_DO_END: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)\bdo\s*\n\s*end\b").unwrap()
});

static RE_RETURN_BREAK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*(?:return\b|break\b|continue\b|error\s*\()").unwrap()
});

static RE_COMMENT_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*--").unwrap()
});

// Roblox globals that should not be shadowed.
const ROBLOX_GLOBALS: &[&str] = &[
    "game",
    "workspace",
    "script",
    "plugin",
    "Instance",
    "Vector3",
    "Vector2",
    "CFrame",
    "Color3",
    "UDim",
    "UDim2",
    "Enum",
    "math",
    "string",
    "table",
    "task",
    "typeof",
    "type",
    "pcall",
    "xpcall",
    "require",
    "print",
    "warn",
    "error",
    "assert",
    "select",
    "pairs",
    "ipairs",
    "next",
    "rawget",
    "rawset",
    "rawequal",
    "rawlen",
    "setmetatable",
    "getmetatable",
    "tonumber",
    "tostring",
    "coroutine",
    "os",
    "tick",
    "time",
    "utf8",
    "bit32",
    "debug",
    "buffer",
];

// ---------------------------------------------------------------------------
// Helper: line number from byte offset
// ---------------------------------------------------------------------------

fn line_col_at(source: &str, byte_offset: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;
    for (i, ch) in source.char_indices() {
        if i >= byte_offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Returns `true` if the byte offset falls inside a `--` line comment or
/// a `--[[ ... ]]` block comment.
///
/// # Known limitations
///
/// - Block comment detection is approximate: it counts `--[[` / `--[=[` opens
///   and `]]` / `]=]` closes before the offset. Nested or unbalanced delimiters
///   may produce incorrect results.
/// - String contents containing `--` may cause false positives for the
///   line-comment heuristic (e.g. `local s = "foo -- bar"`).
/// - These are accepted trade-offs of the regex/heuristic-based approach used
///   throughout this module.
fn is_in_comment(source: &str, byte_offset: usize) -> bool {
    // Check if the line containing this offset is a line comment.
    let line_start = source[..byte_offset]
        .rfind('\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    let line_prefix = &source[line_start..byte_offset];
    if line_prefix.contains("--") {
        // The `--` appears before our match on the same line.
        return true;
    }

    // Rough block-comment check: count `--[[` and `]]` pairs before offset.
    let before = &source[..byte_offset];
    let opens = before.matches("--[[").count() + before.matches("--[=[").count();
    let closes = before.matches("]]").count() + before.matches("]=]").count();
    opens > closes
}

// ---------------------------------------------------------------------------
// Rule implementations
// ---------------------------------------------------------------------------

/// **unused-variable** -- `local x = ...` where `x` never appears again.
/// Skips `_`-prefixed names and function declarations (`local function`).
pub fn check_unused_variable(source: &str, file: &str) -> Vec<LintIssue> {
    let mut issues = Vec::new();

    for cap in RE_LOCAL_ASSIGN.captures_iter(source) {
        let name = cap.get(1).unwrap();
        let var = name.as_str();

        // Skip _ prefixed names.
        if var.starts_with('_') {
            continue;
        }

        // Skip function declarations (handled separately from assignments).
        let full_match = cap.get(0).unwrap();
        let before_eq = &source[full_match.start()..name.end()];
        if before_eq.contains("function") {
            continue;
        }

        // Skip if inside a comment.
        if is_in_comment(source, full_match.start()) {
            continue;
        }

        // Check whether the variable name appears as a whole word after the
        // assignment. Uses byte-level word-boundary checks instead of regex.
        let after_assignment = &source[full_match.end()..];
        if !contains_word(after_assignment, var) {
            let (line, col) = line_col_at(source, name.start());
            issues.push(LintIssue {
                rule: "unused-variable".into(),
                severity: LintSeverity::Warning,
                message: format!("variable `{var}` is assigned but never used"),
                line,
                column: col,
                file: file.into(),
            });
        }
    }

    issues
}

/// **global-shadow** -- `local game = ...` where `game` is a Roblox global.
pub fn check_global_shadow(source: &str, file: &str) -> Vec<LintIssue> {
    let mut issues = Vec::new();

    for cap in RE_LOCAL_ASSIGN.captures_iter(source) {
        let name = cap.get(1).unwrap();
        let var = name.as_str();

        if is_in_comment(source, cap.get(0).unwrap().start()) {
            continue;
        }

        if ROBLOX_GLOBALS.contains(&var) {
            let (line, col) = line_col_at(source, name.start());
            issues.push(LintIssue {
                rule: "global-shadow".into(),
                severity: LintSeverity::Warning,
                message: format!("local `{var}` shadows Roblox global `{var}`"),
                line,
                column: col,
                file: file.into(),
            });
        }
    }

    issues
}

/// **wait-deprecated** -- bare `wait(` not preceded by `.` or word char.
pub fn check_wait_deprecated(source: &str, file: &str) -> Vec<LintIssue> {
    check_deprecated_call(source, file, &RE_WAIT_DEPRECATED, "wait", "wait-deprecated")
}

/// **spawn-deprecated** -- bare `spawn(` pattern.
pub fn check_spawn_deprecated(source: &str, file: &str) -> Vec<LintIssue> {
    check_deprecated_call(source, file, &RE_SPAWN_DEPRECATED, "spawn", "spawn-deprecated")
}

/// **delay-deprecated** -- bare `delay(` pattern.
pub fn check_delay_deprecated(source: &str, file: &str) -> Vec<LintIssue> {
    check_deprecated_call(source, file, &RE_DELAY_DEPRECATED, "delay", "delay-deprecated")
}

fn check_deprecated_call(
    source: &str,
    file: &str,
    re: &Regex,
    func_name: &str,
    rule_name: &str,
) -> Vec<LintIssue> {
    let mut issues = Vec::new();

    for m in re.find_iter(source) {
        if is_in_comment(source, m.start()) {
            continue;
        }
        let (line, col) = line_col_at(source, m.start());
        issues.push(LintIssue {
            rule: rule_name.into(),
            severity: LintSeverity::Warning,
            message: format!(
                "`{func_name}()` is deprecated; use `task.{func_name}()` instead"
            ),
            line,
            column: col,
            file: file.into(),
        });
    }

    issues
}

/// **empty-block** -- `then\n\s*end` or `do\n\s*end` patterns.
pub fn check_empty_block(source: &str, file: &str) -> Vec<LintIssue> {
    let mut issues = Vec::new();

    for re in [&*RE_EMPTY_THEN_END, &*RE_EMPTY_DO_END] {
        for m in re.find_iter(source) {
            if is_in_comment(source, m.start()) {
                continue;
            }
            let (line, col) = line_col_at(source, m.start());
            issues.push(LintIssue {
                rule: "empty-block".into(),
                severity: LintSeverity::Warning,
                message: "empty block body".into(),
                line,
                column: col,
                file: file.into(),
            });
        }
    }

    issues
}

/// **unreachable-code** -- code after unconditional return/break/continue/error().
///
/// Uses indentation as a scope heuristic: only flags the next meaningful line
/// as unreachable if it has the **same or deeper** indentation as the terminal
/// statement. Lines with less indentation are assumed to belong to an outer
/// scope (e.g. code after `end` closing an `if` block).
pub fn check_unreachable_code(source: &str, file: &str) -> Vec<LintIssue> {
    let mut issues = Vec::new();
    let lines: Vec<&str> = source.lines().collect();

    // Pre-compute byte offsets for each line start (I11 fix: avoids O(n²)).
    let line_offsets = build_line_offsets(source);

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        // Skip comment lines.
        if RE_COMMENT_LINE.is_match(line) {
            i += 1;
            continue;
        }
        if RE_RETURN_BREAK.is_match(line) && !is_in_comment(source, line_offsets[i]) {
            let terminal_indent = indent_level(line);

            // Look at the next non-empty, non-comment line.
            let mut j = i + 1;
            while j < lines.len() {
                let next = lines[j].trim();
                if next.is_empty() || next.starts_with("--") {
                    j += 1;
                    continue;
                }
                break;
            }
            if j < lines.len() {
                let next_line = lines[j];
                let next_trimmed = next_line.trim();
                let next_indent = indent_level(next_line);

                // `end`, `else`, `elseif`, `until` are structural — not unreachable.
                let is_structural = next_trimmed.starts_with("end")
                    || next_trimmed.starts_with("else")
                    || next_trimmed.starts_with("elseif")
                    || next_trimmed.starts_with("until")
                    || next_trimmed.starts_with("}");

                // Only flag if the next line is at the same or deeper indent
                // (i.e. same scope or nested). Lines at shallower indent are
                // likely in an outer scope after an `end`.
                if !is_structural && next_indent >= terminal_indent {
                    let byte_off = line_offsets[j];
                    let (line_num, col) = line_col_at(source, byte_off);
                    issues.push(LintIssue {
                        rule: "unreachable-code".into(),
                        severity: LintSeverity::Warning,
                        message: "code after unconditional return/break/continue/error() is unreachable".into(),
                        line: line_num,
                        column: col,
                        file: file.into(),
                    });
                }
            }
        }
        i += 1;
    }

    issues
}

/// Count leading whitespace characters (spaces/tabs) as the indentation level.
fn indent_level(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Pre-compute a table of byte offsets for each line start.
/// `result[i]` is the byte offset where line `i` (0-based) begins.
fn build_line_offsets(source: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    let mut pos = 0;
    for line in source.lines() {
        pos += line.len();
        // Skip past the actual line terminator (\r\n or \n or \r)
        if source[pos..].starts_with("\r\n") {
            pos += 2;
        } else if source[pos..].starts_with('\n') || source[pos..].starts_with('\r') {
            pos += 1;
        }
        offsets.push(pos);
    }
    offsets
}
