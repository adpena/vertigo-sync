use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PluginSmokeMatch {
    pub line: usize,
    pub rule: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PluginSmokeReport {
    pub log_path: String,
    pub clean: bool,
    pub fatal_matches: Vec<PluginSmokeMatch>,
}

const FATAL_PATTERNS: &[(&str, &str)] = &[
    ("out-of-local-registers", "Out of local registers"),
    ("nil-call", "attempt to call a nil value"),
    ("write-apply-failed", "Write apply permanently failed"),
    ("snapshot-sync-failed", "Snapshot sync failed"),
    ("payload-too-large", "Payload Too Large"),
    ("oversize-source", "OVERSIZE_SOURCE"),
];

pub fn scan_studio_log_text(log_path: &str, content: &str) -> PluginSmokeReport {
    scan_studio_log_text_with_options(log_path, content, &[], false)
}

pub fn scan_studio_log_text_with_allowlist(
    log_path: &str,
    content: &str,
    allow_plugins: &[String],
) -> PluginSmokeReport {
    scan_studio_log_text_with_options(log_path, content, allow_plugins, false)
}

pub fn scan_studio_log_text_with_options(
    log_path: &str,
    content: &str,
    allow_plugins: &[String],
    ignore_cloud_plugins: bool,
) -> PluginSmokeReport {
    let mut fatal_matches = Vec::new();
    let allowed = allow_plugins.iter().cloned().collect::<BTreeSet<_>>();
    let enforce_allowlist = !allowed.is_empty();
    let mut seen_unexpected_plugins = BTreeSet::new();

    for (index, line) in content.lines().enumerate() {
        if is_ignored_line(line) {
            continue;
        }
        if enforce_allowlist {
            if let Some(plugin_name) = external_plugin_name(line) {
                if ignore_cloud_plugins && plugin_name.starts_with("cloud_") {
                    continue;
                }
                if !allowed.contains(plugin_name)
                    && seen_unexpected_plugins.insert(plugin_name.to_string())
                {
                    fatal_matches.push(PluginSmokeMatch {
                        line: index + 1,
                        rule: "unexpected-plugin".to_string(),
                        text: line.trim().to_string(),
                    });
                    continue;
                }
            }
        }
        for (rule, needle) in FATAL_PATTERNS {
            if line.contains(needle) {
                fatal_matches.push(PluginSmokeMatch {
                    line: index + 1,
                    rule: (*rule).to_string(),
                    text: line.trim().to_string(),
                });
                break;
            }
        }
    }

    PluginSmokeReport {
        log_path: log_path.to_string(),
        clean: fatal_matches.is_empty(),
        fatal_matches,
    }
}

pub fn scan_studio_log_file(path: &Path) -> Result<PluginSmokeReport> {
    scan_studio_log_file_with_options(path, &[], false)
}

pub fn scan_studio_log_file_with_allowlist(
    path: &Path,
    allow_plugins: &[String],
) -> Result<PluginSmokeReport> {
    scan_studio_log_file_with_options(path, allow_plugins, false)
}

pub fn scan_studio_log_file_with_options(
    path: &Path,
    allow_plugins: &[String],
    ignore_cloud_plugins: bool,
) -> Result<PluginSmokeReport> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    Ok(scan_studio_log_text_with_options(
        &path.display().to_string(),
        &content,
        allow_plugins,
        ignore_cloud_plugins,
    ))
}

pub fn ensure_clean_log(report: &PluginSmokeReport) -> Result<()> {
    if report.clean {
        return Ok(());
    }

    let summary = report
        .fatal_matches
        .iter()
        .take(3)
        .map(|m| format!("{}:{} {}", m.rule, m.line, m.text))
        .collect::<Vec<_>>()
        .join(" | ");
    bail!("Studio plugin smoke failed: {summary}");
}

fn is_ignored_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("httperror: connectfail") && lower.contains("snapshot sync failed")
}

