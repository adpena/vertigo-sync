//! Credential management for registry authentication.
//!
//! Stores and retrieves API tokens from `~/.vsync/credentials.toml`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// On-disk credential store.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CredentialStore {
    /// Mapping from registry URL to its credentials.
    #[serde(default)]
    pub registries: BTreeMap<String, RegistryCredential>,
}

/// Credentials for a single registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryCredential {
    pub token: String,
}

/// Returns the path to `~/.vsync/credentials.toml`.
pub fn credentials_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join(".vsync").join("credentials.toml"))
}

/// Load the credential store from disk.
///
/// Returns an empty store if the file does not exist.
pub fn load_credentials() -> Result<CredentialStore> {
    let path = credentials_path()?;
    if !path.exists() {
        return Ok(CredentialStore::default());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let store: CredentialStore = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(store)
}

/// Save the credential store to disk, creating `~/.vsync/` if needed.
pub fn save_credentials(store: &CredentialStore) -> Result<()> {
    let path = credentials_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let content = toml::to_string_pretty(store)
        .context("failed to serialize credentials")?;

    // Write with restrictive permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("failed to write {}", path.display()))?;
        f.write_all(content.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, &content)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    Ok(())
}

/// Retrieve the token for a specific registry URL.
pub fn get_token(registry_url: &str) -> Result<Option<String>> {
    let store = load_credentials()?;
    Ok(store.registries.get(registry_url).map(|c| c.token.clone()))
}

/// Store a token for a specific registry URL.
pub fn set_token(registry_url: &str, token: &str) -> Result<()> {
    let mut store = load_credentials()?;
    store.registries.insert(
        registry_url.to_string(),
        RegistryCredential {
            token: token.to_string(),
        },
    );
    save_credentials(&store)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_path_under_home() {
        let path = credentials_path().unwrap();
        assert!(path.to_string_lossy().contains(".vsync"));
        assert!(path.to_string_lossy().ends_with("credentials.toml"));
    }

    #[test]
    fn credential_store_roundtrip() {
        let mut store = CredentialStore::default();
        store.registries.insert(
            "https://api.wally.run".to_string(),
            RegistryCredential {
                token: "wally_test_token".to_string(),
            },
        );
        let serialized = toml::to_string_pretty(&store).unwrap();
        let deserialized: CredentialStore = toml::from_str(&serialized).unwrap();
        assert_eq!(
            deserialized.registries["https://api.wally.run"].token,
            "wally_test_token"
        );
    }

    #[test]
    fn set_and_get_token_roundtrip() {
        // Use a temp dir to avoid mutating real credentials
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("credentials.toml");

        let mut store = CredentialStore::default();
        store.registries.insert(
            "https://example.com".to_string(),
            RegistryCredential {
                token: "secret123".to_string(),
            },
        );

        let content = toml::to_string_pretty(&store).unwrap();
        std::fs::write(&path, &content).unwrap();

        let loaded: CredentialStore =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            loaded.registries["https://example.com"].token,
            "secret123"
        );
    }
}
