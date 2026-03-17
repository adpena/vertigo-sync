#![forbid(unsafe_code)]

use anyhow::{Context, Result, bail};
use clap::{ArgAction, Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::time::Duration;
use vertigo_sync::errors::SyncError;
use vertigo_sync::output;
use vertigo_sync::project::parse_project;
use vertigo_sync::server::run_serve;
use vertigo_sync::validate;
use vertigo_sync::{
    DiffEvent, EventDiffCounts, EventPaths, append_event, build_snapshot, diff_snapshots,
    next_event_seq, read_snapshot, run_doctor, run_health_doctor, run_watch, run_watch_native,
    write_json_file,
};

#[derive(Debug, Parser)]
#[command(
    name = "vertigo-sync",
    version,
    about = "Fast, deterministic source sync for Roblox Studio",
    long_about = "Vertigo Sync provides sub-millisecond source synchronization between your \
                  filesystem and Roblox Studio. It replaces Rojo with better performance, \
                  built-in validation, and agent-native MCP tools.",
    after_help = "Examples:\n  \
                  vertigo-sync serve --turbo        Start syncing in turbo mode\n  \
                  vertigo-sync build -o game.rbxl   Build a place file\n  \
                  vertigo-sync doctor               Check project health\n  \
                  vertigo-sync init                 Create a new project\n  \
                  vertigo-sync plugin-install       Install Studio plugin\n\n\
                  Learn more: https://github.com/pena/vertigo-sync",
    term_width = 100,
)]
struct Cli {
    #[arg(
        long,
        default_value = ".",
        help = "Workspace root used to resolve include and relative output paths"
    )]
    root: PathBuf,

    #[arg(
        long,
        default_value = ".vertigo-sync-state",
        help = "Directory for default snapshot/diff/event files"
    )]
    state_dir: PathBuf,

    #[arg(
        long,
        help = "Current snapshot JSON path (used by snapshot/diff/event)"
    )]
    snapshot: Option<PathBuf>,

    #[arg(long, help = "Previous snapshot JSON path used by diff/event")]
    previous: Option<PathBuf>,

    #[arg(long, help = "Diff JSON output path used by event/diff commands")]
    diff: Option<PathBuf>,

    #[arg(
        long,
        help = "Primary output JSON path. snapshot: snapshot output. event: latest event output. doctor/health: report output"
    )]
    output: Option<PathBuf>,

    #[arg(long, help = "JSONL event log path used by event command")]
    event_log: Option<PathBuf>,

    #[arg(
        long = "include",
        value_name = "PATH",
        action = ArgAction::Append,
        value_delimiter = ',',
        help = "Include roots to sync. Default: auto-detect from project file, or 'src'"
    )]
    include: Vec<String>,

    #[arg(
        long,
        default_value_t = 2,
        help = "Polling interval in seconds for watch/serve modes"
    )]
    interval_seconds: u64,

    #[arg(long, default_value_t = 7575, help = "HTTP port used by serve mode")]
    port: u16,

    #[arg(
        long,
        default_value_t = 1024,
        help = "Broadcast channel capacity for serve/event fanout"
    )]
    channel_capacity: usize,

    #[arg(
        long,
        default_value_t = 50,
        help = "Event coalescing window in milliseconds"
    )]
    coalesce_ms: u64,

    #[arg(
        long,
        default_value_t = false,
        help = "Turbo mode: 10ms coalesce, 100ms poll, native fsevents"
    )]
    turbo: bool,

    /// Output machine-readable JSON instead of human-readable text
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Walk include roots and write deterministic snapshot JSON.
    #[command(display_order = 1)]
    Snapshot,
    /// Compare previous snapshot vs current and write deterministic diff JSON.
    #[command(display_order = 2)]
    Diff,
    /// Compute diff and append event JSONL with monotonic sequence number.
    #[command(display_order = 3)]
    Event,
    /// Run determinism + health checks and fail on non-determinism.
    #[command(display_order = 10)]
    Doctor,
    /// Run source-tree health checks.
    #[command(display_order = 11)]
    Health,
    /// Validate Luau source files for common issues.
    #[command(display_order = 12)]
    Validate,
    /// Serve snapshot/diff/events over HTTP + SSE.
    #[command(display_order = 20)]
    Serve,
    /// Blocking watch loop that emits NDJSON diff events to stdout.
    #[command(display_order = 21)]
    Watch,
    /// Native filesystem watch using FSEvents/inotify (replaces polling).
    #[command(display_order = 22)]
    WatchNative,
    /// Build a .rbxl place file from source (replaces `rojo build`).
    #[command(display_order = 30)]
    Build {
        /// Output .rbxl file path.
        #[arg(long, short)]
        output: PathBuf,
        /// Project file path (default: default.project.json).
        #[arg(long, default_value = "default.project.json")]
        project: PathBuf,
        /// Enable binary model (.rbxm/.rbxmx) processing.
        #[arg(long, default_value_t = false)]
        binary_models: bool,
    },
    /// Create a new Vertigo Sync project with standard directory structure.
    #[command(display_order = 40)]
    Init {
        /// Project name (default: current directory name)
        #[arg(long)]
        name: Option<String>,
    },
    /// Install the Vertigo Sync Studio plugin.
    #[command(display_order = 41)]
    PluginInstall,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = resolve_root(&cli.root)?;
    let state_dir = resolve_relative_to_root(&root, &cli.state_dir);

    match &cli.command {
        Command::Snapshot => command_snapshot(&root, &state_dir, &cli),
        Command::Diff => command_diff(&root, &state_dir, &cli),
        Command::Event => command_event(&root, &state_dir, &cli),
        Command::Doctor => command_doctor(&root, &state_dir, &cli),
        Command::Health => command_health(&root, &state_dir, &cli),
        Command::Watch => command_watch(&root, &state_dir, &cli),
        Command::WatchNative => command_watch_native(&root, &state_dir, &cli),
        Command::Validate => command_validate(&root, &cli),
        Command::Build {
            output,
            project,
            binary_models,
        } => command_build(&root, output, project, *binary_models),
        Command::Init { name } => command_init(&root, name.as_deref()),
        Command::PluginInstall => command_plugin_install(),
        Command::Serve => {
            let includes = resolve_effective_includes(&root, &cli.include);
            let (interval, coalesce_ms) = if cli.turbo {
                (Duration::from_millis(100), 10u64)
            } else {
                (
                    Duration::from_secs(cli.interval_seconds.max(1)),
                    cli.coalesce_ms,
                )
            };

            let mode = if cli.turbo { "turbo (10ms coalesce)" } else { "standard" };
            let version = env!("CARGO_PKG_VERSION");
            let http_addr = format!("http://127.0.0.1:{}", cli.port);
            let ws_addr = format!("ws://127.0.0.1:{}/ws", cli.port);
            let watching = includes.join(", ");

            output::banner(version, &[
                ("Server", &http_addr),
                ("WebSocket", &ws_addr),
                ("Mode", mode),
                ("Watching", &watching),
            ]);

            run_serve(
                root,
                includes,
                cli.port,
                interval,
                cli.channel_capacity,
                coalesce_ms,
                cli.turbo,
            )
            .await
        }
    }
}

