#![forbid(unsafe_code)]

use anyhow::{Context, Result, bail};
use clap::{ArgAction, Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::time::Duration;
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
    about = "Deterministic source snapshot/diff stream for Vertigo Sync"
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
        help = "Relative include roots to hash. Repeat flag or use comma-separated values. Default: src,studio-plugin,scripts/dev"
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

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Walk include roots and write deterministic snapshot JSON.
    Snapshot,
    /// Compare previous snapshot vs current and write deterministic diff JSON.
    Diff,
    /// Compute diff and append event JSONL with monotonic sequence number.
    Event,
    /// Run determinism + health checks and fail on non-determinism.
    Doctor,
    /// Run source-tree health checks.
    Health,
    /// Blocking watch loop that emits NDJSON diff events to stdout.
    Watch,
    /// Native filesystem watch using FSEvents/inotify (replaces polling).
    WatchNative,
    /// Serve snapshot/diff/events over HTTP + SSE.
    Serve,
    /// Validate Luau source files for common issues.
    Validate,
    /// Build a .rbxl place file from source (replaces `rojo build`).
    Build {
        /// Output .rbxl file path.
        #[arg(long, short)]
        output: PathBuf,
        /// Project file path (default: default.project.json).
        #[arg(long, default_value = "default.project.json")]
        project: PathBuf,
    },
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
        Command::Build { output, project } => command_build(&root, output, project),
        Command::Serve => {
            let includes = cli.include.clone();
            let (interval, coalesce_ms) = if cli.turbo {
                eprintln!("[vertigo-sync] turbo mode: 10ms coalesce, native fsevents");
                (Duration::from_millis(100), 10u64)
            } else {
                (
                    Duration::from_secs(cli.interval_seconds.max(1)),
                    cli.coalesce_ms,
                )
            };
            run_serve(
                root,
                includes,
                cli.port,
                interval,
                cli.channel_capacity,
                coalesce_ms,
            )
            .await
        }
    }
}

fn command_snapshot(root: &Path, state_dir: &Path, cli: &Cli) -> Result<()> {
    let snapshot = build_snapshot(root, &cli.include)?;
    let snapshot_path = cli
        .snapshot
        .as_ref()
        .or(cli.output.as_ref())
        .map(|value| resolve_relative_to_root(root, value))
        .unwrap_or_else(|| default_snapshot_path(state_dir));

    write_json_file(&snapshot_path, &snapshot)?;

    println!(
        "snapshot path={} entries={} fingerprint={}",
        snapshot_path.display(),
        snapshot.entries.len(),
        snapshot.fingerprint
    );

    Ok(())
}

fn command_diff(root: &Path, state_dir: &Path, cli: &Cli) -> Result<()> {
    let previous_path = resolve_previous_path(root, state_dir, cli);
    let previous = read_snapshot(&previous_path).with_context(|| {
        format!(
            "failed reading previous snapshot {}",
            previous_path.display()
        )
    })?;
    let current = build_snapshot(root, &cli.include)?;
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
        println!("updated snapshot={}", path.display());
    }

    println!(
        "diff path={} added={} modified={} deleted={}",
        output_path.display(),
        diff.added.len(),
        diff.modified.len(),
        diff.deleted.len()
    );

    Ok(())
}

fn command_event(root: &Path, state_dir: &Path, cli: &Cli) -> Result<()> {
    let previous_path = resolve_previous_path(root, state_dir, cli);
    let previous = read_snapshot(&previous_path).with_context(|| {
        format!(
            "failed reading previous snapshot {}",
            previous_path.display()
        )
    })?;
    let current = build_snapshot(root, &cli.include)?;
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

    println!(
        "event log={} seq={} event={} added={} modified={} deleted={} snapshot={} latest_event={} diff={}",
        event_log_path.display(),
        seq,
        event.event,
        event.diff.added,
        event.diff.modified,
        event.diff.deleted,
        snapshot_path.display(),
        latest_event_path.display(),
        diff_output_path.display(),
    );

    Ok(())
}

fn command_doctor(root: &Path, _state_dir: &Path, cli: &Cli) -> Result<()> {
    let determinism = run_doctor(root, &cli.include)?;
    let health = run_health_doctor(root, &cli.include)?;
    let report = serde_json::json!({
        "determinism": determinism,
        "health": health,
    });

    if let Some(output_path) = cli.output.as_ref() {
        let path = resolve_relative_to_root(root, output_path);
        write_json_file(&path, &report)?;
        println!("doctor report path={}", path.display());
    }

    println!("{}", serde_json::to_string_pretty(&report)?);

    if !report["determinism"]["deterministic"]
        .as_bool()
        .unwrap_or(false)
    {
        bail!("doctor detected non-deterministic snapshots")
    }

    Ok(())
}

