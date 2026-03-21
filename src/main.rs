#![forbid(unsafe_code)]

use anyhow::{Context, Result, bail};
use clap::{ArgAction, CommandFactory, Parser, Subcommand};
use rayon::prelude::*;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use vertigo_sync::errors::SyncError;
use vertigo_sync::output;
use vertigo_sync::plugin_smoke;
use vertigo_sync::project::parse_project;
use vertigo_sync::server::{ServeOptions, run_serve};
use vertigo_sync::validate;
use vertigo_sync::{
    DiffEvent, EventDiffCounts, EventPaths, append_event, build_snapshot, diff_snapshots,
    next_event_seq, read_snapshot, run_doctor, run_health_doctor, run_watch, run_watch_native,
    write_json_file,
};

#[derive(Debug, Parser)]
#[command(
    name = "vsync",
    version,
    about = "Fast, deterministic source sync for Roblox Studio",
    long_about = "Vertigo Sync provides sub-millisecond source synchronization between your \
                  filesystem and Roblox Studio. It replaces Rojo with better performance, \
                  built-in validation, and agent-native MCP tools.",
    after_help = "Examples:\n  \
                  vsync serve                                               Start syncing using the discovered project\n  \
                  vsync serve --turbo                                       Start syncing in turbo mode\n  \
                  vsync serve --project roblox/default.project.json         Serve a nested Roblox project explicitly\n  \
                  vsync discover                                             Print local project/server identity\n  \
                  vsync plugin-smoke-log --log latest.log                  Scan a Studio log for fatal plugin smoke failures\n  \
                  vsync build -o game.rbxl                                  Build a place file\n  \
                  vsync doctor                                              Check project health\n  \
                  vsync init                                                Create a new project\n  \
                  vsync plugin-install                                      Install Studio plugin\n  \
                  vsync plugin-set-icon rbxassetid://1234567890             Stamp installed plugin with a toolbar icon asset\n\n\
                  Learn more: https://github.com/pena/vertigo-sync",
    term_width = 100
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

    #[arg(
        long,
        help = "HTTP port used by serve mode (default: 7575, or servePort from project)"
    )]
    port: Option<u16>,

    #[arg(
        long,
        help = "HTTP bind address used by serve mode (default: 127.0.0.1, or serveAddress from project)"
    )]
    address: Option<String>,

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

#[derive(Debug)]
struct ProjectContext {
    project_path: PathBuf,
    project_root: PathBuf,
    includes: Vec<String>,
    tree: vertigo_sync::project::ProjectTree,
    vsync_config: Option<vertigo_sync::config::VsyncConfig>,
}

