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
            let after_ok = abs + wlen >= hay_bytes.len() || !is_ident_byte(hay_bytes[abs + wlen]);
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

static RE_LOCAL_ASSIGN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*local\s+([A-Za-z_][A-Za-z0-9_]*)\s*=").unwrap());

static RE_COMPLEXITY_KEYWORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:if|elseif|while|for|repeat)\b").unwrap());

static RE_LOGICAL_OPERATOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:and|or)\b").unwrap());

/// Matches `if (expr) then` or `elseif (expr) then` — the outer parens are unnecessary.
static RE_PAREN_CONDITION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)\b(?:if|elseif|while)\s*\((.+)\)\s*(?:then|do)\b").unwrap());

/// Matches Yoda conditions: a literal or nil/true/false on the left side of == or ~=.
static RE_YODA_CONDITION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)\b(?:nil|true|false)\s*(?:==|~=)\s*\w").unwrap());

// NOTE: RE_GLOBAL_SHADOW was identical to RE_LOCAL_ASSIGN; reuse the same static.

static RE_WAIT_DEPRECATED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)(?:^|[^.\w])wait\s*\(").unwrap());

static RE_SPAWN_DEPRECATED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)(?:^|[^.\w])spawn\s*\(").unwrap());

static RE_DELAY_DEPRECATED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)(?:^|[^.\w])delay\s*\(").unwrap());

static RE_EMPTY_THEN_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)\bthen\s*\n\s*end\b").unwrap());

static RE_EMPTY_DO_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)\bdo\s*\n\s*end\b").unwrap());

static RE_RETURN_BREAK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*(?:return\b|break\b|continue\b|error\s*\()").unwrap());

static RE_COMMENT_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*--").unwrap());

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

// ---------------------------------------------------------------------------
// Pre-computed comment map
// ---------------------------------------------------------------------------

/// Pre-compute a per-line boolean map indicating whether each line is inside
/// a comment (line comment or block comment). This avoids re-scanning from the
/// beginning of the source on every comment check.
pub fn build_comment_map(source: &str) -> Vec<bool> {
    let mut in_block = false;
    source
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            if in_block {
                if trimmed.contains("]]") || trimmed.contains("]=]") || trimmed.contains("]==]") {
                    in_block = false;
                }
                true
            } else if trimmed.contains("--[[")
                || trimmed.contains("--[=[")
                || trimmed.contains("--[==[")
            {
                // Check if the block also closes on this same line
                let after_open = if trimmed.contains("--[==[") {
                    trimmed.find("--[==[").map(|i| &trimmed[i + 6..])
                } else if trimmed.contains("--[=[") {
                    trimmed.find("--[=[").map(|i| &trimmed[i + 5..])
                } else {
                    trimmed.find("--[[").map(|i| &trimmed[i + 4..])
                };
                if let Some(rest) = after_open {
                    if rest.contains("]]") || rest.contains("]=]") || rest.contains("]==]") {
                        // Opens and closes on same line — line has comment but block doesn't persist
                        return true;
                    }
                }
                in_block = true;
                true
            } else {
                trimmed.starts_with("--")
            }
        })
        .collect()
}

