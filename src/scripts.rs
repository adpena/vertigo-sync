use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// Look up a named script in the scripts map, returning the command string if found.
pub fn resolve_script(name: &str, scripts: &BTreeMap<String, String>) -> Option<String> {
    scripts.get(name).cloned()
}

/// Probe whether a command is available on PATH by attempting to resolve it.
fn which_cmd(name: &str) -> Option<String> {
    #[cfg(target_os = "windows")]
    let lookup = "where";
    #[cfg(not(target_os = "windows"))]
    let lookup = "which";

    Command::new(lookup).arg(name).output().ok().and_then(|o| {
        if o.status.success() {
            let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !path.is_empty() { Some(path) } else { None }
        } else {
            None
        }
    })
}

/// Resolve the shell and flag to use for script execution.
///
/// On Unix: prefers `sh` from PATH, falls back to `/bin/sh`.
/// On Windows: prefers `pwsh`, then `powershell`, then `cmd`.
///
/// Returns `(shell_program, flag)` or an error if no shell is available.
fn resolve_shell() -> Result<(&'static str, &'static str)> {
    #[cfg(target_os = "windows")]
    {
        if which_cmd("pwsh").is_some() {
            Ok(("pwsh", "-Command"))
        } else if which_cmd("powershell").is_some() {
            Ok(("powershell", "-Command"))
        } else if which_cmd("cmd").is_some() {
            Ok(("cmd", "/C"))
        } else {
            bail!("no shell found: vsync requires pwsh, powershell, or cmd to be available on PATH")
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        if which_cmd("sh").is_some() || Path::new("/bin/sh").exists() {
            Ok(("sh", "-c"))
        } else {
            bail!("no shell found: vsync requires `sh` to be available on PATH or at /bin/sh")
        }
    }
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
    let (shell, flag) = resolve_shell()
        .with_context(|| format!("cannot run script '{name}': shell detection failed"))?;

    let mut cmd = Command::new(shell);
    cmd.arg(flag).arg(command);

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
