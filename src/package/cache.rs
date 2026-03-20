use anyhow::{Context, Result, bail};
use std::path::PathBuf;

/// Returns the root directory for the package cache: `~/.vsync/cache/packages/`.
/// Creates the directory tree if it does not exist.
pub fn cache_root() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    let root = home.join(".vsync").join("cache").join("packages");
    if !root.exists() {
        std::fs::create_dir_all(&root)
            .with_context(|| format!("failed to create cache directory {}", root.display()))?;
    }
    Ok(root)
}

/// Returns the path where a cached package zip would be stored, keyed by checksum.
pub fn cached_package_path(checksum: &str) -> Result<PathBuf> {
    if checksum.is_empty() {
        bail!("checksum must not be empty");
    }
    if !checksum
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == ':' || c == '-')
    {
        bail!("invalid checksum format: contains unsafe characters");
    }
    let root = cache_root()?;
    Ok(root.join(format!("{checksum}.zip")))
}

/// Returns `true` if the package with the given checksum is already cached.
pub fn is_cached(checksum: &str) -> Result<bool> {
    let path = cached_package_path(checksum)?;
    Ok(path.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_root_creates_dir() {
        let root = cache_root().unwrap();
        assert!(root.exists());
        assert!(root.ends_with("packages"));
    }

    #[test]
    fn cached_package_path_format() {
        let path = cached_package_path("deadbeef").unwrap();
        assert!(path.to_string_lossy().contains("deadbeef.zip"));
    }

    #[test]
    fn cached_package_path_rejects_path_traversal() {
        let result = cached_package_path("../../etc/passwd");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unsafe characters"));
    }

    #[test]
    fn cached_package_path_allows_sha256_prefix() {
        let result = cached_package_path("sha256:abcdef0123456789");
        assert!(result.is_ok());
    }
}