fn external_plugin_name(line: &str) -> Option<&str> {
    let plugin_name = if let Some((_, suffix)) = line.split_once("Plugin file read '") {
        suffix.split('\'').next()
    } else if let Some((_, suffix)) = line.split_once("loadPlugin ") {
        suffix.split_whitespace().next()
    } else if let Some((_, suffix)) = line.split_once("Running plugin ") {
        suffix.split_whitespace().next()
    } else {
        None
    }?;

    if plugin_name.starts_with("user_") || plugin_name.starts_with("cloud_") {
        Some(plugin_name)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_scan_flags_fatal_patterns() {
        let report = scan_studio_log_text(
            "test.log",
            "hello\nuser_VertigoSyncPlugin.lua.Script:1736: Out of local registers\nbye\n",
        );
        assert!(!report.clean);
        assert_eq!(report.fatal_matches.len(), 1);
        assert_eq!(report.fatal_matches[0].rule, "out-of-local-registers");
    }

    #[test]
    fn smoke_scan_ignores_connectfail_snapshot_noise() {
        let report = scan_studio_log_text(
            "test.log",
            "[VertigoSync] Snapshot sync failed (requested): HttpError: ConnectFail\n",
        );
        assert!(
            report.clean,
            "connectfail relay noise should not fail smoke"
        );
    }

    #[test]
    fn smoke_scan_flags_unexpected_external_plugins_when_allowlist_is_enforced() {
        let report = scan_studio_log_text_with_allowlist(
            "test.log",
            "\
2026-03-20T18:04:11.932Z,7.932210,6fb37000,6,Info [FLog::PluginFileRead] Plugin file read 'user_VertigoSyncPlugin.lua' took 0.7142 msec
2026-03-20T18:04:11.933Z,7.933789,6fb37000,6,Info [FLog::PluginFileRead] Plugin file read 'cloud_3685392992' took 0.9748 msec
2026-03-20T18:04:12.146Z,8.146775,6fb37000,6,Info [FLog::PluginLoadingEnhanced] Running plugin cloud_3685392992 took 0.0554 msec
",
            &["user_VertigoSyncPlugin.lua".to_string()],
        );
        assert!(!report.clean);
        assert!(
            report
                .fatal_matches
                .iter()
                .any(|m| m.rule == "unexpected-plugin" && m.text.contains("cloud_3685392992")),
            "expected unexpected cloud plugin failure, got {:?}",
            report.fatal_matches
        );
    }

    #[test]
    fn smoke_scan_allows_explicit_external_plugins() {
        let report = scan_studio_log_text_with_allowlist(
            "test.log",
            "\
2026-03-20T18:04:11.932Z,7.932210,6fb37000,6,Info [FLog::PluginFileRead] Plugin file read 'user_VertigoSyncPlugin.lua' took 0.7142 msec
2026-03-20T18:04:11.932Z,7.932642,6fb37000,6,Info [FLog::PluginFileRead] Plugin file read 'user_MCPStudioPlugin.rbxm' took 0.2105 msec
",
            &[
                "user_VertigoSyncPlugin.lua".to_string(),
                "user_MCPStudioPlugin.rbxm".to_string(),
            ],
        );
        assert!(report.clean, "expected allowlisted plugins to pass");
    }

    #[test]
    fn smoke_scan_can_ignore_cloud_plugins_without_allowlisting_them() {
        let report = scan_studio_log_text_with_options(
            "test.log",
            "\
2026-03-20T18:04:11.932Z,7.932210,6fb37000,6,Info [FLog::PluginFileRead] Plugin file read 'user_VertigoSyncPlugin.lua' took 0.7142 msec
2026-03-20T18:04:11.933Z,7.933789,6fb37000,6,Info [FLog::PluginFileRead] Plugin file read 'cloud_3685392992' took 0.9748 msec
",
            &["user_VertigoSyncPlugin.lua".to_string()],
            true,
        );
        assert!(
            report.clean,
            "expected ignored cloud plugins not to fail smoke: {:?}",
            report
        );
    }
}
