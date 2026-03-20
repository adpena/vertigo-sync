use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// Look up a named script in the scripts map, returning the command string if found.
pub fn resolve_script(name: &str, scripts: &BTreeMap<String, String>) -> Option<String> {
    scripts.get(name).cloned()
}

/// Execute a named script in a shell subprocess.
///
/// # Trust model
/// Scripts are defined in vsync.toml, which is typically committed to version
/// control. Running `vsync run <name>` trusts the project's vsync.toml the same
/// way `npm run` trusts package.json scripts. The `VSYNC_PROJECT_NAME` env var
/// comes from the project config and is not sanitized — this matches the trust
/// boundary of the project file itself.
///
/// Environment variables `VSYNC_PROJECT_ROOT` and `VSYNC_PROJECT_NAME` are injected
/// so scripts can reference the project context.
///
/// Returns the exit code of the child process.
pub fn run_script(
    name: &str,
    command: &str,
    project_root: &Path,
    project_name: &str,
) -> Result<i32> {
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    };

    cmd.current_dir(project_root)
        .env("VSYNC_PROJECT_ROOT", project_root.as_os_str())
        .env("VSYNC_PROJECT_NAME", project_name);

    let status = cmd
        .status()
        .with_context(|| format!("failed to execute script '{name}': {command}"))?;

    match status.code() {
        Some(code) => Ok(code),
        None => bail!("script '{name}' terminated by signal"),
    }
}