// ---------------------------------------------------------------------------
// Include resolution
// ---------------------------------------------------------------------------

/// Resolve effective include paths: use CLI values if provided, else try to
/// auto-detect from `default.project.json` `$path` entries, else fall back
/// to `["src"]`.
fn resolve_effective_includes(root: &Path, cli_includes: &[String]) -> Vec<String> {
    if !cli_includes.is_empty() {
        return cli_includes.to_vec();
    }

    // Try auto-detect from project file.
    let project_path = root.join("default.project.json");
    if project_path.is_file() {
        if let Ok(content) = std::fs::read_to_string(&project_path) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
                let mut paths: Vec<String> = Vec::new();
                if let Some(tree) = value.get("tree").and_then(|t| t.as_object()) {
                    collect_dollar_paths(tree, &mut paths);
                }
                if !paths.is_empty() {
                    // Deduplicate and sort for determinism.
                    paths.sort();
                    paths.dedup();
                    return paths;
                }
            }
        }
    }

    // Fall back.
    vec!["src".to_string()]
}

/// Recursively collect `$path` values from a Rojo/Vertigo project tree.
fn collect_dollar_paths(
    obj: &serde_json::Map<String, serde_json::Value>,
    out: &mut Vec<String>,
) {
    if let Some(serde_json::Value::String(p)) = obj.get("$path") {
        // Extract the top-level directory (e.g. "src/Server" -> "src").
        let top = p.split('/').next().unwrap_or(p);
        out.push(top.to_string());
    }
    for (key, val) in obj {
        if key.starts_with('$') {
            continue;
        }
        if let Some(child_obj) = val.as_object() {
            collect_dollar_paths(child_obj, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

fn command_snapshot(root: &Path, state_dir: &Path, cli: &Cli) -> Result<()> {
    let includes = resolve_effective_includes(root, &cli.include);
    let snapshot = build_snapshot(root, &includes)?;
    let snapshot_path = cli
        .snapshot
        .as_ref()
        .or(cli.output.as_ref())
        .map(|value| resolve_relative_to_root(root, value))
        .unwrap_or_else(|| default_snapshot_path(state_dir));

    write_json_file(&snapshot_path, &snapshot)?;

    if cli.json {
        println!("{}", serde_json::to_string(&snapshot)?);
    } else {
        output::success("Snapshot captured");
        output::kv("Entries", &format!("{} files", snapshot.entries.len()));
        output::kv("Fingerprint", &snapshot.fingerprint);
        output::kv("Output", &snapshot_path.display().to_string());
    }

    Ok(())
}

fn command_diff(root: &Path, state_dir: &Path, cli: &Cli) -> Result<()> {
    let includes = resolve_effective_includes(root, &cli.include);
    let previous_path = resolve_previous_path(root, state_dir, cli);
    let previous = read_snapshot(&previous_path).with_context(|| {
        format!(
            "failed reading previous snapshot {}",
            previous_path.display()
        )
    })?;
    let current = build_snapshot(root, &includes)?;
    let diff = diff_snapshots(&previous, &current);

    let output_path = cli
        .diff
        .as_ref()
        .or(cli.output.as_ref())
        .map(|value| resolve_relative_to_root(root, value))
        .unwrap_or_else(|| default_diff_path(state_dir));
    write_json_file(&output_path, &diff)?;

    if let Some(snapshot_path) = cli.snapshot.as_ref() {
        let path = resolve_relative_to_root(root, snapshot_path);
        write_json_file(&path, &current)?;
    }

    if cli.json {
        println!("{}", serde_json::to_string(&diff)?);
    } else {
        let added = diff.added.len();
        let modified = diff.modified.len();
        let deleted = diff.deleted.len();

        if added == 0 && modified == 0 && deleted == 0 {
            output::success("No changes detected");
        } else {
            output::success("Diff computed");
            if added > 0 {
                output::kv("Added", &format!("{added} files"));
            }
            if modified > 0 {
                output::kv("Modified", &format!("{modified} files"));
            }
            if deleted > 0 {
                output::kv("Deleted", &format!("{deleted} files"));
            }
        }
        output::kv("Output", &output_path.display().to_string());
    }

    Ok(())
}

fn command_event(root: &Path, state_dir: &Path, cli: &Cli) -> Result<()> {
    let includes = resolve_effective_includes(root, &cli.include);
    let previous_path = resolve_previous_path(root, state_dir, cli);
    let previous = read_snapshot(&previous_path).with_context(|| {
        format!(
            "failed reading previous snapshot {}",
            previous_path.display()
        )
    })?;
    let current = build_snapshot(root, &includes)?;
    let diff = diff_snapshots(&previous, &current);

    let event_log_path = cli
        .event_log
        .as_ref()
        .map(|value| resolve_relative_to_root(root, value))
        .unwrap_or_else(|| default_event_log_path(state_dir));

    let seq = next_event_seq(&event_log_path)?;
    let event = DiffEvent {
        seq,
        event: "patch".to_string(),
        timestamp_utc: chrono::Utc::now().to_rfc3339(),
        source_hash: current.fingerprint.clone(),
        snapshot_hash: current.fingerprint.clone(),
        diff: EventDiffCounts {
            added: diff.added.len(),
            modified: diff.modified.len(),
            deleted: diff.deleted.len(),
        },
        paths: EventPaths {
            added: diff.added.iter().map(|entry| entry.path.clone()).collect(),
            modified: diff
                .modified
                .iter()
                .map(|entry| entry.path.clone())
                .collect(),
            deleted: diff
                .deleted
                .iter()
                .map(|entry| entry.path.clone())
                .collect(),
        },
    };

    append_event(&event_log_path, &event)?;

    let latest_event_path = cli
        .output
        .as_ref()
        .map(|value| resolve_relative_to_root(root, value))
        .unwrap_or_else(|| default_event_output_path(state_dir));
    write_json_file(&latest_event_path, &event)?;

    let diff_output_path = cli
        .diff
        .as_ref()
        .map(|value| resolve_relative_to_root(root, value))
        .unwrap_or_else(|| default_diff_path(state_dir));
    write_json_file(&diff_output_path, &diff)?;

    let snapshot_path = cli
        .snapshot
        .as_ref()
        .map(|value| resolve_relative_to_root(root, value))
        .unwrap_or_else(|| default_snapshot_path(state_dir));
    write_json_file(&snapshot_path, &current)?;

    if cli.json {
        println!("{}", serde_json::to_string(&event)?);
    } else {
        output::success(&format!("Event #{seq} recorded"));
        output::kv("Added", &format!("{}", event.diff.added));
        output::kv("Modified", &format!("{}", event.diff.modified));
        output::kv("Deleted", &format!("{}", event.diff.deleted));
        output::kv("Log", &event_log_path.display().to_string());
    }

    Ok(())
}

fn command_doctor(root: &Path, _state_dir: &Path, cli: &Cli) -> Result<()> {
    let includes = resolve_effective_includes(root, &cli.include);
    let determinism = run_doctor(root, &includes)?;
    let health = run_health_doctor(root, &includes)?;

    if cli.json {
        let report = serde_json::json!({
            "determinism": determinism,
            "health": health,
        });
        if let Some(output_path) = cli.output.as_ref() {
            let path = resolve_relative_to_root(root, output_path);
            write_json_file(&path, &report)?;
        }
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        output::header("Vertigo Sync Doctor");
        output::separator("Vertigo Sync Doctor");
        eprintln!();

        // Determinism section.
        output::header("Determinism");
        if determinism.deterministic {
            output::success(&format!("Snapshot 1: {}", determinism.first_fingerprint));
            output::success(&format!("Snapshot 2: {}", determinism.second_fingerprint));
            output::success("Hashes match \u{2014} deterministic");
        } else {
            output::error_msg(&format!("Snapshot 1: {}", determinism.first_fingerprint));
            output::error_msg(&format!("Snapshot 2: {}", determinism.second_fingerprint));
            if let Some(ref path) = determinism.mismatch_path {
                output::error_msg(&format!("First mismatch: {path}"));
            }
            output::error_msg("Hashes differ \u{2014} NON-DETERMINISTIC");
        }
        eprintln!();

        // Source health section.
        output::header("Source Health");
        output::success(&format!("{} files checked", health.file_count));
        let warning_count = health
            .issues
            .iter()
            .filter(|i| i.severity == "warning")
            .count();
        let error_count = health
            .issues
            .iter()
            .filter(|i| i.severity == "error")
            .count();
        if warning_count > 0 {
            output::warn(&format!("{warning_count} warning(s)"));
        }
        if error_count > 0 {
            output::error_msg(&format!("{error_count} error(s)"));
        }
        eprintln!();

        // Summary.
        if determinism.deterministic && health.healthy {
            output::success("All checks passed");
        } else {
            output::error_msg("Some checks failed");
        }

        if let Some(output_path) = cli.output.as_ref() {
            let path = resolve_relative_to_root(root, output_path);
            let report = serde_json::json!({
                "determinism": determinism,
                "health": health,
            });
            write_json_file(&path, &report)?;
        }
    }

    if !determinism.deterministic {
        let err = SyncError::NonDeterministic {
            hash1: determinism.first_fingerprint.clone(),
            hash2: determinism.second_fingerprint.clone(),
        };
        if !cli.json {
            output::error_msg(&err.suggestion());
        }
        bail!("doctor detected non-deterministic snapshots")
    }

    Ok(())
}

fn command_health(root: &Path, _state_dir: &Path, cli: &Cli) -> Result<()> {
    let includes = resolve_effective_includes(root, &cli.include);
    let report = run_health_doctor(root, &includes)?;

    if cli.json {
        if let Some(output_path) = cli.output.as_ref() {
            let path = resolve_relative_to_root(root, output_path);
            write_json_file(&path, &report)?;
        }
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        output::header("Source Health");
        output::success(&format!("{} files checked", report.file_count));
        output::kv("Fingerprint", &report.fingerprint);

        for issue in &report.issues {
            match issue.severity.as_str() {
                "error" => output::error_msg(&format!("{}: {}", issue.path, issue.message)),
                "warning" => output::warn(&format!("{}: {}", issue.path, issue.message)),
                _ => output::info(&format!("{}: {}", issue.path, issue.message)),
            }
        }

        if report.healthy {
            output::success("Healthy");
        } else {
            output::error_msg("Issues found");
        }

        if let Some(output_path) = cli.output.as_ref() {
            let path = resolve_relative_to_root(root, output_path);
            write_json_file(&path, &report)?;
        }
    }

    if !report.healthy {
        bail!("health doctor found blocking issues")
    }

    Ok(())
}

fn command_validate(root: &Path, cli: &Cli) -> Result<()> {
    let includes = resolve_effective_includes(root, &cli.include);
    let report = validate::validate_source(root, &includes)?;

    if cli.json {
        println!("{}", serde_json::to_string(&report)?);
        if let Some(output_path) = cli.output.as_ref() {
            let path = resolve_relative_to_root(root, output_path);
            write_json_file(&path, &report)?;
        }
        if report.errors > 0 {
            bail!(
                "validation failed: {} error(s), {} warning(s)",
                report.errors,
                report.warnings
            )
        }
        return Ok(());
    }

    // Print issues in a cargo-style format with colors.
    for issue in &report.issues {
        let location = if issue.line > 0 {
            format!("{}:{}", issue.path, issue.line)
        } else {
            issue.path.clone()
        };
        let msg = format!("[{}]: {location}: {}", issue.rule, issue.message);
        match issue.severity.as_str() {
            "error" => output::error_msg(&msg),
            "warning" => output::warn(&msg),
            _ => output::info(&msg),
        }
    }

    // Optionally run selene if available.
    if let Some(selene_output) = validate::run_selene(root, &includes)
        && !selene_output.is_empty()
    {
        eprintln!();
        output::header("selene output");
        for line in &selene_output {
            output::info(line);
        }
    }

    eprintln!();
    if report.clean {
        output::success(&format!(
            "{} files checked, no issues",
            report.files_checked
        ));
    } else {
        let status = format!(
            "{} files, {} error(s), {} warning(s)",
            report.files_checked, report.errors, report.warnings
        );
        if report.errors > 0 {
            output::error_msg(&status);
        } else {
            output::warn(&status);
        }
    }

    if let Some(output_path) = cli.output.as_ref() {
        let path = resolve_relative_to_root(root, output_path);
        write_json_file(&path, &report)?;
    }

    if report.errors > 0 {
        let err = SyncError::ValidationFailed {
            errors: report.errors,
            warnings: report.warnings,
        };
        output::error_msg(&err.suggestion());
        bail!(
            "validation failed: {} error(s), {} warning(s)",
            report.errors,
            report.warnings
        )
    }

    Ok(())
}

fn command_watch(root: &Path, state_dir: &Path, cli: &Cli) -> Result<()> {
    let includes = resolve_effective_includes(root, &cli.include);
    let output_dir = cli
        .output
        .as_ref()
        .map(|value| resolve_relative_to_root(root, value))
        .unwrap_or_else(|| state_dir.to_path_buf());

    run_watch(
        root,
        &includes,
        Duration::from_secs(cli.interval_seconds.max(1)),
        Some(&output_dir),
    )
}

fn command_watch_native(root: &Path, state_dir: &Path, cli: &Cli) -> Result<()> {
    let includes = resolve_effective_includes(root, &cli.include);
    let output_dir = cli
        .output
        .as_ref()
        .map(|value| resolve_relative_to_root(root, value))
        .unwrap_or_else(|| state_dir.to_path_buf());

    let coalesce_ms = if cli.turbo {
        output::info("turbo mode: 10ms coalesce, native fsevents");
        10
    } else {
        cli.coalesce_ms
    };
    run_watch_native(
        root,
        &includes,
        Some(&output_dir),
        Duration::from_millis(coalesce_ms),
    )
}

fn command_build(root: &Path, output: &Path, project: &Path, _binary_models: bool) -> Result<()> {
    use rbx_dom_weak::{InstanceBuilder, WeakDom};
    use vertigo_sync::project::resolve_instance_class;

    let project_path = if project.is_absolute() {
        project.to_path_buf()
    } else {
        root.join(project)
    };

    let tree = parse_project(&project_path)?;

    output::header("Build");
    output::kv("Project", &project_path.display().to_string());
    output::kv("Output", &output.display().to_string());
    output::kv("Name", &tree.name);
    output::kv("Mappings", &tree.mappings.len().to_string());

    // Build the DataModel DOM from source files.
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let data_model_ref = dom.root_ref();

    // Create service containers from project tree mappings.
    let mut service_refs: std::collections::HashMap<String, rbx_dom_weak::types::Ref> =
        std::collections::HashMap::new();

    for mapping in &tree.mappings {
        let segments: Vec<&str> = mapping.instance_path.split('.').collect();

        let mut parent_ref = data_model_ref;
        for (i, segment) in segments.iter().enumerate() {
            let existing = dom
                .get_by_ref(parent_ref)
                .and_then(|inst| {
                    inst.children()
                        .iter()
                        .find(|&&child_ref| {
                            dom.get_by_ref(child_ref)
                                .map(|c| c.name == *segment)
                                .unwrap_or(false)
                        })
                        .copied()
                });

            parent_ref = if let Some(existing_ref) = existing {
                existing_ref
            } else {
                let class = if i == 0 {
                    mapping.class_name.as_str()
                } else if i == segments.len() - 1 {
                    let fs_full = root.join(&mapping.fs_path);
                    if fs_full.is_dir() {
                        resolve_container_class(&fs_full)
                    } else {
                        resolve_instance_class(&mapping.fs_path)
                    }
                } else {
                    segment
                };

                let builder = InstanceBuilder::new(class).with_name(*segment);
                dom.insert(parent_ref, builder)
            };

            service_refs
                .entry(segment.to_string())
                .or_insert(parent_ref);
        }

        let fs_full = root.join(&mapping.fs_path);
        if fs_full.is_dir() {
            populate_from_dir(&mut dom, parent_ref, &fs_full, root)?;
        } else if fs_full.is_file() {
            populate_file(&mut dom, parent_ref, &fs_full)?;
        }
    }

    let instance_count = count_instances(&dom, data_model_ref);
    output::kv("Instances", &instance_count.to_string());

    let output_path = if output.is_absolute() {
        output.to_path_buf()
    } else {
        root.join(output)
    };

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let root_children: Vec<_> = dom.root().children().to_vec();
    let file = std::io::BufWriter::new(
        std::fs::File::create(&output_path)
            .with_context(|| format!("failed to create output file {}", output_path.display()))?,
    );

    let ext = output_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("rbxl");

    match ext {
        "rbxlx" => {
            let options = rbx_xml::EncodeOptions::new()
                .property_behavior(rbx_xml::EncodePropertyBehavior::WriteUnknown);
            rbx_xml::to_writer(file, &dom, &root_children, options)
                .context("failed to write .rbxlx")?;
        }
        _ => {
            rbx_binary::to_writer(file, &dom, &root_children).context("failed to write .rbxl")?;
        }
    }

    let file_size = std::fs::metadata(&output_path)?.len();
    output::success(&format!(
        "Wrote {} ({:.1} KB)",
        output_path.display(),
        file_size as f64 / 1024.0
    ));

    Ok(())
}

// ---------------------------------------------------------------------------
// Init command
// ---------------------------------------------------------------------------

fn command_init(root: &Path, name: Option<&str>) -> Result<()> {
    let project_name = name
        .map(|n| n.to_string())
        .or_else(|| {
            root.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.to_string())
        })
        .unwrap_or_else(|| "my-project".to_string());

    output::header(&format!("Initializing project: {project_name}"));
    eprintln!();

    let project_path = root.join("default.project.json");
    if project_path.exists() {
        output::warn("default.project.json already exists, skipping project file creation");
    } else {
        output::step(1, 4, "Creating default.project.json");
        let project_json = serde_json::json!({
            "name": project_name,
            "tree": {
                "$className": "DataModel",
                "ServerScriptService": {
                    "Server": {
                        "$path": "src/Server"
                    }
                },
                "StarterPlayer": {
                    "StarterPlayerScripts": {
                        "Client": {
                            "$path": "src/Client"
                        }
                    }
                },
                "ReplicatedStorage": {
                    "Shared": {
                        "$path": "src/Shared"
                    }
                }
            }
        });
        let formatted = serde_json::to_string_pretty(&project_json)?;
        std::fs::write(&project_path, formatted.as_bytes())
            .with_context(|| format!("failed to write {}", project_path.display()))?;
        output::success("Created default.project.json");
    }

    // Create source directories and boilerplate files.
    let dirs_and_files: &[(&str, &str, &str)] = &[
        (
            "src/Server",
            "init.server.luau",
            "--!strict\nprint(\"[Server] Hello from Vertigo Sync!\")\n",
        ),
        (
            "src/Client",
            "init.client.luau",
            "--!strict\nprint(\"[Client] Hello from Vertigo Sync!\")\n",
        ),
        (
            "src/Shared",
            "init.luau",
            "--!strict\nreturn {}\n",
        ),
    ];

    for (i, (dir, file, content)) in dirs_and_files.iter().enumerate() {
        let dir_path = root.join(dir);
        let file_path = dir_path.join(file);

        output::step(i + 2, 4, &format!("Creating {dir}/{file}"));

        std::fs::create_dir_all(&dir_path)
            .with_context(|| format!("failed to create directory {}", dir_path.display()))?;

        if file_path.exists() {
            output::warn(&format!("{dir}/{file} already exists, skipping"));
        } else {
            std::fs::write(&file_path, content)
                .with_context(|| format!("failed to write {}", file_path.display()))?;
            output::success(&format!("Created {dir}/{file}"));
        }
    }

    eprintln!();
    output::success(&format!("Project '{project_name}' initialized"));
    eprintln!();
    output::info("Next steps:");
    output::info("  vertigo-sync serve --turbo     Start syncing");
    output::info("  vertigo-sync plugin-install     Install Studio plugin");
    output::info("  vertigo-sync doctor             Verify project health");

    Ok(())
}

// ---------------------------------------------------------------------------
// Plugin install command
// ---------------------------------------------------------------------------

/// Embedded Studio plugin source, compiled into the binary.
const PLUGIN_SOURCE: &str = include_str!("../../../studio-plugin/VertigoSyncPlugin.lua");

fn command_plugin_install() -> Result<()> {
    let plugins_dir = detect_plugins_dir()?;

    output::step(1, 2, "Detecting Roblox Plugins directory");
    output::success(&format!("Found: {}", plugins_dir.display()));

    std::fs::create_dir_all(&plugins_dir)
        .with_context(|| format!("failed to create {}", plugins_dir.display()))?;

    let dest = plugins_dir.join("VertigoSyncPlugin.lua");
    output::step(2, 2, "Installing VertigoSyncPlugin.lua");
    std::fs::write(&dest, PLUGIN_SOURCE)
        .with_context(|| format!("failed to write {}", dest.display()))?;

    output::success(&format!("Installed to {}", dest.display()));
    eprintln!();
    output::info("Restart Roblox Studio to load the plugin.");

    Ok(())
}

/// Detect the OS-appropriate Roblox Studio plugins directory.
fn detect_plugins_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = dirs_or_home() {
            let path = home.join("Documents/Roblox/Plugins");
            return Ok(path);
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            let path = PathBuf::from(local).join("Roblox").join("Plugins");
            return Ok(path);
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(home) = dirs_or_home() {
            let path = home.join(".local/share/Roblox/Plugins");
            if path.parent().map(|p| p.exists()).unwrap_or(false) {
                return Ok(path);
            }
        }
    }

    Err(SyncError::PluginDirNotFound.into())
}

/// Get the user home directory without pulling in the `dirs` crate.
fn dirs_or_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// Build helpers (unchanged logic, kept for the build command)
// ---------------------------------------------------------------------------

/// Recursively populate a DOM container from a filesystem directory.
fn populate_from_dir(
    dom: &mut rbx_dom_weak::WeakDom,
    parent_ref: rbx_dom_weak::types::Ref,
    dir: &Path,
    _root: &Path,
) -> Result<()> {
    use vertigo_sync::project::resolve_instance_class;

    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read directory {}", dir.display()))?
        .filter_map(|e| e.ok())
        .collect();

    // Sort for determinism.
    entries.sort_by_key(|e| e.file_name());

    // Collect .meta.json filenames for sidecar lookup (zero extra syscalls).
    let meta_names: std::collections::HashSet<String> = entries
        .iter()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".meta.json") {
                Some(name)
            } else {
                None
            }
        })
        .collect();

    for entry in &entries {
        let path = entry.path();
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();

        // Skip hidden files.
        if name_str.starts_with('.') {
            continue;
        }

        // Skip .meta.json sidecars (they are applied to siblings).
        if name_str.ends_with(".meta.json") {
            continue;
        }

        if path.is_dir() {
            let class = resolve_container_class(&path);

            let folder_ref = dom.insert(
                parent_ref,
                rbx_dom_weak::InstanceBuilder::new(class).with_name(&*name_str),
            );

            // If there's an init script, load its source.
            for init_name in &[
                "init.server.luau",
                "init.server.lua",
                "init.client.luau",
                "init.client.lua",
                "init.luau",
                "init.lua",
            ] {
                let init_path = path.join(init_name);
                if init_path.exists() {
                    if let Ok(source) = std::fs::read_to_string(&init_path)
                        && let Some(inst) = dom.get_by_ref_mut(folder_ref)
                    {
                        inst.properties.insert(
                            "Source".into(),
                            rbx_dom_weak::types::Variant::String(source),
                        );
                    }
                    break;
                }
            }

            // Recurse into subdirectory (skip init scripts).
            populate_from_dir(dom, folder_ref, &path, _root)?;
        } else if path.is_file() {
            // Skip init scripts (already handled by parent directory).
            if name_str.starts_with("init.") {
                continue;
            }

            let class = resolve_instance_class(&name_str);

            // Skip files that resolve to "Skip" (e.g. .meta.json handled as sidecars).
            if class == "Skip" {
                continue;
            }

            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

            match ext {
                "luau" | "lua" => {
                    let instance_name = resolve_instance_name(&name_str);
                    let mut builder =
                        rbx_dom_weak::InstanceBuilder::new(class).with_name(instance_name);

                    if let Ok(source) = std::fs::read_to_string(&path) {
                        builder = builder.with_property(
                            "Source",
                            rbx_dom_weak::types::Variant::String(source),
                        );
                    }

                    // Apply .meta.json sidecar if present.
                    let meta_name = format!("{}.meta.json", name_str);
                    if meta_names.contains(&meta_name) {
                        let meta_path = dir.join(&meta_name);
                        if let Ok(meta_content) = std::fs::read_to_string(&meta_path)
                            && let Ok(meta) = vertigo_sync::parse_meta_json(&meta_content)
                        {
                            builder = apply_meta_to_builder(builder, &meta);
                        }
                    }

                    dom.insert(parent_ref, builder);
                }
                "json" => {
                    let instance_name = name_str.strip_suffix(".json").unwrap_or(&name_str);
                    let mut builder =
                        rbx_dom_weak::InstanceBuilder::new("ModuleScript").with_name(instance_name);
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        builder = builder.with_property(
                            "Source",
                            rbx_dom_weak::types::Variant::String(content),
                        );
                    }
                    dom.insert(parent_ref, builder);
                }
                "txt" => {
                    let instance_name = name_str.strip_suffix(".txt").unwrap_or(&name_str);
                    let mut builder =
                        rbx_dom_weak::InstanceBuilder::new("StringValue").with_name(instance_name);
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        builder = builder.with_property(
                            "Value",
                            rbx_dom_weak::types::Variant::String(content),
                        );
                    }
                    dom.insert(parent_ref, builder);
                }
                "csv" => {
                    let instance_name = name_str.strip_suffix(".csv").unwrap_or(&name_str);
                    let builder = rbx_dom_weak::InstanceBuilder::new("LocalizationTable")
                        .with_name(instance_name);
                    dom.insert(parent_ref, builder);
                }
                "rbxm" | "rbxmx" => {
                    let file = std::fs::File::open(&path);
                    if let Ok(file) = file {
                        let reader = std::io::BufReader::new(file);
                        let model_dom = if ext == "rbxm" {
                            rbx_binary::from_reader(reader).ok()
                        } else {
                            rbx_xml::from_reader_default(reader).ok()
                        };
                        if let Some(model_dom) = model_dom {
                            merge_model_into_dom(dom, parent_ref, &model_dom);
                        }
                    }
                }
                _ => {
                    // Unknown file type — skip.
                }
            }
        }
    }

    Ok(())
}

