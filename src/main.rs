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
    use rbx_dom_weak::{InstanceBuilder, WeakDom};
    use vertigo_sync::project::resolve_instance_class;

    let project_path = if project.is_absolute() {
        project.to_path_buf()
    } else {
        root.join(project)
    };

    let tree = parse_project(&project_path)?;

    println!("[vertigo-sync] build");
    println!("  project: {}", project_path.display());
    println!("  output:  {}", output.display());
    println!("  name:    {}", tree.name);
    println!("  mappings: {}", tree.mappings.len());

    // Build the DataModel DOM from source files.
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let data_model_ref = dom.root_ref();

    // Create service containers from project tree mappings.
    // Group mappings by their top-level service (first segment of instance_path).
    let mut service_refs: std::collections::HashMap<String, rbx_dom_weak::types::Ref> =
        std::collections::HashMap::new();

    for mapping in &tree.mappings {
        // Parse instance path: "ServerScriptService.Server" → ["ServerScriptService", "Server"]
        let segments: Vec<&str> = mapping.instance_path.split('.').collect();

        // Ensure each ancestor exists in the DOM.
        let mut parent_ref = data_model_ref;
        for (i, segment) in segments.iter().enumerate() {
            let existing = dom
                .get_by_ref(parent_ref)
                .map(|inst| {
                    inst.children()
                        .iter()
                        .find(|&&child_ref| {
                            dom.get_by_ref(child_ref)
                                .map(|c| c.name == *segment)
                                .unwrap_or(false)
                        })
                        .copied()
                })
                .flatten();

            parent_ref = if let Some(existing_ref) = existing {
                existing_ref
            } else {
                // Determine class name for this segment.
                let class = if i == 0 {
                    // Top-level: use the mapping's class_name or the segment name.
                    mapping.class_name.as_str()
                } else if i == segments.len() - 1 {
                    // Leaf: this is where the $path points, determine from fs_path.
                    let fs_full = root.join(&mapping.fs_path);
                    if fs_full.is_dir() {
                        resolve_container_class(&fs_full)
                    } else {
                        resolve_instance_class(&mapping.fs_path)
                    }
                } else {
                    // Intermediate: use the segment name as class (Roblox service names = class names).
                    segment
                };

                let builder = InstanceBuilder::new(class).with_name(*segment);
                dom.insert(parent_ref, builder)
            };

            service_refs
                .entry(segment.to_string())
                .or_insert(parent_ref);
        }

        // Now populate the leaf container with source files from the filesystem path.
        let fs_full = root.join(&mapping.fs_path);
        if fs_full.is_dir() {
            populate_from_dir(&mut dom, parent_ref, &fs_full, root)?;
        } else if fs_full.is_file() {
            populate_file(&mut dom, parent_ref, &fs_full)?;
        }
    }

    // Count instances.
    let instance_count = count_instances(&dom, data_model_ref);
    println!("  instances: {}", instance_count);

    // Serialize to binary .rbxl format.
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

    // Determine format from extension.
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
    println!(
        "  wrote: {} ({:.1} KB)",
        output_path.display(),
        file_size as f64 / 1024.0
    );
    println!("[vertigo-sync] build complete");

    Ok(())
}

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

    for entry in entries {
        let path = entry.path();
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();

        // Skip hidden files and non-Luau files.
        if name_str.starts_with('.') {
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
                    if let Ok(source) = std::fs::read_to_string(&init_path) {
                        if let Some(inst) = dom.get_by_ref_mut(folder_ref) {
                            inst.properties.insert(
                                "Source".into(),
                                rbx_dom_weak::types::Variant::String(source),
                            );
                        }
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

            // Only process Luau/Lua files.
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext != "luau" && ext != "lua" {
                continue;
            }

            let class = resolve_instance_class(&name_str);
            let instance_name = resolve_instance_name(&name_str);

            let mut builder = rbx_dom_weak::InstanceBuilder::new(class).with_name(instance_name);

            if let Ok(source) = std::fs::read_to_string(&path) {
                builder =
                    builder.with_property("Source", rbx_dom_weak::types::Variant::String(source));
            }

            dom.insert(parent_ref, builder);
        }
    }

    Ok(())
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
    let mut count = 1; // Count the root itself.
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

fn default_event_output_path(state_dir: &Path) -> PathBuf {
    state_dir.join("latest-event.json")
}