#[derive(Debug, Serialize)]
struct DiscoveryProjectReport {
    name: String,
    project_id: String,
    project_path: String,
    project_root: String,
    includes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct DiscoveryServerReport {
    server_url: String,
    reachable: bool,
    server_id: Option<String>,
    project_name: Option<String>,
    project_id: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct DiscoveryReport {
    project: DiscoveryProjectReport,
    server: DiscoveryServerReport,
    matches: bool,
}

#[derive(Debug, Serialize)]
struct ValidateCommandReport {
    source: validate::ValidationReport,
    plugin_safety: validate::PluginSafetyReport,
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
    /// Print the active project identity and, if reachable, the bound server identity.
    #[command(display_order = 12)]
    Discover {
        /// Project file path (default: default.project.json).
        #[arg(long, default_value = "default.project.json")]
        project: PathBuf,
        /// Server base URL to inspect (default: derived from project serveAddress/servePort, or http://127.0.0.1:7575).
        #[arg(long)]
        server_url: Option<String>,
    },
    /// Validate Luau source files for common issues.
    #[command(display_order = 13)]
    Validate {
        /// Project file path (default: default.project.json).
        #[arg(long, default_value = "default.project.json")]
        project: PathBuf,
    },
    /// Scan a Roblox Studio log for fatal Vertigo Sync plugin smoke failures.
    #[command(display_order = 14)]
    PluginSmokeLog {
        /// Path to a Roblox Studio log file.
        #[arg(long)]
        log: PathBuf,
        /// External user_/cloud_ plugins permitted during this smoke run.
        #[arg(long = "allow-plugin", action = ArgAction::Append)]
        allow_plugin: Vec<String>,
        /// Ignore Roblox-managed cloud_ plugin loads unless they emit a separate fatal smoke pattern.
        #[arg(long, default_value_t = false)]
        ignore_cloud_plugins: bool,
    },
    /// Serve snapshot/diff/events over HTTP + SSE.
    #[command(display_order = 20)]
    Serve {
        /// Project file path (default: default.project.json).
        #[arg(long, default_value = "default.project.json")]
        project: PathBuf,
    },
    /// Blocking watch loop that emits NDJSON diff events to stdout.
    #[command(display_order = 21)]
    Watch {
        /// Project file path (default: default.project.json).
        #[arg(long, default_value = "default.project.json")]
        project: PathBuf,
    },
    /// Native filesystem watch using FSEvents/inotify (replaces polling).
    #[command(display_order = 22)]
    WatchNative {
        /// Project file path (default: default.project.json).
        #[arg(long, default_value = "default.project.json")]
        project: PathBuf,
    },
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
    /// Extract scripts from a place file back to the filesystem (rojo syncback equivalent).
    #[command(display_order = 31)]
    Syncback {
        /// Input .rbxl or .rbxlx file.
        #[arg(long, short)]
        input: PathBuf,
        /// Project file for path mapping.
        #[arg(long, default_value = "default.project.json")]
        project: PathBuf,
        /// Dry run — show what would be written without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Generate a Rojo-compatible sourcemap.json for luau-lsp integration.
    ///
    /// Produces a tree-structured JSON file that luau-lsp uses for
    /// require resolution, type checking, and autocomplete across the
    /// entire DataModel — without Rojo running.
    #[command(display_order = 32)]
    Sourcemap {
        /// Output path (default: sourcemap.json).
        #[arg(long, short, default_value = "sourcemap.json")]
        output: PathBuf,
        /// Project file path.
        #[arg(long, default_value = "default.project.json")]
        project: PathBuf,
        /// Include non-script instances (Folders, StringValues, etc.) in the sourcemap.
        #[arg(long, default_value_t = true)]
        include_non_scripts: bool,
        /// Watch for filesystem changes and regenerate automatically.
        #[arg(long, default_value_t = false)]
        watch: bool,
    },
    /// Create a new Vertigo Sync project with standard directory structure.
    #[command(display_order = 40)]
    Init {
        /// Project name (default: current directory name)
        #[arg(long)]
        name: Option<String>,
    },
    /// Install packages from vsync.toml.
    #[command(display_order = 44)]
    Install {
        #[arg(long, default_value = "default.project.json")]
        project: PathBuf,
    },
    /// Add a dependency to vsync.toml.
    #[command(display_order = 45)]
    Add {
        /// Package spec (e.g., "roblox/roact@^17.0.0") or alias followed by spec
        package: Vec<String>,
        /// Add to server-dependencies instead of shared dependencies
        #[arg(long)]
        server: bool,
        /// Add to dev-dependencies instead of shared dependencies
        #[arg(long)]
        dev: bool,
        #[arg(long, default_value = "default.project.json")]
        project: PathBuf,
    },
    /// Remove a dependency from vsync.toml.
    #[command(display_order = 46)]
    Remove {
        /// Package alias name to remove
        package: String,
        #[arg(long, default_value = "default.project.json")]
        project: PathBuf,
    },
    /// Update dependencies.
    #[command(display_order = 47)]
    Update {
        /// Specific package to update (default: all)
        package: Option<String>,
        #[arg(long, default_value = "default.project.json")]
        project: PathBuf,
    },
    /// Migrate from Rojo ecosystem configs to vsync.toml.
    #[command(display_order = 48)]
    Migrate,
    /// Run a project script defined in vsync.toml [scripts].
    #[command(display_order = 49)]
    Run {
        /// Script name.
        name: String,
    },
    /// Open a place file in Roblox Studio.
    #[command(display_order = 50)]
    Open {
        /// Path to .rbxl or .rbxlx file to open (default: build a temp file and open it).
        #[arg(long, short)]
        file: Option<PathBuf>,
        /// Project file path.
        #[arg(long, default_value = "default.project.json")]
        project: PathBuf,
    },
    /// Install the Vertigo Sync Studio plugin.
    #[command(display_order = 41)]
    PluginInstall,
    /// Set the toolbar icon asset on the installed Studio plugin.
    #[command(display_order = 42)]
    PluginSetIcon {
        /// Roblox image asset ID, for example `rbxassetid://1234567890` or `1234567890`.
        asset_id: String,
    },
    /// Format Luau source files.
    #[command(display_order = 43)]
    Fmt {
        /// Check formatting without writing changes (exit 1 if unformatted).
        #[arg(long, default_value_t = false)]
        check: bool,
        /// Print a unified diff for each file that would change.
        #[arg(long, default_value_t = false)]
        diff: bool,
        /// Specific path to format (default: all project includes).
        path: Option<PathBuf>,
        /// Project file path.
        #[arg(long, default_value = "default.project.json")]
        project: PathBuf,
    },
    /// Generate shell completion scripts.
    #[command(display_order = 90)]
    Completions {
        /// Shell to generate completions for.
        shell: clap_complete::Shell,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let json_mode = cli.json;
    let result = run_cli(cli).await;
    if let Err(e) = result {
        if json_mode {
            let err_json = serde_json::json!({ "error": format!("{e:#}") });
            eprintln!("{}", serde_json::to_string(&err_json).unwrap_or_default());
        } else {
            eprintln!("error: {e:#}");
        }
        std::process::exit(1);
    }
}

async fn run_cli(cli: Cli) -> Result<()> {
    let root = resolve_root(&cli.root)?;
    let state_dir = resolve_relative_to_root(&root, &cli.state_dir);

    match &cli.command {
        Command::Snapshot => command_snapshot(&root, &state_dir, &cli),
        Command::Diff => command_diff(&root, &state_dir, &cli),
        Command::Event => command_event(&root, &state_dir, &cli),
        Command::Doctor => command_doctor(&root, &state_dir, &cli),
        Command::Health => command_health(&root, &state_dir, &cli),
        Command::Discover {
            project,
            server_url,
        } => command_discover(&root, project, server_url.as_deref(), &cli).await,
        Command::Watch { project } => command_watch(&root, &state_dir, project, &cli),
        Command::WatchNative { project } => command_watch_native(&root, &state_dir, project, &cli),
        Command::Validate { project } => command_validate(&root, project, &cli),
        Command::PluginSmokeLog {
            log,
            allow_plugin,
            ignore_cloud_plugins,
        } => {
            command_plugin_smoke_log(log, allow_plugin, *ignore_cloud_plugins, &cli)
        }
        Command::Build {
            output,
            project,
            binary_models,
        } => command_build(&root, output, project, *binary_models),
        Command::Syncback {
            input,
            project,
            dry_run,
        } => command_syncback(&root, input, project, *dry_run),
        Command::Sourcemap {
            output,
            project,
            include_non_scripts,
            watch,
        } => command_sourcemap(&root, output, project, *include_non_scripts, *watch, &cli).await,
        Command::Init { name } => command_init(&root, name.as_deref()),
        Command::Migrate => command_migrate(&root),
        Command::Run { name } => {
            let project_context = resolve_project_context(
                &root,
                Path::new("default.project.json"),
                &cli.include,
            )?;
            let scripts = project_context
                .vsync_config
                .as_ref()
                .map(|c| &c.scripts);
            let empty = std::collections::BTreeMap::new();
            let scripts = scripts.unwrap_or(&empty);
            match vertigo_sync::scripts::resolve_script(name, scripts) {
                Some(command) => {
                    let project_name = &project_context.tree.name;
                    let code = vertigo_sync::scripts::run_script(
                        name,
                        &command,
                        &project_context.project_root,
                        project_name,
                    )?;
                    std::process::exit(code);
                }
                None => {
                    if scripts.is_empty() {
                        anyhow::bail!(
                            "no script '{name}' found — no [scripts] defined in vsync.toml"
                        );
                    } else {
                        let available: Vec<&str> = scripts.keys().map(|s| s.as_str()).collect();
                        anyhow::bail!(
                            "unknown script '{name}'. Available scripts: {}",
                            available.join(", ")
                        );
                    }
                }
            }
        }
        Command::Open { file, project } => {
            let place_path = if let Some(f) = file {
                if f.is_absolute() {
                    f.clone()
                } else {
                    root.join(f)
                }
            } else {
                // Build a temp place file
                let ctx = resolve_project_context(&root, project, &cli.include)?;
                let temp_path = ctx
                    .project_root
                    .join(".vertigo-sync-state")
                    .join("preview.rbxl");
                std::fs::create_dir_all(temp_path.parent().unwrap())?;
                command_build(&root, &temp_path, project, false)?;
                output::info(&format!("Built {}", temp_path.display()));
                temp_path
            };

            if !place_path.exists() {
                bail!("place file not found: {}", place_path.display());
            }

            #[cfg(target_os = "macos")]
            {
                std::process::Command::new("open")
                    .arg(&place_path)
                    .spawn()
                    .context("failed to open place file — is Roblox Studio installed?")?;
            }
            #[cfg(target_os = "windows")]
            {
                std::process::Command::new("cmd")
                    .args(["/C", "start", ""])
                    .arg(&place_path)
                    .spawn()
                    .context("failed to open place file — is Roblox Studio installed?")?;
            }
            #[cfg(target_os = "linux")]
            {
                std::process::Command::new("xdg-open")
                    .arg(&place_path)
                    .spawn()
                    .context("failed to open place file — is xdg-open installed?")?;
            }

            output::success(&format!("Opened {}", place_path.display()));
            Ok(())
        }
        Command::PluginInstall => command_plugin_install(),
        Command::PluginSetIcon { asset_id } => command_plugin_set_icon(asset_id),
        Command::Fmt {
            check,
            diff,
            path,
            project,
        } => command_fmt(&root, project, path.as_deref(), *check, *diff, &cli),
        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(*shell, &mut cmd, "vsync", &mut std::io::stdout());
            Ok(())
        }
        Command::Install { project } => {
            let ctx = resolve_project_context(&root, project, &cli.include)?;
            let config = ctx.vsync_config.clone().unwrap_or_default();
            let _report =
                vertigo_sync::package::installer::install(&ctx.project_root, &config).await?;
            Ok(())
        }
        Command::Add { package, server, dev, project } => {
            use vertigo_sync::config::{DependencySpec, save_config};
            use vertigo_sync::package::registry::parse_version_req;

            // Parse positional args: either `<spec>` or `<alias> <spec>`
            let (alias, spec) = match package.len() {
                1 => {
                    let spec = &package[0];
                    let (_scope, name, _ver) = parse_version_req(spec)
                        .with_context(|| format!("invalid package spec: {spec}"))?;
                    (name, spec.clone())
                }
                2 => (package[0].clone(), package[1].clone()),
                _ => bail!("usage: vsync add [<alias>] <spec>  (e.g. vsync add roblox/roact@^17.0.0)"),
            };

            // Validate the spec parses correctly
            let (_scope, _name, _ver) = parse_version_req(&spec)
                .with_context(|| format!("invalid package spec: {spec}"))?;

            let ctx = resolve_project_context(&root, project, &cli.include)?;
            let mut config = ctx.vsync_config.clone().unwrap_or_default();

            // Add to the appropriate dependency map
            if *server {
                config.server_dependencies.insert(alias.clone(), DependencySpec::Simple(spec.clone()));
                output::success(&format!("added {alias} = \"{spec}\" to [server-dependencies]"));
            } else if *dev {
                config.dev_dependencies.insert(alias.clone(), DependencySpec::Simple(spec.clone()));
                output::success(&format!("added {alias} = \"{spec}\" to [dev-dependencies]"));
            } else {
                config.dependencies.insert(alias.clone(), DependencySpec::Simple(spec.clone()));
                output::success(&format!("added {alias} = \"{spec}\" to [dependencies]"));
            }

            save_config(&ctx.project_root, &config)?;

            // Run install
            let _report =
                vertigo_sync::package::installer::install(&ctx.project_root, &config).await?;
            Ok(())
        }
        Command::Remove { package, project } => {
            use vertigo_sync::config::save_config;
            use vertigo_sync::package::lockfile::Lockfile;

            let ctx = resolve_project_context(&root, project, &cli.include)?;
            let mut config = ctx.vsync_config.clone().unwrap_or_default();

            let had_shared = config.dependencies.remove(package).is_some();
            let had_server = config.server_dependencies.remove(package).is_some();
            let had_dev = config.dev_dependencies.remove(package).is_some();

            if !had_shared && !had_server && !had_dev {
                bail!("dependency '{package}' not found in vsync.toml");
            }

            save_config(&ctx.project_root, &config)?;

            // Remove from Packages/ directory
            let packages_dir = ctx.project_root.join(
                config.package.packages_dir.as_deref().unwrap_or("Packages"),
            );
            // The package could be under any scope, so search for directories named `package`
            if packages_dir.is_dir() {
                if let Ok(entries) = std::fs::read_dir(&packages_dir) {
                    for entry in entries.flatten() {
                        let pkg_path = entry.path().join(package);
                        if pkg_path.is_dir() {
                            std::fs::remove_dir_all(&pkg_path).with_context(|| {
                                format!("failed to remove {}", pkg_path.display())
                            })?;
                        }
                    }
                }
            }

            // Update vsync.lock — remove entries whose name ends with /{package}
            let lock_path = ctx.project_root.join("vsync.lock");
            if let Some(mut lockfile) = Lockfile::load(&lock_path)? {
                let suffix = format!("/{package}");
                lockfile.packages.retain(|p| {
                    !p.name.ends_with(&suffix) && p.name != *package
                });
                lockfile.save(&lock_path)?;
            }

            output::success(&format!("removed {package}"));
            Ok(())
        }
        Command::Update { package, project } => {
            let ctx = resolve_project_context(&root, project, &cli.include)?;
            let config = ctx.vsync_config.clone().unwrap_or_default();
            let lock_path = ctx.project_root.join("vsync.lock");

            if let Some(pkg_name) = &package {
                // Update specific package: remove from lockfile
                if let Some(mut lockfile) = vertigo_sync::package::lockfile::Lockfile::load(&lock_path)? {
                    let before = lockfile.packages.len();
                    lockfile.packages.retain(|p| {
                        // Match by alias or full name
                        !p.name.ends_with(&format!("/{pkg_name}")) && p.name != *pkg_name
                    });
                    let removed = before - lockfile.packages.len();
                    if removed > 0 {
                        lockfile.save(&lock_path)?;
                        output::info(&format!("Removed {pkg_name} from lockfile, re-resolving..."));
                    } else {
                        output::warn(&format!("{pkg_name} not found in lockfile"));
                    }
                }
            } else {
                // Update all: delete lockfile entirely
                if lock_path.exists() {
                    std::fs::remove_file(&lock_path)?;
                    output::info("Removed lockfile, re-resolving all dependencies...");
                }
            }

            let report = vertigo_sync::package::installer::install(&ctx.project_root, &config).await?;
            output::success(&format!(
                "{} installed, {} cached, {} total",
                report.installed, report.cached, report.total
            ));
            Ok(())
        }
        Command::Serve { project } => {
            let project_context = resolve_project_context(&root, project, &cli.include)?;
            let includes = project_context.includes.clone();
            let (interval, coalesce_ms) = if cli.turbo {
                (Duration::from_millis(100), 10u64)
            } else {
                (
                    Duration::from_secs(cli.interval_seconds.max(1)),
                    cli.coalesce_ms,
                )
            };

            // Resolve port and address: CLI flags > project file > defaults.
            let port = cli.port.or(project_context.tree.serve_port).unwrap_or(7575);
            let address = cli
                .address
                .clone()
                .or_else(|| project_context.tree.serve_address.clone())
                .unwrap_or_else(|| "127.0.0.1".to_string());

            let mode = if cli.turbo {
                "turbo (10ms coalesce)"
            } else {
                "standard"
            };
            let version = env!("CARGO_PKG_VERSION");
            let http_addr = format!("http://{address}:{port}");
            let ws_addr = format!("ws://{address}:{port}/ws");
            let watching = includes.join(", ");
            let project_display = project_context.project_path.display().to_string();

            output::banner(
                version,
                &[
                    ("Server", &http_addr),
                    ("WebSocket", &ws_addr),
                    ("Mode", mode),
                    ("Project", &project_display),
                    ("Watching", &watching),
                ],
            );

            run_serve(ServeOptions {
                root: project_context.project_root,
                project_path: project_context.project_path,
                includes,
                port,
                interval,
                channel_capacity: cli.channel_capacity,
                coalesce_ms,
                turbo: cli.turbo,
                address,
            })
            .await
        }
    }
}

// ---------------------------------------------------------------------------
// Include resolution
// ---------------------------------------------------------------------------

/// Resolve effective include paths: use CLI values if provided, else try to
/// auto-detect from the selected project file `$path` entries, else fall back
/// to `["src"]`.
fn resolve_effective_includes(project_path: &Path, cli_includes: &[String]) -> Vec<String> {
    if !cli_includes.is_empty() {
        return cli_includes.to_vec();
    }

    // Try auto-detect from the selected project file only.
    if project_path.is_file() {
        if let Ok(content) = std::fs::read_to_string(project_path) {
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

#[derive(Debug)]
struct HttpTarget {
    host: String,
    port: u16,
    path_prefix: String,
}

fn parse_http_url(raw: &str) -> Result<HttpTarget> {
    let trimmed = raw.trim().trim_end_matches('/');
    let without_scheme = trimmed
        .strip_prefix("http://")
        .with_context(|| format!("unsupported discovery URL scheme: {trimmed}"))?;

    let (authority, path_prefix) = match without_scheme.split_once('/') {
        Some((left, right)) => (left, format!("/{}", right.trim_matches('/'))),
        None => (without_scheme, String::new()),
    };

    if authority.is_empty() {
        bail!("missing host in discovery URL: {trimmed}");
    }

    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let end = rest
            .find(']')
            .with_context(|| format!("invalid bracketed host in discovery URL: {trimmed}"))?;
        let host = &rest[..end];
        let port_str = rest[end + 1..]
            .strip_prefix(':')
            .with_context(|| format!("missing port in discovery URL: {trimmed}"))?;
        let port = port_str
            .parse::<u16>()
            .with_context(|| format!("invalid port in discovery URL: {trimmed}"))?;
        (host.to_string(), port)
    } else {
        let (host, port_str) = authority
            .rsplit_once(':')
            .with_context(|| format!("missing port in discovery URL: {trimmed}"))?;
        if host.is_empty() {
            bail!("missing host in discovery URL: {trimmed}");
        }
        let port = port_str
            .parse::<u16>()
            .with_context(|| format!("invalid port in discovery URL: {trimmed}"))?;
        (host.to_string(), port)
    };

    Ok(HttpTarget {
        host,
        port,
        path_prefix,
    })
}

async fn fetch_json_http(base_url: &str, endpoint: &str) -> Result<serde_json::Value> {
    let target = parse_http_url(base_url)?;
    let endpoint = endpoint.trim_start_matches('/');
    let path = if target.path_prefix.is_empty() {
        format!("/{}", endpoint)
    } else {
        format!("{}/{}", target.path_prefix, endpoint)
    };
    let url = format!("http://{}:{}{}", target.host, target.port, path);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .context("failed to build HTTP client")?;
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("failed to fetch {url}"))?;
    if !resp.status().is_success() {
        bail!("HTTP {} from {url}", resp.status());
    }
    resp.json().await.context("failed to parse JSON response")
}

fn resolve_project_context(
    root: &Path,
    project: &Path,
    cli_includes: &[String],
) -> Result<ProjectContext> {
    let project_path = if project.is_absolute() {
        project.to_path_buf()
    } else {
        root.join(project)
    };
    let project_path = resolve_project_path(root, project, &project_path)?;
    let tree = parse_project(&project_path)?;
    let includes = resolve_effective_includes(&project_path, cli_includes);
    let project_root = project_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.to_path_buf());

    let vsync_config = vertigo_sync::config::load_config(&project_root)?;

    Ok(ProjectContext {
        project_path,
        project_root,
        includes,
        tree,
        vsync_config,
    })
}

fn resolve_project_path(root: &Path, requested: &Path, candidate: &Path) -> Result<PathBuf> {
    if candidate.is_file() {
        return Ok(candidate.to_path_buf());
    }

    if requested != Path::new("default.project.json") {
        bail!("project file not found: {}", candidate.display());
    }

    if let Some(discovered) = discover_single_project_path(root)? {
        return Ok(discovered);
    }

    bail!(
        "project file not found: {} (and no unambiguous nested default.project.json was discovered under {})",
        candidate.display(),
        root.display()
    );
}

fn discover_single_project_path(root: &Path) -> Result<Option<PathBuf>> {
    let mut matches = Vec::new();
    collect_project_files(root, root, 0, &mut matches)?;

    if matches.is_empty() {
        return Ok(None);
    }

    if matches.len() == 1 {
        return Ok(matches.into_iter().next());
    }

    let rendered = matches
        .iter()
        .map(|path| {
            path.strip_prefix(root)
                .unwrap_or(path)
                .display()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "multiple nested project files found under {}: {}. Pass --project explicitly",
        root.display(),
        rendered
    );
}

fn collect_project_files(
    root: &Path,
    current: &Path,
    depth: usize,
    out: &mut Vec<PathBuf>,
) -> Result<()> {
    if depth > 4 {
        return Ok(());
    }

    for entry in std::fs::read_dir(current)
        .with_context(|| format!("failed to read {}", current.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if file_type.is_dir() {
            if should_skip_project_search_dir(root, &path, &name) {
                continue;
            }
            collect_project_files(root, &path, depth + 1, out)?;
            continue;
        }

        if file_type.is_file() && name == "default.project.json" {
            out.push(path);
        }
    }

    out.sort();
    Ok(())
}

fn should_skip_project_search_dir(root: &Path, path: &Path, name: &str) -> bool {
    if name.starts_with('.') {
        return true;
    }

    matches!(
        name,
        "target" | "node_modules" | "Packages" | "DevPackages" | "build" | "dist" | "out" | ".git"
    ) || path == root.join("target")
}

/// Recursively collect `$path` values from a Rojo/Vertigo project tree.
fn collect_dollar_paths(obj: &serde_json::Map<String, serde_json::Value>, out: &mut Vec<String>) {
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
    let includes = resolve_effective_includes(&root.join("default.project.json"), &cli.include);
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
    let includes = resolve_effective_includes(&root.join("default.project.json"), &cli.include);
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
    let includes = resolve_effective_includes(&root.join("default.project.json"), &cli.include);
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
    let project_context =
        resolve_project_context(root, Path::new("default.project.json"), &cli.include)?;
    let determinism = run_doctor(&project_context.project_root, &project_context.includes)?;
    let health = run_health_doctor(&project_context.project_root, &project_context.includes)?;
    let project_report = serde_json::json!({
        "name": project_context.tree.name,
        "project_id": project_context.tree.project_id,
        "project_path": project_context.project_path,
        "project_root": project_context.project_root,
        "includes": project_context.includes,
    });

    if cli.json {
        let report = serde_json::json!({
            "project": project_report,
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

        output::header("Project");
        output::kv("Name", &project_context.tree.name);
        output::kv("Project ID", &project_context.tree.project_id);
        output::kv(
            "Project File",
            &project_context.project_path.display().to_string(),
        );
        output::kv(
            "Project Root",
            &project_context.project_root.display().to_string(),
        );
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
                "project": project_report,
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
    let project_context =
        resolve_project_context(root, Path::new("default.project.json"), &cli.include)?;
    let report = run_health_doctor(&project_context.project_root, &project_context.includes)?;

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

fn discover_server_url(project_context: &ProjectContext, override_url: Option<&str>) -> String {
    if let Some(server_url) = override_url {
        return server_url.to_string();
    }

    let address = project_context
        .tree
        .serve_address
        .as_deref()
        .unwrap_or("127.0.0.1");
    let port = project_context.tree.serve_port.unwrap_or(7575);
    format!("http://{address}:{port}")
}

async fn command_discover(
    root: &Path,
    project: &Path,
    server_url: Option<&str>,
    cli: &Cli,
) -> Result<()> {
    let project_context = resolve_project_context(root, project, &cli.include)?;
    let server_url = discover_server_url(&project_context, server_url);
    let project_report = DiscoveryProjectReport {
        name: project_context.tree.name.clone(),
        project_id: project_context.tree.project_id.clone(),
        project_path: project_context.project_path.display().to_string(),
        project_root: project_context.project_root.display().to_string(),
        includes: project_context.includes.clone(),
    };

    let server_result = fetch_json_http(&server_url, "/discover").await;
    let server_report = match server_result {
        Ok(value) => DiscoveryServerReport {
            server_url: server_url.clone(),
            reachable: true,
            server_id: value
                .get("server_id")
                .and_then(|v| v.as_str())
                .map(|v| v.to_string()),
            project_name: value
                .get("project_name")
                .and_then(|v| v.as_str())
                .map(|v| v.to_string()),
            project_id: value
                .get("project_id")
                .and_then(|v| v.as_str())
                .map(|v| v.to_string()),
            error: None,
        },
        Err(err) => DiscoveryServerReport {
            server_url,
            reachable: false,
            server_id: None,
            project_name: None,
            project_id: None,
            error: Some(err.to_string()),
        },
    };

    let matches = server_report
        .project_id
        .as_ref()
        .is_some_and(|server_project_id| server_project_id == &project_report.project_id);

    let report = DiscoveryReport {
        project: project_report,
        server: server_report,
        matches,
    };

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        output::header("Vertigo Sync Discovery");
        output::kv("Project", &report.project.name);
        output::kv("Project ID", &report.project.project_id);
        output::kv("Project Path", &report.project.project_path);
        output::kv("Project Root", &report.project.project_root);
        output::kv("Server URL", &report.server.server_url);
        if report.server.reachable {
            output::success("Server reachable");
            if let Some(ref server_id) = report.server.server_id {
                output::kv("Server ID", server_id);
            }
            if let Some(ref server_project) = report.server.project_name {
                output::kv("Server Project", server_project);
            }
            if let Some(ref server_project_id) = report.server.project_id {
                output::kv("Server Project ID", server_project_id);
            }
            if report.matches {
                output::success("Project identity matches server");
            } else {
                output::warn("Project identity does not match server");
            }
        } else if let Some(ref err) = report.server.error {
            output::warn(&format!("Server unreachable: {err}"));
        }
    }

    Ok(())
}

fn command_validate(root: &Path, project: &Path, cli: &Cli) -> Result<()> {
    let project_context =
        resolve_project_context(root, project, &cli.include)?;
    let mut report = validate::validate_source_with_ignores(
        &project_context.project_root,
        &project_context.includes,
        &project_context.tree.glob_ignore_paths,
    )?;

    // Apply .vsyncignore patterns — remove issues for ignored files.
    let ignore_patterns = load_ignore_patterns(&project_context.project_root);
    if !ignore_patterns.is_empty() {
        report.issues.retain(|issue| {
            !ignore_patterns.iter().any(|p| p.matches(&issue.path))
        });
        report.errors = report.issues.iter().filter(|i| i.severity == "error").count();
        report.warnings = report.issues.iter().filter(|i| i.severity == "warning").count();
        report.clean = report.errors == 0 && report.warnings == 0;
    }

    // Wire existing validate.rs rules into the configurable [lint] config from
    // vsync.toml.  Rules set to "off" are dropped; severity can be escalated
    // or downgraded via "error" / "warn".
    if let Some(ref config) = project_context.vsync_config {
        report.issues.retain(|issue| {
            config.lint.get(&issue.rule).map(|s| s.as_str()) != Some("off")
        });
        for issue in &mut report.issues {
            if let Some(configured) = config.lint.get(&issue.rule) {
                match configured.as_str() {
                    "error" => issue.severity = "error".to_string(),
                    "warn" => issue.severity = "warning".to_string(),
                    _ => {}
                }
            }
        }
        // Recompute summary counts after filtering and severity changes.
        report.errors = report.issues.iter().filter(|i| i.severity == "error").count();
        report.warnings = report.issues.iter().filter(|i| i.severity == "warning").count();
        report.clean = report.errors == 0 && report.warnings == 0;
    }

    let plugin_safety =
        validate::validate_plugin_source_text("embedded://VertigoSyncPlugin.lua", PLUGIN_SOURCE)?;
    let combined = ValidateCommandReport {
        source: report.clone(),
        plugin_safety: plugin_safety.clone(),
    };

    if cli.json {
        println!("{}", serde_json::to_string(&combined)?);
        if let Some(output_path) = cli.output.as_ref() {
            let path = resolve_relative_to_root(root, output_path);
            write_json_file(&path, &combined)?;
        }
        if report.errors > 0 || !plugin_safety.errors.is_empty() {
            bail!(
                "validation failed: {} source error(s), {} source warning(s), {} plugin safety error(s)",
                report.errors,
                report.warnings,
                plugin_safety.errors.len()
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
    if let Some(selene_output) =
        validate::run_selene(&project_context.project_root, &project_context.includes)
    {
        if !selene_output.is_empty() {
            eprintln!();
            output::header("selene output");
            for line in &selene_output {
                output::info(line);
            }
        }
        output::warn("Selene passthrough is deprecated and will be removed in v1.0. Built-in lint rules will replace it.");
    }

    // Built-in configurable lint pass.
    {
        let lint_config = project_context
            .vsync_config
            .as_ref()
            .map(|c| c.lint.clone())
            .unwrap_or_default();
        let lint_issues = vertigo_sync::lint::lint_source_tree_with_ignores(
            &project_context.project_root,
            &project_context.includes,
            &lint_config,
            &ignore_patterns,
        );
        if !lint_issues.is_empty() {
            eprintln!();
            output::header("lint");
            let mut lint_errors = 0usize;
            let mut lint_warnings = 0usize;
            for issue in &lint_issues {
                let msg = format!(
                    "{}:{}: {} [{}] {}",
                    issue.file, issue.line, issue.severity, issue.rule, issue.message
                );
                match issue.severity {
                    vertigo_sync::lint::LintSeverity::Error => {
                        output::error_msg(&msg);
                        lint_errors += 1;
                    }
                    vertigo_sync::lint::LintSeverity::Warning => {
                        output::warn(&msg);
                        lint_warnings += 1;
                    }
                }
            }
            eprintln!();
            output::info(&format!(
                "lint: {} error(s), {} warning(s)",
                lint_errors, lint_warnings
            ));
        }
    }

    eprintln!();
    output::header("plugin safety");
    output::info(&format!(
        "top-level symbols: {} / {}",
        plugin_safety.top_level_symbol_count, plugin_safety.top_level_symbol_budget
    ));
    if let Some(top_finding) = plugin_safety.function_risk_findings.first() {
        output::info(&format!(
            "highest function risk: `{}` lines {}-{} score {}",
            top_finding.name, top_finding.start_line, top_finding.end_line, top_finding.risk_score
        ));
    }
    for warning in &plugin_safety.warnings {
        output::warn(&format!("[{}]: {}", warning.rule, warning.message));
    }
    for error in &plugin_safety.errors {
        output::error_msg(&format!("[{}]: {}", error.rule, error.message));
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

    if plugin_safety.errors.is_empty() {
        output::success("generated plugin passed safety validation");
    } else {
        output::error_msg(&format!(
            "generated plugin failed safety validation with {} error(s)",
            plugin_safety.errors.len()
        ));
    }

    if let Some(output_path) = cli.output.as_ref() {
        let path = resolve_relative_to_root(root, output_path);
        write_json_file(&path, &combined)?;
    }

    if report.errors > 0 || !plugin_safety.errors.is_empty() {
        let err = SyncError::ValidationFailed {
            errors: report.errors + plugin_safety.errors.len(),
            warnings: report.warnings,
        };
        output::error_msg(&err.suggestion());
        bail!(
            "validation failed: {} source error(s), {} source warning(s), {} plugin safety error(s)",
            report.errors,
            report.warnings,
            plugin_safety.errors.len()
        )
    }

    Ok(())
}

fn load_ignore_patterns(project_root: &Path) -> Vec<glob::Pattern> {
    let ignore_path = project_root.join(".vsyncignore");
    if !ignore_path.exists() {
        return Vec::new();
    }
    let content = match std::fs::read_to_string(&ignore_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .filter_map(|line| glob::Pattern::new(line.trim()).ok())
        .collect()
}

fn command_fmt(
    root: &Path,
    project: &Path,
    path: Option<&Path>,
    check: bool,
    diff: bool,
    cli: &Cli,
) -> Result<()> {
    let project_context = resolve_project_context(root, project, &cli.include)?;
    let format_config = project_context
        .vsync_config
        .as_ref()
        .map(|c| c.format.clone())
        .unwrap_or_default();

    // Collect files to format.
    let files: Vec<std::path::PathBuf> = if let Some(p) = path {
        let resolved = if p.is_absolute() {
            p.to_path_buf()
        } else {
            project_context.project_root.join(p)
        };
        if resolved.is_file() {
            vec![resolved]
        } else if resolved.is_dir() {
            vertigo_sync::fmt::collect_lua_files(&resolved)?
        } else {
            anyhow::bail!("path does not exist: {}", resolved.display());
        }
    } else {
        let mut all = Vec::new();
        for inc in &project_context.includes {
            let inc_path = project_context.project_root.join(inc);
            if inc_path.is_dir() {
                all.extend(vertigo_sync::fmt::collect_lua_files(&inc_path)?);
            } else if inc_path.is_file()
                && (inc.ends_with(".luau") || inc.ends_with(".lua"))
            {
                all.push(inc_path);
            }
        }
        all
    };

    // Apply .vsyncignore patterns.
    let ignore_patterns = load_ignore_patterns(&project_context.project_root);
    let mut files = files;
    if !ignore_patterns.is_empty() {
        files.retain(|path| {
            let rel = path
                .strip_prefix(&project_context.project_root)
                .unwrap_or(path);
            let rel_str = rel.to_string_lossy();
            !ignore_patterns.iter().any(|p| p.matches(&rel_str))
        });
    }

    if files.is_empty() {
        if !cli.json {
            vertigo_sync::output::header("Fmt");
            eprintln!("  No .luau/.lua files found.");
        }
        return Ok(());
    }

    // Process all files in parallel using rayon.
    let results: Vec<(PathBuf, Result<bool, String>)> = files
        .par_iter()
        .map(|path| {
            let source = match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(e) => return (path.clone(), Err(format!("{e}"))),
            };
            match vertigo_sync::fmt::format_source(&source, &format_config) {
                Ok(formatted) => {
                    if formatted == source {
                        (path.clone(), Ok(false))
                    } else if check {
                        (path.clone(), Ok(true)) // would change
                    } else {
                        match std::fs::write(path, &formatted) {
                            Ok(_) => (path.clone(), Ok(true)),
                            Err(e) => (path.clone(), Err(format!("{e}"))),
                        }
                    }
                }
                Err(e) => (path.clone(), Err(format!("{e}"))),
            }
        })
        .collect();

    // Aggregate results.
    let checked_count = results.len();
    let mut changed_count: usize = 0;
    let mut errors: Vec<String> = Vec::new();

    for (file_path, result) in &results {
        match result {
            Ok(true) => {
                changed_count += 1;
                if diff {
                    // Re-read for diff output (only in diff mode).
                    let rel = file_path
                        .strip_prefix(&project_context.project_root)
                        .unwrap_or(file_path)
                        .display()
                        .to_string();
                    if let Ok(source) = std::fs::read_to_string(file_path) {
                        if let Ok(formatted) =
                            vertigo_sync::fmt::format_source(&source, &format_config)
                        {
                            print!("{}", unified_diff(&source, &formatted, &rel));
                        }
                    }
                }
                if check {
                    let rel = file_path
                        .strip_prefix(&project_context.project_root)
                        .unwrap_or(file_path)
                        .display();
                    eprintln!("  would change: {rel}");
                }
            }
            Ok(false) => {}
            Err(e) => {
                let rel = file_path
                    .strip_prefix(&project_context.project_root)
                    .unwrap_or(file_path)
                    .display();
                errors.push(format!("{rel}: {e}"));
            }
        }
    }

    if cli.json {
        let report = serde_json::json!({
            "files_checked": checked_count,
            "files_changed": changed_count,
            "errors": errors,
        });
        println!("{}", serde_json::to_string(&report)?);
    } else {
        vertigo_sync::output::header("Fmt");
        eprintln!("  {checked_count} file(s) checked, {changed_count} file(s) {}.",
            if check || diff { "would change" } else { "formatted" }
        );
        for err in &errors {
            eprintln!("  error: {err}");
        }
    }

    if check && changed_count > 0 {
        anyhow::bail!("{changed_count} file(s) are not formatted — run `vsync fmt` to fix");
    }

    Ok(())
}

fn unified_diff(old: &str, new: &str, file_name: &str) -> String {
    use similar::TextDiff;
    let diff = TextDiff::from_lines(old, new);
    diff.unified_diff()
        .header(&format!("a/{file_name}"), &format!("b/{file_name}"))
        .context_radius(3)
        .to_string()
}

fn command_plugin_smoke_log(
    log: &Path,
    allow_plugins: &[String],
    ignore_cloud_plugins: bool,
    cli: &Cli,
) -> Result<()> {
    let report =
        plugin_smoke::scan_studio_log_file_with_options(log, allow_plugins, ignore_cloud_plugins)?;

    if cli.json {
        println!("{}", serde_json::to_string(&report)?);
        if !report.clean {
            plugin_smoke::ensure_clean_log(&report)?;
        }
        return Ok(());
    }

    if report.clean {
        output::success(&format!(
            "Studio plugin smoke passed for {}",
            report.log_path
        ));
        return Ok(());
    }

    output::error_msg(&format!(
        "Studio plugin smoke failed for {}",
        report.log_path
    ));
    for fatal in &report.fatal_matches {
        output::error_msg(&format!(
            "[{}]: {}:{} {}",
            fatal.rule, report.log_path, fatal.line, fatal.text
        ));
    }
    plugin_smoke::ensure_clean_log(&report)
}

fn command_watch(root: &Path, state_dir: &Path, project: &Path, cli: &Cli) -> Result<()> {
    let project_context = resolve_project_context(root, project, &cli.include)?;
    let default_output_dir = if cli.state_dir.is_absolute() {
        state_dir.to_path_buf()
    } else {
        project_context.project_root.join(&cli.state_dir)
    };
    let output_dir = cli
        .output
        .as_ref()
        .map(|value| resolve_relative_to_root(&project_context.project_root, value))
        .unwrap_or(default_output_dir);

    run_watch(
        &project_context.project_root,
        &project_context.includes,
        Duration::from_secs(cli.interval_seconds.max(1)),
        Some(&output_dir),
    )
}

fn command_watch_native(root: &Path, state_dir: &Path, project: &Path, cli: &Cli) -> Result<()> {
    let project_context = resolve_project_context(root, project, &cli.include)?;
    let default_output_dir = if cli.state_dir.is_absolute() {
        state_dir.to_path_buf()
    } else {
        project_context.project_root.join(&cli.state_dir)
    };
    let output_dir = cli
        .output
        .as_ref()
        .map(|value| resolve_relative_to_root(&project_context.project_root, value))
        .unwrap_or(default_output_dir);

    let coalesce_ms = if cli.turbo {
        output::info("turbo mode: 10ms coalesce, native fsevents");
        10
    } else {
        cli.coalesce_ms
    };
    run_watch_native(
        &project_context.project_root,
        &project_context.includes,
        Some(&output_dir),
        Duration::from_millis(coalesce_ms),
    )
}

fn command_build(root: &Path, output: &Path, project: &Path, _binary_models: bool) -> Result<()> {
    use rbx_dom_weak::{InstanceBuilder, WeakDom};
    use vertigo_sync::project::resolve_instance_class;

    let project_context = resolve_project_context(root, project, &[])?;
    let project_path = project_context.project_path;
    let project_root = project_context.project_root;
    let tree = project_context.tree;

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
            let existing = dom.get_by_ref(parent_ref).and_then(|inst| {
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
                    let fs_full = project_root.join(&mapping.fs_path);
                    if fs_full.is_dir() {
                        resolve_container_class(&fs_full)
                    } else {
                        resolve_instance_class(&mapping.fs_path)
                    }
                } else {
                    segment
                };

                let mut builder = InstanceBuilder::new(class).with_name(*segment);

                // Apply $properties from the mapping to the leaf instance.
                if i == segments.len() - 1 {
                    if let Some(ref props) = mapping.properties {
                        for (key, value) in props {
                            match value {
                                serde_json::Value::Bool(b) => {
                                    builder = builder.with_property(
                                        key.as_str(),
                                        rbx_dom_weak::types::Variant::Bool(*b),
                                    );
                                }
                                serde_json::Value::String(s) => {
                                    builder = builder.with_property(
                                        key.as_str(),
                                        rbx_dom_weak::types::Variant::String(s.clone()),
                                    );
                                }
                                serde_json::Value::Number(n) => {
                                    if let Some(i) = n.as_i64() {
                                        builder = builder.with_property(
                                            key.as_str(),
                                            rbx_dom_weak::types::Variant::Int32(i as i32),
                                        );
                                    } else if let Some(f) = n.as_f64() {
                                        builder = builder.with_property(
                                            key.as_str(),
                                            rbx_dom_weak::types::Variant::Float64(f),
                                        );
                                    }
                                }
                                // TODO: support array/object property types (e.g. Color3, Vector3)
                                _ => {}
                            }
                        }
                    }
                }

                dom.insert(parent_ref, builder)
            };

            service_refs
                .entry(segment.to_string())
                .or_insert(parent_ref);
        }

        let fs_full = project_root.join(&mapping.fs_path);
        if fs_full.is_dir() {
            populate_from_dir(&mut dom, parent_ref, &fs_full, &project_root)?;
        } else if fs_full.is_file() {
            populate_file(&mut dom, parent_ref, &fs_full)?;
        }
    }

    let instance_count = count_instances(&dom, data_model_ref);
    output::kv("Instances", &instance_count.to_string());

    let output_path = if output.is_absolute() {
        output.to_path_buf()
    } else {
        project_root.join(output)
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
// Syncback command
// ---------------------------------------------------------------------------

fn command_syncback(root: &Path, input: &Path, project: &Path, dry_run: bool) -> Result<()> {
    use rbx_dom_weak::WeakDom;

    let input_path = if input.is_absolute() {
        input.to_path_buf()
    } else {
        root.join(input)
    };
    let project_context = resolve_project_context(root, project, &[])?;
    let project_path = project_context.project_path;
    let project_root = project_context.project_root;

    if !input_path.exists() {
        bail!("input file does not exist: {}", input_path.display());
    }

    let tree = project_context.tree;

    output::header("Syncback");
    output::kv("Input", &input_path.display().to_string());
    output::kv("Project", &project_path.display().to_string());
    if dry_run {
        output::kv("Mode", "dry-run");
    }

    // Parse place file.
    let file = std::fs::File::open(&input_path)
        .with_context(|| format!("failed to open {}", input_path.display()))?;
    let reader = std::io::BufReader::new(file);

    let ext = input_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("rbxl");

    let dom: WeakDom = match ext {
        "rbxlx" => {
            rbx_xml::from_reader_default(reader).context("failed to parse .rbxlx place file")?
        }
        _ => rbx_binary::from_reader(reader).context("failed to parse .rbxl place file")?,
    };

    // Build reverse mapping: DataModel instance path -> filesystem path.
    let mut instance_to_fs: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for mapping in &tree.mappings {
        instance_to_fs.insert(mapping.instance_path.clone(), mapping.fs_path.clone());
    }

    // Walk the DOM and extract scripts.
    let mut written = 0usize;
    let mut skipped = 0usize;
    let mut ctx = SyncbackContext {
        instance_to_fs: &instance_to_fs,
        root: &project_root,
        dry_run,
        written: &mut written,
        skipped: &mut skipped,
    };

    syncback_walk(&dom, dom.root_ref(), "", &mut ctx)?;

    eprintln!();
    if dry_run {
        output::success(&format!(
            "Dry run complete: {written} files would be written, {skipped} skipped"
        ));
    } else {
        output::success(&format!(
            "Syncback complete: {written} files written, {skipped} skipped"
        ));
    }

    Ok(())
}

/// Recursively walk DOM instances and extract script sources to the filesystem.
struct SyncbackContext<'a> {
    instance_to_fs: &'a std::collections::HashMap<String, String>,
    root: &'a Path,
    dry_run: bool,
    written: &'a mut usize,
    skipped: &'a mut usize,
}

fn syncback_walk(
    dom: &rbx_dom_weak::WeakDom,
    inst_ref: rbx_dom_weak::types::Ref,
    parent_dm_path: &str,
    ctx: &mut SyncbackContext<'_>,
) -> Result<()> {
    let Some(inst) = dom.get_by_ref(inst_ref) else {
        return Ok(());
    };

    let dm_path = if parent_dm_path.is_empty() {
        inst.name.clone()
    } else if inst.name.is_empty() {
        parent_dm_path.to_string()
    } else {
        format!("{parent_dm_path}.{}", inst.name)
    };

    // Check if this instance is a script with source.
    let is_script = matches!(
        inst.class.as_str(),
        "Script" | "LocalScript" | "ModuleScript"
    );

    if is_script {
        let source_key: rbx_dom_weak::Ustr = "Source".into();
        if let Some(rbx_dom_weak::types::Variant::String(source)) = inst.properties.get(&source_key)
        {
            // Try to find the best filesystem mapping for this instance.
            if let Some(fs_path) = resolve_syncback_path(&dm_path, ctx.instance_to_fs, inst, dom) {
                let full_path = ctx.root.join(&fs_path);
                if ctx.dry_run {
                    output::kv("  [dry-run]", &fs_path);
                } else {
                    if let Some(parent) = full_path.parent() {
                        std::fs::create_dir_all(parent).with_context(|| {
                            format!("failed to create directory {}", parent.display())
                        })?;
                    }
                    std::fs::write(&full_path, source)
                        .with_context(|| format!("failed to write {}", full_path.display()))?;
                    output::kv("  wrote", &fs_path);
                }
                *ctx.written += 1;
            } else {
                *ctx.skipped += 1;
            }
        }
    }

    // Recurse into children.
    for &child_ref in inst.children() {
        syncback_walk(dom, child_ref, &dm_path, ctx)?;
    }

    Ok(())
}

/// Resolve the filesystem path for a script instance during syncback.
///
/// Finds the longest matching prefix in the instance-to-filesystem mapping,
/// then appends the remaining path segments with the appropriate file extension.
fn resolve_syncback_path(
    dm_path: &str,
    instance_to_fs: &std::collections::HashMap<String, String>,
    inst: &rbx_dom_weak::Instance,
    _dom: &rbx_dom_weak::WeakDom,
) -> Option<String> {
    let normalized_dm_path = dm_path.strip_prefix("DataModel.").unwrap_or(dm_path);

    // Find the longest matching prefix.
    let mut best_prefix = "";
    let mut best_fs = "";
    for (inst_path, fs_path) in instance_to_fs {
        if normalized_dm_path.starts_with(inst_path.as_str())
            && inst_path.len() > best_prefix.len()
            && (normalized_dm_path.len() == inst_path.len()
                || normalized_dm_path.as_bytes().get(inst_path.len()) == Some(&b'.'))
        {
            best_prefix = inst_path;
            best_fs = fs_path;
        }
    }

    if best_fs.is_empty() {
        return None;
    }

    let suffix = if normalized_dm_path.len() > best_prefix.len() {
        &normalized_dm_path[best_prefix.len() + 1..] // skip the '.'
    } else {
        ""
    };

    let ext = match inst.class.as_str() {
        "Script" => ".server.luau",
        "LocalScript" => ".client.luau",
        _ => ".luau",
    };

    // Check if this script is the "init" script for its parent (direct child of a mapped node).
    let is_init = if suffix.is_empty() {
        true
    } else if !suffix.contains('.') {
        // Single segment — check if this is a directory with children.
        !inst.children().is_empty()
    } else {
        false
    };

    if is_init && suffix.is_empty() {
        // Root-level init script for the mapped directory.
        let init_name = match inst.class.as_str() {
            "Script" => "init.server.luau",
            "LocalScript" => "init.client.luau",
            _ => "init.luau",
        };
        Some(format!("{best_fs}/{init_name}"))
    } else if is_init {
        // Init script for a subdirectory.
        let dir_path = suffix.replace('.', "/");
        let init_name = match inst.class.as_str() {
            "Script" => "init.server.luau",
            "LocalScript" => "init.client.luau",
            _ => "init.luau",
        };
        Some(format!("{best_fs}/{dir_path}/{init_name}"))
    } else {
        // Regular script file.
        let segments: Vec<&str> = suffix.split('.').collect();
        if segments.len() == 1 {
            Some(format!("{best_fs}/{}{ext}", segments[0]))
        } else {
            let dir_part = segments[..segments.len() - 1].join("/");
            let file_name = segments[segments.len() - 1];
            Some(format!("{best_fs}/{dir_part}/{file_name}{ext}"))
        }
    }
}

// ---------------------------------------------------------------------------
// Init command
// ---------------------------------------------------------------------------

async fn command_sourcemap(
    root: &Path,
    output: &Path,
    project: &Path,
    include_non_scripts: bool,
    watch: bool,
    cli: &Cli,
) -> Result<()> {
    use vertigo_sync::sourcemap::generate_sourcemap;

    let project_context = resolve_project_context(root, project, &cli.include)?;
    let project_path = project_context.project_path;
    let project_root = project_context.project_root;
    let includes = project_context.includes;
    let tree = project_context.tree;

    let output_path = if output.is_absolute() {
        output.to_path_buf()
    } else {
        project_root.join(output)
    };

    let write_sourcemap = |tree: &vertigo_sync::project::ProjectTree| -> Result<()> {
        let sourcemap = generate_sourcemap(&project_root, tree, include_non_scripts)?;
        let json =
            serde_json::to_string_pretty(&sourcemap).context("failed to serialize sourcemap")?;
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&output_path, json.as_bytes())
            .with_context(|| format!("failed to write sourcemap to {}", output_path.display()))?;
        Ok(())
    };

    write_sourcemap(&tree)?;

    if cli.json {
        let sourcemap = generate_sourcemap(&project_root, &tree, include_non_scripts)?;
        println!("{}", serde_json::to_string(&sourcemap)?);
    } else {
        output::success("Sourcemap generated");
        output::kv("Output", &output_path.display().to_string());
        output::kv("Project", &project_path.display().to_string());
    }

    if watch {
        if !cli.json {
            output::info("Watching for changes (Ctrl+C to stop)...");
        }

        use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
        use std::sync::mpsc;

        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
        let mut watcher = RecommendedWatcher::new(tx, Config::default())
            .context("failed to create filesystem watcher")?;

        for inc in &includes {
            let watch_path = project_root.join(inc);
            if watch_path.exists() {
                watcher
                    .watch(&watch_path, RecursiveMode::Recursive)
                    .with_context(|| format!("failed to watch {}", watch_path.display()))?;
            }
        }

        watcher
            .watch(&project_path, RecursiveMode::NonRecursive)
            .with_context(|| format!("failed to watch project file {}", project_path.display()))?;

        let coalesce_window = Duration::from_millis(100);
        loop {
            match rx.recv() {
                Ok(Ok(_event)) => {}
                Ok(Err(e)) => {
                    output::warn(&format!("watch error: {e}"));
                    continue;
                }
                Err(_) => break,
            }

            let deadline = std::time::Instant::now() + coalesce_window;
            while std::time::Instant::now() < deadline {
                match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                {
                    Ok(_) => {}
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }

            let new_tree = match parse_project(&project_path) {
                Ok(t) => t,
                Err(e) => {
                    output::warn(&format!("project parse error: {e}"));
                    continue;
                }
            };

            match write_sourcemap(&new_tree) {
                Ok(()) => {
                    if !cli.json {
                        output::success("Sourcemap regenerated");
                    }
                }
                Err(e) => {
                    output::warn(&format!("sourcemap generation error: {e}"));
                }
            }
        }
    }

    Ok(())
}

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

    vertigo_sync::init::run_init(root, name)?;

    eprintln!();
    output::success(&format!("Project '{project_name}' initialized"));
    eprintln!();
    output::info("Next steps:");
    output::info("  vsync install                   Install dependencies");
    output::info("  vsync validate                  Check project health");
    output::info("  vsync fmt                       Format source files");
    output::info("  vsync serve --turbo             Start syncing");
    output::info("  vsync plugin-install            Install Studio plugin");

    Ok(())
}

// ---------------------------------------------------------------------------
// Migrate command
// ---------------------------------------------------------------------------

fn command_migrate(root: &Path) -> Result<()> {
    output::header("Migrating to vsync.toml");
    eprintln!();

    let report = vertigo_sync::migrate::run_migrate(root)?;

    if !report.wally_migrated && !report.selene_migrated && !report.stylua_migrated {
        if root.join("vsync.toml").exists() {
            output::info("vsync.toml already exists — skipping migration");
        } else {
            output::info("No ecosystem configs found; wrote vsync.toml with defaults");
        }
        return Ok(());
    }

    if report.wally_migrated {
        let mut parts = Vec::new();
        if report.dep_count > 0 {
            parts.push(format!(
                "{} {}",
                report.dep_count,
                if report.dep_count == 1 { "dependency" } else { "dependencies" }
            ));
        }
        if report.server_dep_count > 0 {
            parts.push(format!(
                "{} server {}",
                report.server_dep_count,
                if report.server_dep_count == 1 { "dependency" } else { "dependencies" }
            ));
        }
        if report.dev_dep_count > 0 {
            parts.push(format!(
                "{} dev {}",
                report.dev_dep_count,
                if report.dev_dep_count == 1 { "dependency" } else { "dependencies" }
            ));
        }
        let suffix = if parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", parts.join(", "))
        };
        output::success(&format!(
            "Migrated wally.toml \u{2192} vsync.toml{suffix}"
        ));
    }
    if report.selene_migrated {
        output::success("Migrated selene.toml \u{2192} vsync.toml [lint]");
    }
    if report.stylua_migrated {
        output::success("Migrated stylua.toml \u{2192} vsync.toml [format]");
    }
    if report.aftman_found {
        eprintln!();
        output::warn("aftman.toml / foreman.toml detected — tool versions are not migrated automatically");
    }

    eprintln!();
    output::success("vsync.toml created");
    output::info("Review the generated config and remove old ecosystem files when ready.");

    Ok(())
}

// ---------------------------------------------------------------------------
// Plugin install command
// ---------------------------------------------------------------------------

/// Embedded Studio plugin source, compiled into the binary.
const PLUGIN_SOURCE: &str = include_str!("../assets/VertigoSyncPlugin.lua");
const TOOLBAR_ICON_LINE_PREFIX: &str = "DEFAULT_TOOLBAR_ICON_ASSET = ";

fn command_plugin_install() -> Result<()> {
    let plugin_safety =
        validate::validate_plugin_source_text("embedded://VertigoSyncPlugin.lua", PLUGIN_SOURCE)?;
    output::step(1, 3, "Validating generated plugin safety");
    for warning in &plugin_safety.warnings {
        output::warn(&format!("[{}]: {}", warning.rule, warning.message));
    }
    if !plugin_safety.errors.is_empty() {
        for error in &plugin_safety.errors {
            output::error_msg(&format!("[{}]: {}", error.rule, error.message));
        }
        bail!("refusing to install an unsafe generated plugin");
    }
    output::success(&format!(
        "Plugin safety passed (top-level symbols: {} / {})",
        plugin_safety.top_level_symbol_count, plugin_safety.top_level_symbol_budget
    ));

    let plugins_dir = detect_plugins_dir()?;

    output::step(2, 3, "Detecting Roblox Plugins directory");
    output::success(&format!("Found: {}", plugins_dir.display()));

    std::fs::create_dir_all(&plugins_dir)
        .with_context(|| format!("failed to create {}", plugins_dir.display()))?;

    let dest = plugins_dir.join("VertigoSyncPlugin.lua");
    output::step(3, 3, "Installing VertigoSyncPlugin.lua");
    std::fs::write(&dest, PLUGIN_SOURCE)
        .with_context(|| format!("failed to write {}", dest.display()))?;

    output::success(&format!("Installed to {}", dest.display()));
    eprintln!();
    output::info("Restart Roblox Studio to load the plugin.");

    Ok(())
}

fn command_plugin_set_icon(asset_id: &str) -> Result<()> {
    let plugins_dir = detect_plugins_dir()?;
    let dest = plugins_dir.join("VertigoSyncPlugin.lua");

    if !dest.exists() {
        bail!(
            "installed plugin not found at {}; run `vsync plugin-install` first",
            dest.display()
        );
    }

    let normalized = normalize_toolbar_icon_asset(asset_id)?;
    let existing = std::fs::read_to_string(&dest)
        .with_context(|| format!("failed to read {}", dest.display()))?;

    let replacement = format!(r#"{}"{}","#, TOOLBAR_ICON_LINE_PREFIX, normalized);
    let mut updated_lines = Vec::new();
    let mut replaced = false;
    for line in existing.lines() {
        let trimmed = line.trim_start();
        if !replaced && trimmed.starts_with(TOOLBAR_ICON_LINE_PREFIX) {
            let indentation_len = line.len() - trimmed.len();
            let indentation = &line[..indentation_len];
            updated_lines.push(format!("{indentation}{replacement}"));
            replaced = true;
        } else {
            updated_lines.push(line.to_string());
        }
    }

    if !replaced {
        bail!(
            "installed plugin does not contain the expected toolbar icon line; reinstall the plugin first"
        );
    }

    let updated = format!("{}\n", updated_lines.join("\n"));

    std::fs::write(&dest, updated)
        .with_context(|| format!("failed to write {}", dest.display()))?;

    output::success(&format!(
        "Updated installed plugin toolbar icon to {normalized}"
    ));
    output::info("Restart Roblox Studio to reload the plugin.");

    Ok(())
}

fn normalize_toolbar_icon_asset(asset_id: &str) -> Result<String> {
    let trimmed = asset_id.trim();
    if trimmed.is_empty() {
        bail!("asset ID must not be empty");
    }

    if let Some(raw) = trimmed.strip_prefix("rbxassetid://") {
        if raw.chars().all(|ch| ch.is_ascii_digit()) && !raw.is_empty() {
            return Ok(trimmed.to_string());
        }
        bail!("asset ID must be numeric after `rbxassetid://`");
    }

    if trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        return Ok(format!("rbxassetid://{trimmed}"));
    }

    bail!("asset ID must be numeric or `rbxassetid://<numeric>`");
}

#[cfg(test)]
mod plugin_icon_tests {
    use super::normalize_toolbar_icon_asset;

    #[test]
    fn normalize_toolbar_icon_asset_accepts_plain_numeric() {
        let asset = normalize_toolbar_icon_asset("71461188969386").unwrap();
        assert_eq!(asset, "rbxassetid://71461188969386");
    }

    #[test]
    fn normalize_toolbar_icon_asset_accepts_prefixed_numeric() {
        let asset = normalize_toolbar_icon_asset("rbxassetid://71461188969386").unwrap();
        assert_eq!(asset, "rbxassetid://71461188969386");
    }

    #[test]
    fn normalize_toolbar_icon_asset_rejects_invalid() {
        assert!(normalize_toolbar_icon_asset("rbxassetid://abc").is_err());
        assert!(normalize_toolbar_icon_asset("abc").is_err());
        assert!(normalize_toolbar_icon_asset("").is_err());
    }
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
                        builder = builder
                            .with_property("Source", rbx_dom_weak::types::Variant::String(source));
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
                "json" | "yaml" | "yml" | "toml" => {
                    let instance_name = name_str
                        .strip_suffix(".json")
                        .or_else(|| name_str.strip_suffix(".yaml"))
                        .or_else(|| name_str.strip_suffix(".yml"))
                        .or_else(|| name_str.strip_suffix(".toml"))
                        .unwrap_or(&name_str);
                    let mut builder =
                        rbx_dom_weak::InstanceBuilder::new("ModuleScript").with_name(instance_name);
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        builder = builder
                            .with_property("Source", rbx_dom_weak::types::Variant::String(content));
                    }
                    dom.insert(parent_ref, builder);
                }
                "jsonc" => {
                    let instance_name = name_str.strip_suffix(".jsonc").unwrap_or(&name_str);
                    let mut builder =
                        rbx_dom_weak::InstanceBuilder::new("ModuleScript").with_name(instance_name);
                    if let Ok(raw) = std::fs::read_to_string(&path) {
                        let clean = vertigo_sync::strip_json_comments(&raw);
                        builder = builder
                            .with_property("Source", rbx_dom_weak::types::Variant::String(clean));
                    }
                    dom.insert(parent_ref, builder);
                }
                "txt" => {
                    let instance_name = name_str.strip_suffix(".txt").unwrap_or(&name_str);
                    let mut builder =
                        rbx_dom_weak::InstanceBuilder::new("StringValue").with_name(instance_name);
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        builder = builder
                            .with_property("Value", rbx_dom_weak::types::Variant::String(content));
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
                builder = builder.with_property(
                    key.as_str(),
                    rbx_dom_weak::types::Variant::String(s.clone()),
                );
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
    use super::{
        Cli, Command, PLUGIN_SOURCE, ProjectContext, SyncbackContext, discover_server_url,
        fetch_json_http, parse_http_url, populate_from_dir, resolve_container_class,
        resolve_effective_includes, resolve_instance_name, resolve_project_context,
        resolve_syncback_path, syncback_walk,
    };
    use axum::{Json, Router, routing::get};
    use clap::Parser;
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};
    use vertigo_sync::validate;

    fn write_project(path: &Path, name: &str, source_root: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create project parent");
        }
        fs::write(
            path,
            serde_json::to_vec_pretty(&json!({
                "name": name,
                "tree": {
                    "$className": "DataModel",
                    "ServerScriptService": {
                        "Server": {
                            "$path": source_root
                        }
                    }
                }
            }))
            .expect("serialize project"),
        )
        .expect("write project");
    }

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

    #[test]
    fn populate_from_dir_handles_yaml_and_toml() {
        use rbx_dom_weak::{InstanceBuilder, WeakDom};

        let temp = tempfile::tempdir().expect("tempdir");
        let src = temp.path().join("src");
        fs::create_dir_all(&src).expect("create src dir");
        fs::write(src.join("config.yaml"), "key: value").expect("write yaml");
        fs::write(src.join("data.yml"), "items:\n  - a").expect("write yml");
        fs::write(src.join("settings.toml"), "[section]\nkey = true").expect("write toml");

        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root_ref = dom.root_ref();
        let parent_ref = dom.insert(root_ref, InstanceBuilder::new("Folder").with_name("Test"));

        populate_from_dir(&mut dom, parent_ref, &src, temp.path()).expect("populate");

        let parent = dom.get_by_ref(parent_ref).unwrap();
        let child_names: Vec<String> = parent
            .children()
            .iter()
            .filter_map(|&r| dom.get_by_ref(r).map(|i| i.name.clone()))
            .collect();

        assert!(
            child_names.contains(&"config".to_string()),
            "yaml file not found: {child_names:?}"
        );
        assert!(
            child_names.contains(&"data".to_string()),
            "yml file not found: {child_names:?}"
        );
        assert!(
            child_names.contains(&"settings".to_string()),
            "toml file not found: {child_names:?}"
        );

        // Verify they are all ModuleScripts with Source property.
        let source_key: rbx_dom_weak::Ustr = "Source".into();
        for &child_ref in parent.children() {
            let child = dom.get_by_ref(child_ref).unwrap();
            assert_eq!(
                child.class, "ModuleScript",
                "wrong class for {}",
                child.name
            );
            assert!(
                child.properties.contains_key(&source_key),
                "missing Source for {}",
                child.name
            );
        }
    }

    #[test]
    fn populate_from_dir_handles_jsonc() {
        use rbx_dom_weak::{InstanceBuilder, WeakDom};

        let temp = tempfile::tempdir().expect("tempdir");
        let src = temp.path().join("src");
        fs::create_dir_all(&src).expect("create src dir");
        fs::write(
            src.join("config.jsonc"),
            "{\n  // This is a comment\n  \"key\": \"value\"\n}",
        )
        .expect("write jsonc");

        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root_ref = dom.root_ref();
        let parent_ref = dom.insert(root_ref, InstanceBuilder::new("Folder").with_name("Test"));

        populate_from_dir(&mut dom, parent_ref, &src, temp.path()).expect("populate");

        let parent = dom.get_by_ref(parent_ref).unwrap();
        let child_names: Vec<String> = parent
            .children()
            .iter()
            .filter_map(|&r| dom.get_by_ref(r).map(|i| i.name.clone()))
            .collect();

        assert!(
            child_names.contains(&"config".to_string()),
            "jsonc file not found: {child_names:?}"
        );

        // Verify it's a ModuleScript with stripped comments in Source.
        let source_key: rbx_dom_weak::Ustr = "Source".into();
        for &child_ref in parent.children() {
            let child = dom.get_by_ref(child_ref).unwrap();
            assert_eq!(child.class, "ModuleScript");
            let source = child.properties.get(&source_key).expect("missing Source");
            if let rbx_dom_weak::types::Variant::String(s) = source {
                assert!(
                    !s.contains("// This is a comment"),
                    "JSONC comment should be stripped"
                );
                assert!(
                    s.contains("\"key\": \"value\""),
                    "JSON content should remain"
                );
            } else {
                panic!("Source should be a String variant");
            }
        }
    }

    #[test]
    fn syncback_resolve_path_basic() {
        let mut instance_to_fs = std::collections::HashMap::new();
        instance_to_fs.insert(
            "ServerScriptService.Server".to_string(),
            "src/Server".to_string(),
        );

        use rbx_dom_weak::{InstanceBuilder, WeakDom};
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root_ref = dom.root_ref();
        let script_ref = dom.insert(
            root_ref,
            InstanceBuilder::new("ModuleScript").with_name("DataService"),
        );

        let inst = dom.get_by_ref(script_ref).unwrap();
        let result = resolve_syncback_path(
            "ServerScriptService.Server.Services.DataService",
            &instance_to_fs,
            inst,
            &dom,
        );
        assert_eq!(
            result,
            Some("src/Server/Services/DataService.luau".to_string())
        );
    }

    #[test]
    fn syncback_resolve_path_ignores_datamodel_prefix() {
        let mut instance_to_fs = std::collections::HashMap::new();
        instance_to_fs.insert(
            "ServerScriptService.Server".to_string(),
            "src/Server".to_string(),
        );

        use rbx_dom_weak::{InstanceBuilder, WeakDom};
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root_ref = dom.root_ref();
        let script_ref = dom.insert(
            root_ref,
            InstanceBuilder::new("ModuleScript").with_name("DataService"),
        );

        let inst = dom.get_by_ref(script_ref).unwrap();
        let result = resolve_syncback_path(
            "DataModel.ServerScriptService.Server.Services.DataService",
            &instance_to_fs,
            inst,
            &dom,
        );
        assert_eq!(
            result,
            Some("src/Server/Services/DataService.luau".to_string())
        );
    }

    #[test]
    fn resolve_project_context_uses_selected_nested_project() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        write_project(
            &root.join("default.project.json"),
            "RootGame",
            "root-src/Server",
        );

        let nested_dir = root.join("apps/game");
        write_project(
            &nested_dir.join("default.project.json"),
            "NestedGame",
            "nested-src/Server",
        );

        let context =
            resolve_project_context(root, Path::new("apps/game/default.project.json"), &[])
                .expect("resolve project context");

        assert_eq!(
            context.project_path,
            nested_dir.join("default.project.json")
        );
        assert_eq!(context.project_root, nested_dir);
        assert_eq!(context.tree.name, "NestedGame");
        assert_eq!(context.includes, vec!["nested-src".to_string()]);
    }

    #[test]
    fn resolve_project_context_auto_discovers_unique_nested_project() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let nested_dir = root.join("roblox");
        write_project(
            &nested_dir.join("default.project.json"),
            "NestedGame",
            "src/Server",
        );

        let context = resolve_project_context(root, Path::new("default.project.json"), &[])
            .expect("resolve discovered project context");

        assert_eq!(
            context.project_path,
            nested_dir.join("default.project.json")
        );
        assert_eq!(context.project_root, nested_dir);
        assert_eq!(context.tree.name, "NestedGame");
    }

    #[test]
    fn resolve_project_context_rejects_ambiguous_nested_projects() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        write_project(
            &root.join("roblox/default.project.json"),
            "GameA",
            "src/Server",
        );
        write_project(
            &root.join("packages/test/default.project.json"),
            "GameB",
            "src/Server",
        );

        let err = resolve_project_context(root, Path::new("default.project.json"), &[])
            .expect_err("expected ambiguous project discovery to fail");

        let rendered = format!("{err:#}");
        assert!(rendered.contains("multiple nested project files"));
        assert!(rendered.contains("roblox/default.project.json"));
        assert!(rendered.contains("packages/test/default.project.json"));
    }

    #[test]
    fn resolve_effective_includes_does_not_auto_discover_descendants() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_project(
            &temp.path().join("apps/game/default.project.json"),
            "NestedGame",
            "nested-src/Server",
        );

        let includes = resolve_effective_includes(&temp.path().join("default.project.json"), &[]);

        assert_eq!(includes, vec!["src".to_string()]);
    }

    #[test]
    fn watch_commands_accept_project_flag() {
        let cli = Cli::try_parse_from([
            "vsync",
            "watch",
            "--project",
            "apps/game/default.project.json",
        ])
        .expect("parse watch args");
        match cli.command {
            Command::Watch { project } => {
                assert_eq!(project, PathBuf::from("apps/game/default.project.json"));
            }
            other => panic!("expected watch command, got {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "vsync",
            "watch-native",
            "--project",
            "apps/game/default.project.json",
        ])
        .expect("parse watch-native args");
        match cli.command {
            Command::WatchNative { project } => {
                assert_eq!(project, PathBuf::from("apps/game/default.project.json"));
            }
            other => panic!("expected watch-native command, got {other:?}"),
        }
    }

    #[test]
    fn parse_http_url_accepts_localhost_and_path_prefix() {
        let target = parse_http_url("http://127.0.0.1:7575/api").expect("parse url");
        assert_eq!(target.host, "127.0.0.1");
        assert_eq!(target.port, 7575);
        assert_eq!(target.path_prefix, "/api");
    }

    #[test]
    fn parse_http_url_rejects_https() {
        assert!(parse_http_url("https://127.0.0.1:7575").is_err());
    }

    #[tokio::test]
    async fn fetch_json_http_reads_discover_payload() {
        let app = Router::new().route(
            "/discover",
            get(|| async {
                Json(json!({
                    "server_id": "server-123",
                    "project_name": "Game",
                    "project_id": "proj-123"
                }))
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("http://{}:{}", addr.ip(), addr.port());
        let value = fetch_json_http(&url, "/discover")
            .await
            .expect("fetch discover json");
        server.abort();

        assert_eq!(value["server_id"], "server-123");
        assert_eq!(value["project_name"], "Game");
        assert_eq!(value["project_id"], "proj-123");
    }

    #[test]
    fn discover_server_url_prefers_project_serve_settings() {
        let context = ProjectContext {
            project_path: PathBuf::from("/tmp/default.project.json"),
            project_root: PathBuf::from("/tmp"),
            includes: vec!["src".to_string()],
            tree: vertigo_sync::project::ProjectTree {
                name: "Game".to_string(),
                project_id: "proj-123".to_string(),
                mappings: vec![],
                glob_ignore_paths: vec![],
                emit_legacy_scripts: true,
                serve_port: Some(34872),
                serve_address: Some("127.0.0.1".to_string()),
                vertigo_sync: None,
            },
            vsync_config: None,
        };

        assert_eq!(
            discover_server_url(&context, None),
            "http://127.0.0.1:34872"
        );
        assert_eq!(
            discover_server_url(&context, Some("http://127.0.0.1:9999")),
            "http://127.0.0.1:9999"
        );
    }

    #[test]
    fn embedded_plugin_keeps_builders_opt_in_by_default() {
        assert!(
            PLUGIN_SOURCE.contains("BUILDERS_ENABLED_DEFAULT = false"),
            "embedded plugin should keep builders disabled by default"
        );
    }

    #[test]
    fn embedded_plugin_hardens_oversized_source_and_queue_overflow() {
        assert!(
            PLUGIN_SOURCE.contains("MAX_LUA_SOURCE_LENGTH = 199999"),
            "embedded plugin should enforce a Lua source size guard"
        );
        assert!(
            PLUGIN_SOURCE.contains("local SCRIPT_CLASSES = table.freeze({"),
            "embedded plugin should define script-class membership for Lua source guards"
        );
        assert!(
            PLUGIN_SOURCE.contains("applyQueueLimit = 0"),
            "embedded plugin should default the apply queue limit to unlimited"
        );
        assert!(
            PLUGIN_SOURCE.contains("VertigoSyncApplyQueueLimit"),
            "embedded plugin should expose a configurable apply queue limit setting"
        );
        assert!(
            PLUGIN_SOURCE.contains("OVERSIZE_SOURCE:"),
            "embedded plugin should mark oversized source writes as hard rejections"
        );
        assert!(
            PLUGIN_SOURCE.contains("resetTransientSyncState()"),
            "embedded plugin should hard-reset transient sync state on overflow"
        );
        assert!(
            PLUGIN_SOURCE.contains("bootstrapManagedIndex()"),
            "embedded plugin should rebuild managed state before full snapshot reconciliation"
        );
        assert!(
            PLUGIN_SOURCE.contains("hardRejectedShaByPath[entryPath]"),
            "embedded plugin should remember hard-rejected snapshot entries by sha"
        );
        assert!(
            PLUGIN_SOURCE.contains("stageDelete(path)"),
            "embedded plugin should stage managed-path deletions during snapshot reconciliation"
        );
        assert!(
            PLUGIN_SOURCE.contains("Write apply permanently failed for %s after %d retries: %s"),
            "embedded plugin should keep permanent write failures observable"
        );
        assert!(
            PLUGIN_SOURCE.contains("resyncRequested = true"),
            "embedded plugin should force a full resync after hard sync divergence"
        );
        assert!(
            PLUGIN_SOURCE.contains("beginFullResync()\n\tbootstrapManagedIndex()"),
            "embedded plugin should rebuild managed state before staging rewind operations"
        );
        assert!(
            PLUGIN_SOURCE.contains("\"Time Travel\""),
            "embedded plugin should expose a compact time-travel title"
        );
        assert!(
            PLUGIN_SOURCE.contains("HistoryHeader")
                && PLUGIN_SOURCE.contains("\"time\"")
                && PLUGIN_SOURCE.contains("\"add\"")
                && PLUGIN_SOURCE.contains("\"mod\"")
                && PLUGIN_SOURCE.contains("\"del\""),
            "embedded plugin should render a compact diff history header"
        );
        assert!(
            PLUGIN_SOURCE.contains("ttLiveDot")
                && PLUGIN_SOURCE.contains("JumpOldest\", \"|<\"")
                && PLUGIN_SOURCE.contains("StepBack\", \"<\"")
                && PLUGIN_SOURCE.contains("StepFwd\", \">\"")
                && PLUGIN_SOURCE.contains("JumpLatest\", \">|\""),
            "embedded plugin should expose compact transport controls for time travel"
        );
        assert!(
            PLUGIN_SOURCE.contains("VertigoSyncBuilderQueueDepth")
                && PLUGIN_SOURCE.contains("VertigoSyncBuilderPumpActive")
                && PLUGIN_SOURCE.contains("VertigoSyncBuilderLastMs")
                && PLUGIN_SOURCE.contains("VertigoSyncBuilderAvgMs")
                && PLUGIN_SOURCE.contains("VertigoSyncBuilderMaxMs"),
            "embedded plugin should expose builder scheduler performance attributes"
        );
        assert!(
            PLUGIN_SOURCE.contains("Builder slice over budget:")
                && PLUGIN_SOURCE.contains("recordBuilderPerf(path, result, builderElapsed * 1000)"),
            "embedded plugin should record and surface slow builder slices"
        );
        assert!(
            PLUGIN_SOURCE.contains("result.BackgroundBuild == true")
                && PLUGIN_SOURCE.contains("Builder scheduled: %s"),
            "embedded plugin should support non-blocking background builders"
        );
        assert!(
            PLUGIN_SOURCE.contains("VertigoSyncTimeTravelHardPause"),
            "embedded plugin should expose a hard-pause time-travel attribute for historical preview isolation"
        );
        assert!(
            PLUGIN_SOURCE.contains("Keep fetch/apply running even during historical mode"),
            "embedded plugin should continue fetch/apply during historical mode so rewound snapshots fully materialize"
        );
        assert!(
            PLUGIN_SOURCE.contains("VertigoPreviewSyncState")
                && PLUGIN_SOURCE.contains("previewStatusSummary()")
                && PLUGIN_SOURCE.contains("builderStatusSummary()"),
            "embedded plugin should surface preview and builder telemetry in the panel"
        );
    }

    #[test]
    fn embedded_plugin_contains_edit_preview_runtime_contract() {
        assert!(
            PLUGIN_SOURCE.contains("editPreview"),
            "embedded plugin should understand vertigoSync.editPreview config"
        );
        assert!(
            PLUGIN_SOURCE.contains("builderMethod"),
            "embedded plugin should support configurable preview builder entry methods"
        );
        assert!(
            PLUGIN_SOURCE.contains("VertigoPreviewLastBuildError"),
            "embedded plugin should expose preview build failure state"
        );
    }

    #[test]
    fn embedded_plugin_passes_plugin_safety_validation() {
        let report = validate::validate_plugin_source_text(
            "embedded://VertigoSyncPlugin.lua",
            PLUGIN_SOURCE,
        )
        .expect("plugin safety validation should run");
        assert_eq!(report.path, "embedded://VertigoSyncPlugin.lua");
        assert!(
            report.compile_ok,
            "embedded plugin should compile with luau-compile"
        );
        assert!(
            report.analyze_ok,
            "embedded plugin should pass luau-analyze"
        );
        assert!(
            report.errors.is_empty(),
            "embedded plugin should not have plugin safety errors: {:?}",
            report.errors
        );
    }

    #[test]
    fn embedded_plugin_does_not_use_removed_gotham_italic_enum() {
        assert!(
            !PLUGIN_SOURCE.contains("Enum.Font.GothamItalic"),
            "embedded plugin must not use removed Enum.Font.GothamItalic"
        );
        assert!(
            PLUGIN_SOURCE.contains("Font.new(\"rbxasset://fonts/families/Montserrat.json\", Enum.FontWeight.Regular, Enum.FontStyle.Italic)"),
            "embedded plugin should use a modern FontFace italic fallback"
        );
    }

    #[test]
    fn embedded_plugin_stays_under_roblox_top_level_symbol_budget() {
        let report = validate::validate_plugin_source_text(
            "embedded://VertigoSyncPlugin.lua",
            PLUGIN_SOURCE,
        )
        .expect("plugin safety validation should run");
        assert!(
            PLUGIN_SOURCE.contains("local Runtime = {}"),
            "embedded plugin should namespace runtime helpers to keep the top-level scope small"
        );
        assert!(
            PLUGIN_SOURCE.contains(
                "pcall(Runtime.applyWrite, path, ready.source, ready.sha256 or op.expectedSha)"
            ),
            "embedded plugin should keep apply queue writes namespaced after the Runtime refactor"
        );
        assert!(
            PLUGIN_SOURCE.contains("if UI.historyListFrame ~= nil then"),
            "embedded plugin should guard optional history list UI wiring"
        );
        assert!(
            PLUGIN_SOURCE.contains("task.defer(Runtime.processFetchQueue)"),
            "embedded plugin should defer the namespaced fetch queue pump after 413 fallback"
        );
        assert!(
            report.top_level_symbol_count <= report.top_level_symbol_budget,
            "embedded plugin should stay under the Roblox plugin top-level symbol budget; got {}",
            report.top_level_symbol_count
        );
    }

    #[test]
    fn syncback_walk_writes_into_selected_project_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let nested_dir = root.join("apps/game");
        let mut instance_to_fs = std::collections::HashMap::new();
        instance_to_fs.insert(
            "ServerScriptService.Server".to_string(),
            "nested-src/Server".to_string(),
        );

        use rbx_dom_weak::{InstanceBuilder, WeakDom};
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root_ref = dom.root_ref();
        let service_ref = dom.insert(
            root_ref,
            InstanceBuilder::new("ServerScriptService").with_name("ServerScriptService"),
        );
        let folder_ref = dom.insert(
            service_ref,
            InstanceBuilder::new("Folder").with_name("Server"),
        );
        dom.insert(
            folder_ref,
            InstanceBuilder::new("ModuleScript")
                .with_name("DataService")
                .with_property(
                    "Source",
                    rbx_dom_weak::types::Variant::String("return 'nested'\n".to_string()),
                ),
        );

        let mut written = 0usize;
        let mut skipped = 0usize;
        let mut ctx = SyncbackContext {
            instance_to_fs: &instance_to_fs,
            root: &nested_dir,
            dry_run: false,
            written: &mut written,
            skipped: &mut skipped,
        };
        syncback_walk(&dom, root_ref, "", &mut ctx).expect("syncback walk");

        assert_eq!(written, 1);
        assert_eq!(skipped, 0);

        let restored = fs::read_to_string(nested_dir.join("nested-src/Server/DataService.luau"))
            .expect("read restored nested script");
        assert_eq!(restored, "return 'nested'\n");
        assert!(
            !root.join("nested-src/Server/DataService.luau").exists(),
            "syncback should write under the selected project root"
        );
    }
}