/// Apply InstanceMeta properties to a builder.
fn apply_meta_to_builder(
    mut builder: rbx_dom_weak::InstanceBuilder,
    meta: &vertigo_sync::InstanceMeta,
) -> rbx_dom_weak::InstanceBuilder {
    for (key, value) in &meta.properties {
        match value {
            serde_json::Value::Bool(b) => {
                builder =
                    builder.with_property(key.as_str(), rbx_dom_weak::types::Variant::Bool(*b));
            }
            serde_json::Value::String(s) => {
                builder = builder
                    .with_property(key.as_str(), rbx_dom_weak::types::Variant::String(s.clone()));
            }
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    builder = builder
                        .with_property(key.as_str(), rbx_dom_weak::types::Variant::Int32(i as i32));
                } else if let Some(f) = n.as_f64() {
                    builder = builder
                        .with_property(key.as_str(), rbx_dom_weak::types::Variant::Float64(f));
                }
            }
            _ => {}
        }
    }
    builder
}

/// Merge instances from a model DOM into the build DOM under a parent.
fn merge_model_into_dom(
    dom: &mut rbx_dom_weak::WeakDom,
    parent_ref: rbx_dom_weak::types::Ref,
    model_dom: &rbx_dom_weak::WeakDom,
) {
    let model_root = model_dom.root();
    for &child_ref in model_root.children() {
        merge_model_instance(dom, parent_ref, model_dom, child_ref);
    }
}

