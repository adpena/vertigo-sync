//! Built-in Luau source formatter powered by StyLua.
//!
//! Maps the project-level [`FormatConfig`](crate::config::FormatConfig) into a
//! StyLua [`Config`](stylua_lib::Config) and exposes helpers for formatting single
//! sources, checking whether a file would change, and formatting files in-place.

use anyhow::{Context, Result};
use std::path::Path;

use crate::config::FormatConfig;

// ---------------------------------------------------------------------------
// Config mapping
// ---------------------------------------------------------------------------

/// Build a StyLua [`Config`] from the project-level [`FormatConfig`].
fn build_stylua_config(fc: &FormatConfig) -> stylua_lib::Config {
    #[allow(deprecated)]
    let mut cfg = stylua_lib::Config {
        syntax: stylua_lib::LuaVersion::Luau,
        ..stylua_lib::Config::default()
    };

    if let Some(ref it) = fc.indent_type {
        match it.to_lowercase().as_str() {
            "tabs" | "tab" => cfg.indent_type = stylua_lib::IndentType::Tabs,
            "spaces" | "space" => cfg.indent_type = stylua_lib::IndentType::Spaces,
            _ => {}
        }
    }

    if let Some(w) = fc.indent_width {
        cfg.indent_width = w as usize;
    }

    if let Some(w) = fc.line_width {
        cfg.column_width = w as usize;
    }

    if let Some(ref qs) = fc.quote_style {
        match qs.to_lowercase().as_str() {
            "single" => cfg.quote_style = stylua_lib::QuoteStyle::ForceSingle,
            "double" => cfg.quote_style = stylua_lib::QuoteStyle::ForceDouble,
            "auto" | "autoprefer" | "autopreferdouble" => {
                cfg.quote_style = stylua_lib::QuoteStyle::AutoPreferDouble
            }
            "autoprefersingle" => cfg.quote_style = stylua_lib::QuoteStyle::AutoPreferSingle,
            _ => {}
        }
    }

    if let Some(ref cp) = fc.call_parentheses {
        match cp.to_lowercase().as_str() {
            "always" => cfg.call_parentheses = stylua_lib::CallParenType::Always,
            "nosinglestring" | "no_single_string" | "no-single-string" => {
                cfg.call_parentheses = stylua_lib::CallParenType::NoSingleString
            }
            "nosingletable" | "no_single_table" | "no-single-table" => {
                cfg.call_parentheses = stylua_lib::CallParenType::NoSingleTable
            }
            "none" => cfg.call_parentheses = stylua_lib::CallParenType::None,
            "input" => cfg.call_parentheses = stylua_lib::CallParenType::Input,
            _ => {}
        }
    }

    if let Some(ref css) = fc.collapse_simple_statement {
        match css.to_lowercase().as_str() {
            "never" => cfg.collapse_simple_statement = stylua_lib::CollapseSimpleStatement::Never,
            "functiononly" | "function_only" | "function-only" => {
                cfg.collapse_simple_statement = stylua_lib::CollapseSimpleStatement::FunctionOnly
            }
            "conditionalonly" | "conditional_only" | "conditional-only" => {
                cfg.collapse_simple_statement = stylua_lib::CollapseSimpleStatement::ConditionalOnly
            }
            "always" => cfg.collapse_simple_statement = stylua_lib::CollapseSimpleStatement::Always,
            _ => {}
        }
    }

    cfg
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Format a Luau source string according to `config`.
pub fn format_source(source: &str, config: &FormatConfig) -> Result<String> {
    let cfg = build_stylua_config(config);
    stylua_lib::format_code(source, cfg, None, stylua_lib::OutputVerification::None)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Returns `true` if formatting `source` would produce a different string.
pub fn check_source(source: &str, config: &FormatConfig) -> Result<bool> {
    let formatted = format_source(source, config)?;
    Ok(formatted != source)
}

/// Format a file in-place. Returns `true` if the file was changed.
pub fn format_file(path: &Path, config: &FormatConfig) -> Result<bool> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let formatted = format_source(&source, config)?;
    if formatted == source {
        return Ok(false);
    }
    std::fs::write(path, &formatted)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// File walker
// ---------------------------------------------------------------------------

// TODO: extract shared walk_lua_files utility (see also lint/mod.rs)

/// Recursively collect `.luau` and `.lua` files under `dir`.
pub fn collect_lua_files(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    collect_lua_files_inner(dir, &mut out)?;
    Ok(out)
}

fn collect_lua_files_inner(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
    let read_dir = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read dir: {}", dir.display()))?;

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
            collect_lua_files_inner(&entry.path(), out)?;
            continue;
        }

        if !ft.is_file() {
            continue;
        }

        if name_str.ends_with(".luau") || name_str.ends_with(".lua") {
            out.push(entry.path());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_source_normalizes_whitespace() {
        let input = "local   x   =    1";
        let output = format_source(input, &FormatConfig::default()).unwrap();
        assert_eq!(output, "local x = 1\n");
    }

    #[test]
    fn check_source_detects_diff() {
        let messy = "local   x=1";
        assert!(check_source(messy, &FormatConfig::default()).unwrap());
    }

    #[test]
    fn check_source_clean_returns_false() {
        let clean = "local x = 1\n";
        assert!(!check_source(clean, &FormatConfig::default()).unwrap());
    }

    #[test]
    fn config_maps_indent_spaces() {
        let fc = FormatConfig {
            indent_type: Some("spaces".into()),
            indent_width: Some(2),
            ..Default::default()
        };
        let cfg = build_stylua_config(&fc);
        assert_eq!(cfg.indent_type, stylua_lib::IndentType::Spaces);
        assert_eq!(cfg.indent_width, 2);
    }

    #[test]
    fn config_maps_quote_style() {
        let fc = FormatConfig {
            quote_style: Some("single".into()),
            ..Default::default()
        };
        let cfg = build_stylua_config(&fc);
        assert_eq!(cfg.quote_style, stylua_lib::QuoteStyle::ForceSingle);
    }

    #[test]
    fn format_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.luau");
        std::fs::write(&path, "local   x   =    1").unwrap();
        let changed = format_file(&path, &FormatConfig::default()).unwrap();
        assert!(changed);
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "local x = 1\n");

        // Second pass should be a no-op.
        let changed2 = format_file(&path, &FormatConfig::default()).unwrap();
        assert!(!changed2);
    }
}