fn command_health(root: &Path, _state_dir: &Path, cli: &Cli) -> Result<()> {
    let report = run_health_doctor(root, &cli.include)?;

    if let Some(output_path) = cli.output.as_ref() {
        let path = resolve_relative_to_root(root, output_path);
        write_json_file(&path, &report)?;
        println!("health report path={}", path.display());
    }

    println!("{}", serde_json::to_string_pretty(&report)?);

    if !report.healthy {
        bail!("health doctor found blocking issues")
    }

    Ok(())
}

fn command_validate(root: &Path, cli: &Cli) -> Result<()> {
    let report = validate::validate_source(root, &cli.include)?;

    // Print issues in a cargo-style format.
    for issue in &report.issues {
        let severity_tag = match issue.severity.as_str() {
            "error" => "error",
            "warning" => "warning",
            _ => "info",
        };
        let location = if issue.line > 0 {
            format!("{}:{}", issue.path, issue.line)
        } else {
            issue.path.clone()
        };
        println!(
            "{severity_tag}[{}]: {location}: {}",
            issue.rule, issue.message
        );
    }

    // Optionally run selene if available.
    if let Some(selene_output) = validate::run_selene(root, &cli.include) {
        if !selene_output.is_empty() {
            println!();
            println!("--- selene output ---");
            for line in &selene_output {
                println!("{line}");
            }
        }
    }

    println!();
    println!(
        "validate: files={} errors={} warnings={} clean={}",
        report.files_checked, report.errors, report.warnings, report.clean
    );

    if let Some(output_path) = cli.output.as_ref() {
        let path = resolve_relative_to_root(root, output_path);
        write_json_file(&path, &report)?;
        println!("report path={}", path.display());
    }

    if report.errors > 0 {
        bail!(
            "validation failed: {} error(s), {} warning(s)",
            report.errors,
            report.warnings
        )
    }

    Ok(())
}

fn command_watch(root: &Path, state_dir: &Path, cli: &Cli) -> Result<()> {
    let output_dir = cli
        .output
        .as_ref()
        .map(|value| resolve_relative_to_root(root, value))
        .unwrap_or_else(|| state_dir.to_path_buf());

    run_watch(
        root,
        &cli.include,
        Duration::from_secs(cli.interval_seconds.max(1)),
        Some(&output_dir),
    )
}

fn command_watch_native(root: &Path, state_dir: &Path, cli: &Cli) -> Result<()> {
    let output_dir = cli
        .output
        .as_ref()
        .map(|value| resolve_relative_to_root(root, value))
        .unwrap_or_else(|| state_dir.to_path_buf());

    let coalesce_ms = if cli.turbo {
        eprintln!("[vertigo-sync] turbo mode: 10ms coalesce, native fsevents");
        10
    } else {
        cli.coalesce_ms
    };
    run_watch_native(
        root,
        &cli.include,
        Some(&output_dir),
        Duration::from_millis(coalesce_ms),
    )
}

fn command_build(root: &Path, output: &Path, project: &Path) -> Result<()> {
    let project_path = if project.is_absolute() {
        project.to_path_buf()
    } else {
        root.join(project)
    };

    let tree = parse_project(&project_path)?;

    println!("[vertigo-sync] build (dry-run)");
    println!("  project: {}", project_path.display());
    println!("  output:  {}", output.display());
    println!("  name:    {}", tree.name);
    println!("  mappings:");

    for mapping in &tree.mappings {
        println!(
            "    {} -> {} (class={}, ignore_unknown={})",
            mapping.fs_path, mapping.instance_path, mapping.class_name, mapping.ignore_unknown
        );
    }

    // Build a snapshot to show what files would be included.
    let fs_paths: Vec<String> = tree.mappings.iter().map(|m| m.fs_path.clone()).collect();
    let snapshot = build_snapshot(root, &fs_paths)?;
    println!("  source files: {}", snapshot.entries.len());
    println!("  fingerprint:  {}", snapshot.fingerprint);
    println!();
    println!("[vertigo-sync] rbx-dom integration is WIP — dry-run only");

    Ok(())
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