/// Recursively copy an instance from model_dom into dom.
fn merge_model_instance(
    dom: &mut rbx_dom_weak::WeakDom,
    parent_ref: rbx_dom_weak::types::Ref,
    model_dom: &rbx_dom_weak::WeakDom,
    model_ref: rbx_dom_weak::types::Ref,
) {
    let Some(inst) = model_dom.get_by_ref(model_ref) else {
        return;
    };
    let mut builder =
        rbx_dom_weak::InstanceBuilder::new(inst.class.as_str()).with_name(inst.name.as_str());

    for (key, variant) in &inst.properties {
        builder = builder.with_property(key.as_str(), variant.clone());
    }

    let new_ref = dom.insert(parent_ref, builder);

    for &child_ref in inst.children() {
        merge_model_instance(dom, new_ref, model_dom, child_ref);
    }
}

/// Populate a single file into the DOM.
fn populate_file(
    dom: &mut rbx_dom_weak::WeakDom,
    parent_ref: rbx_dom_weak::types::Ref,
    path: &Path,
) -> Result<()> {
    use vertigo_sync::project::resolve_instance_class;

    let path_str = path.to_string_lossy();
    let class = resolve_instance_class(&path_str);
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .map(resolve_instance_name)
        .unwrap_or("unnamed");

    let mut builder = rbx_dom_weak::InstanceBuilder::new(class).with_name(name);

    if let Ok(source) = std::fs::read_to_string(path) {
        builder = builder.with_property("Source", rbx_dom_weak::types::Variant::String(source));
    }

    dom.insert(parent_ref, builder);
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn resolve_container_class(path: &Path) -> &'static str {
    if path.join("init.server.luau").exists() || path.join("init.server.lua").exists() {
        "Script"
    } else if path.join("init.client.luau").exists() || path.join("init.client.lua").exists() {
        "LocalScript"
    } else if path.join("init.luau").exists() || path.join("init.lua").exists() {
        "ModuleScript"
    } else {
        "Folder"
    }
}