/// Check whether a byte offset falls on a comment line according to the
/// pre-computed comment map.
fn is_comment_line(source: &str, byte_offset: usize, comment_map: &[bool]) -> bool {
    let line_idx = source[..byte_offset].matches('\n').count();
    comment_map.get(line_idx).copied().unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Rule implementations
// ---------------------------------------------------------------------------

/// **unused-variable** -- `local x = ...` where `x` never appears again.
/// Skips `_`-prefixed names and function declarations (`local function`).
pub fn check_unused_variable(source: &str, file: &str, comment_map: &[bool]) -> Vec<LintIssue> {
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
        if is_comment_line(source, full_match.start(), comment_map) {
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
pub fn check_global_shadow(source: &str, file: &str, comment_map: &[bool]) -> Vec<LintIssue> {
    let mut issues = Vec::new();

    for cap in RE_LOCAL_ASSIGN.captures_iter(source) {
        let name = cap.get(1).unwrap();
        let var = name.as_str();

        if is_comment_line(source, cap.get(0).unwrap().start(), comment_map) {
            continue;
        }

        if ROBLOX_GLOBALS.contains(&var) {
            // Skip self-aliasing pattern: `local string = string` (common perf idiom)
            let after_eq = source[cap.get(0).unwrap().end()..].trim_start();
            // Extract the first token after `=` to compare with var_name
            let rhs_token_end = after_eq
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(after_eq.len());
            let rhs_token = &after_eq[..rhs_token_end];
            if rhs_token == var {
                continue; // self-aliasing for performance, not a true shadow
            }

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
pub fn check_wait_deprecated(source: &str, file: &str, comment_map: &[bool]) -> Vec<LintIssue> {
    check_deprecated_call(
        source,
        file,
        &RE_WAIT_DEPRECATED,
        "wait",
        "wait-deprecated",
        comment_map,
    )
}

/// **spawn-deprecated** -- bare `spawn(` pattern.
pub fn check_spawn_deprecated(source: &str, file: &str, comment_map: &[bool]) -> Vec<LintIssue> {
    check_deprecated_call(
        source,
        file,
        &RE_SPAWN_DEPRECATED,
        "spawn",
        "spawn-deprecated",
        comment_map,
    )
}

/// **delay-deprecated** -- bare `delay(` pattern.
pub fn check_delay_deprecated(source: &str, file: &str, comment_map: &[bool]) -> Vec<LintIssue> {
    check_deprecated_call(
        source,
        file,
        &RE_DELAY_DEPRECATED,
        "delay",
        "delay-deprecated",
        comment_map,
    )
}

fn check_deprecated_call(
    source: &str,
    file: &str,
    re: &Regex,
    func_name: &str,
    rule_name: &str,
    comment_map: &[bool],
) -> Vec<LintIssue> {
    let mut issues = Vec::new();

    for m in re.find_iter(source) {
        if is_comment_line(source, m.start(), comment_map) {
            continue;
        }
        let (line, col) = line_col_at(source, m.start());
        issues.push(LintIssue {
            rule: rule_name.into(),
            severity: LintSeverity::Warning,
            message: format!("`{func_name}()` is deprecated; use `task.{func_name}()` instead"),
            line,
            column: col,
            file: file.into(),
        });
    }

    issues
}

/// **empty-block** -- `then\n\s*end` or `do\n\s*end` patterns.
pub fn check_empty_block(source: &str, file: &str, comment_map: &[bool]) -> Vec<LintIssue> {
    let mut issues = Vec::new();

    for re in [&*RE_EMPTY_THEN_END, &*RE_EMPTY_DO_END] {
        for m in re.find_iter(source) {
            if is_comment_line(source, m.start(), comment_map) {
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
pub fn check_unreachable_code(source: &str, file: &str, comment_map: &[bool]) -> Vec<LintIssue> {
    let mut issues = Vec::new();
    let lines: Vec<&str> = source.lines().collect();

    // Pre-compute byte offsets for each line start (I11 fix: avoids O(n^2)).
    let line_offsets = build_line_offsets(source);

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        // Skip comment lines.
        if RE_COMMENT_LINE.is_match(line) {
            i += 1;
            continue;
        }
        if RE_RETURN_BREAK.is_match(line) && !comment_map.get(i).copied().unwrap_or(false) {
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
                        message:
                            "code after unconditional return/break/continue/error() is unreachable"
                                .into(),
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

// ---------------------------------------------------------------------------
// Complexity lint rules (P2)
// ---------------------------------------------------------------------------

/// Default threshold for function-length rule (lines).
const DEFAULT_FUNCTION_LENGTH: usize = 100;
/// Default threshold for nesting-depth rule (levels).
const DEFAULT_NESTING_DEPTH: usize = 5;
/// Default threshold for cyclomatic-complexity rule (branches).
const DEFAULT_CYCLOMATIC_COMPLEXITY: usize = 10;

/// Track Lua/Luau block depth changes on a single line.
/// Returns the net change in depth (positive for openers, negative for `end`).
fn line_depth_delta(trimmed: &str) -> i32 {
    let mut delta: i32 = 0;

    // Block openers: function, if...then, for...do, while...do, repeat, do
    let is_opener = trimmed.starts_with("function ")
        || trimmed.starts_with("local function ")
        || trimmed.contains("= function(")
        || trimmed.contains("= function (")
        || ((trimmed.starts_with("if ") || trimmed.starts_with("elseif "))
            && trimmed.contains("then")
            && !trimmed.contains("end"))
        || ((trimmed.starts_with("for ") || trimmed.starts_with("while "))
            && trimmed.ends_with("do"))
        || trimmed == "repeat"
        || (trimmed.starts_with("repeat") && !trimmed.contains("until"))
        || trimmed == "do";

    if is_opener {
        delta += 1;
    }

    // Block closers
    let is_closer = trimmed == "end"
        || trimmed == "end)"
        || trimmed == "end,"
        || trimmed.starts_with("end)")
        || trimmed.starts_with("end,")
        || trimmed.starts_with("end ")
        || trimmed.starts_with("until ")
        || trimmed == "until";

    if is_closer {
        delta -= 1;
    }

    delta
}

/// Heuristic function span extractor.
/// Returns `(name, start_line_0based, end_line_0based)` tuples.
fn extract_function_spans(source: &str, comment_map: &[bool]) -> Vec<(String, usize, usize)> {
    let lines: Vec<&str> = source.lines().collect();
    let mut spans = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        if comment_map.get(i).copied().unwrap_or(false) {
            i += 1;
            continue;
        }
        let trimmed = lines[i].trim();
        let is_fn = trimmed.starts_with("function ")
            || trimmed.starts_with("local function ")
            || trimmed.contains("= function(")
            || trimmed.contains("= function (");

        if is_fn {
            // Extract function name heuristically
            let name = if let Some(rest) = trimmed.strip_prefix("local function ") {
                rest.split('(').next().unwrap_or("anonymous").trim()
            } else if let Some(rest) = trimmed.strip_prefix("function ") {
                rest.split('(').next().unwrap_or("anonymous").trim()
            } else if let Some(eq_pos) = trimmed.find("= function") {
                trimmed[..eq_pos].trim().trim_start_matches("local ").trim()
            } else {
                "anonymous"
            };

            let fn_start = i;
            let mut depth: i32 = 1;
            let mut j = i + 1;
            while j < lines.len() && depth > 0 {
                if !comment_map.get(j).copied().unwrap_or(false) {
                    let t = lines[j].trim();
                    depth += line_depth_delta(t);
                }
                j += 1;
            }
            spans.push((name.to_string(), fn_start, j.saturating_sub(1)));
            i = j;
        } else {
            i += 1;
        }
    }

    spans
}

/// **function-length** -- warn if a function body exceeds the configured line limit.
pub fn check_function_length(source: &str, file: &str, comment_map: &[bool]) -> Vec<LintIssue> {
    check_function_length_with_threshold(source, file, comment_map, DEFAULT_FUNCTION_LENGTH)
}

/// Inner implementation with configurable threshold (for testing).
pub fn check_function_length_with_threshold(
    source: &str,
    file: &str,
    comment_map: &[bool],
    max_lines: usize,
) -> Vec<LintIssue> {
    let mut issues = Vec::new();
    let spans = extract_function_spans(source, comment_map);
    let line_offsets = build_line_offsets(source);

    for (name, start, end) in &spans {
        let body_lines = end.saturating_sub(*start) + 1;
        if body_lines > max_lines {
            let byte_off = line_offsets.get(*start).copied().unwrap_or(0);
            let (line, col) = line_col_at(source, byte_off);
            issues.push(LintIssue {
                rule: "function-length".into(),
                severity: LintSeverity::Warning,
                message: format!(
                    "function `{name}` is {body_lines} lines long (limit: {max_lines})"
                ),
                line,
                column: col,
                file: file.into(),
            });
        }
    }

    issues
}

/// **nesting-depth** -- warn if nesting exceeds the configured depth limit.
pub fn check_nesting_depth(source: &str, file: &str, comment_map: &[bool]) -> Vec<LintIssue> {
    check_nesting_depth_with_threshold(source, file, comment_map, DEFAULT_NESTING_DEPTH)
}

/// Inner implementation with configurable threshold.
pub fn check_nesting_depth_with_threshold(
    source: &str,
    file: &str,
    comment_map: &[bool],
    max_depth: usize,
) -> Vec<LintIssue> {
    let mut issues = Vec::new();
    let lines: Vec<&str> = source.lines().collect();
    let line_offsets = build_line_offsets(source);
    let spans = extract_function_spans(source, comment_map);

    for (name, start, end) in &spans {
        let mut depth: i32 = 0;
        let mut max_seen: i32 = 0;
        let mut max_line_idx = *start;

        for idx in *start..=*end {
            if idx >= lines.len() {
                break;
            }
            if comment_map.get(idx).copied().unwrap_or(false) {
                continue;
            }
            let trimmed = lines[idx].trim();
            let delta = line_depth_delta(trimmed);

            if delta > 0 {
                depth += delta;
                if depth > max_seen {
                    max_seen = depth;
                    max_line_idx = idx;
                }
            } else if delta < 0 {
                depth += delta;
            }
        }

        if max_seen as usize > max_depth {
            let byte_off = line_offsets.get(max_line_idx).copied().unwrap_or(0);
            let (line, col) = line_col_at(source, byte_off);
            issues.push(LintIssue {
                rule: "nesting-depth".into(),
                severity: LintSeverity::Warning,
                message: format!(
                    "function `{name}` has nesting depth {max_seen} (limit: {max_depth})"
                ),
                line,
                column: col,
                file: file.into(),
            });
        }
    }

    issues
}

/// **parentheses-condition** -- unnecessary parentheses around `if (x) then` conditions.
pub fn check_parentheses_condition(
    source: &str,
    file: &str,
    comment_map: &[bool],
) -> Vec<LintIssue> {
    let mut issues = Vec::new();

    for m in RE_PAREN_CONDITION.find_iter(source) {
        if is_comment_line(source, m.start(), comment_map) {
            continue;
        }

        // Check if the parentheses are actually wrapping the entire condition,
        // not a function call like `if foo(x) then`
        let matched = m.as_str();

        // Extract the keyword to find where the paren starts
        let paren_start = matched.find('(');
        if let Some(ps) = paren_start {
            // Check that the character before '(' is whitespace (not an identifier char),
            // which means it's `if (expr)` not `if func(expr)`
            let before_paren = &matched[..ps];
            let before_trimmed = before_paren.trim_end();
            if before_trimmed.ends_with("if")
                || before_trimmed.ends_with("elseif")
                || before_trimmed.ends_with("while")
            {
                let (line, col) = line_col_at(source, m.start());
                issues.push(LintIssue {
                    rule: "parentheses-condition".into(),
                    severity: LintSeverity::Warning,
                    message: "unnecessary parentheses around condition".into(),
                    line,
                    column: col,
                    file: file.into(),
                });
            }
        }
    }

    issues
}

/// **comparison-order** -- Yoda conditions like `nil == x` instead of `x == nil`.
pub fn check_comparison_order(source: &str, file: &str, comment_map: &[bool]) -> Vec<LintIssue> {
    let mut issues = Vec::new();

    for m in RE_YODA_CONDITION.find_iter(source) {
        if is_comment_line(source, m.start(), comment_map) {
            continue;
        }

        let (line, col) = line_col_at(source, m.start());
        let matched = m.as_str();
        issues.push(LintIssue {
            rule: "comparison-order".into(),
            severity: LintSeverity::Warning,
            message: format!("Yoda condition `{matched}` — prefer the variable on the left side"),
            line,
            column: col,
            file: file.into(),
        });
    }

    issues
}

/// **cyclomatic-complexity** -- warn if a function has too many branches.
pub fn check_cyclomatic_complexity(
    source: &str,
    file: &str,
    comment_map: &[bool],
) -> Vec<LintIssue> {
    check_cyclomatic_complexity_with_threshold(
        source,
        file,
        comment_map,
        DEFAULT_CYCLOMATIC_COMPLEXITY,
    )
}

/// Inner implementation with configurable threshold.
pub fn check_cyclomatic_complexity_with_threshold(
    source: &str,
    file: &str,
    comment_map: &[bool],
    max_complexity: usize,
) -> Vec<LintIssue> {
    let mut issues = Vec::new();
    let lines: Vec<&str> = source.lines().collect();
    let line_offsets = build_line_offsets(source);
    let spans = extract_function_spans(source, comment_map);

    for (name, start, end) in &spans {
        // Start at 1 (the function itself is one path)
        let mut complexity: usize = 1;

        for idx in *start..=*end {
            if idx >= lines.len() {
                break;
            }
            if comment_map.get(idx).copied().unwrap_or(false) {
                continue;
            }
            let line = lines[idx];
            complexity += RE_COMPLEXITY_KEYWORD.find_iter(line).count();
            complexity += RE_LOGICAL_OPERATOR.find_iter(line).count();
        }

        if complexity > max_complexity {
            let byte_off = line_offsets.get(*start).copied().unwrap_or(0);
            let (line, col) = line_col_at(source, byte_off);
            issues.push(LintIssue {
                rule: "cyclomatic-complexity".into(),
                severity: LintSeverity::Warning,
                message: format!(
                    "function `{name}` has cyclomatic complexity {complexity} (limit: {max_complexity})"
                ),
                line,
                column: col,
                file: file.into(),
            });
        }
    }

    issues
}