fn resolve_instance_name(name: &str) -> &str {
    name.strip_suffix(".server.luau")
        .or_else(|| name.strip_suffix(".server.lua"))
        .or_else(|| name.strip_suffix(".client.luau"))
        .or_else(|| name.strip_suffix(".client.lua"))
        .or_else(|| name.strip_suffix(".luau"))
        .or_else(|| name.strip_suffix(".lua"))
        .unwrap_or(name)
}

/// Count total instances in a DOM subtree.
fn count_instances(dom: &rbx_dom_weak::WeakDom, root: rbx_dom_weak::types::Ref) -> usize {
    let mut count = 1;
    if let Some(inst) = dom.get_by_ref(root) {
        for &child in inst.children() {
            count += count_instances(dom, child);
        }
    }
    count
}

fn resolve_root(path: &Path) -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to get current working directory")?;
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };

    candidate
        .canonicalize()
        .with_context(|| format!("failed to resolve root path {}", candidate.display()))
}

fn resolve_previous_path(root: &Path, state_dir: &Path, cli: &Cli) -> PathBuf {
    if let Some(path) = cli.previous.as_ref() {
        return resolve_relative_to_root(root, path);
    }
    default_previous_path(state_dir)
}

fn resolve_relative_to_root(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn default_snapshot_path(state_dir: &Path) -> PathBuf {
    state_dir.join("snapshot.current.json")
}

fn default_previous_path(state_dir: &Path) -> PathBuf {
    state_dir.join("snapshot.previous.json")
}

fn default_diff_path(state_dir: &Path) -> PathBuf {
    state_dir.join("latest-diff.json")
}

fn default_event_log_path(state_dir: &Path) -> PathBuf {
    state_dir.join("events.jsonl")
}

fn default_event_output_path(state_dir: &Path) -> PathBuf {
    state_dir.join("latest-event.json")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{resolve_container_class, resolve_instance_name};
    use std::fs;

    #[test]
    fn resolve_instance_name_strips_script_suffixes() {
        assert_eq!(resolve_instance_name("Foo.server.luau"), "Foo");
        assert_eq!(resolve_instance_name("Bar.client.lua"), "Bar");
        assert_eq!(resolve_instance_name("Baz.luau"), "Baz");
        assert_eq!(resolve_instance_name("Qux.lua"), "Qux");
    }

    #[test]
    fn resolve_container_class_detects_module_init() {
        let temp = tempfile::tempdir().expect("tempdir");
        let module_dir = temp.path().join("SharedModule");
        fs::create_dir_all(&module_dir).expect("create module dir");
        fs::write(module_dir.join("init.luau"), "return {}").expect("write init");
        assert_eq!(resolve_container_class(&module_dir), "ModuleScript");
    }

    #[test]
    fn resolve_container_class_defaults_to_folder() {
        let temp = tempfile::tempdir().expect("tempdir");
        let folder_dir = temp.path().join("PlainFolder");
        fs::create_dir_all(&folder_dir).expect("create folder dir");
        assert_eq!(resolve_container_class(&folder_dir), "Folder");
    }
}
