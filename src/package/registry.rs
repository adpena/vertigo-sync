use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A single version entry from the Wally package index.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IndexEntry {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub realm: String,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "server-dependencies")]
    pub server_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub description: String,
}

/// Validate that a scope or package name contains only safe characters.
/// Wally identifiers allow: a-z, A-Z, 0-9, -, _
pub fn validate_identifier(value: &str, label: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{label} cannot be empty");
    }
    if !value.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("{label} '{value}' contains invalid characters (only a-z, A-Z, 0-9, -, _ allowed)");
    }
    Ok(())
}

/// Parse a version-requirement spec like `"scope/name@^version"` into `(scope, name, version_req)`.
pub fn parse_version_req(spec: &str) -> Result<(String, String, String)> {
    let at_pos = spec
        .find('@')
        .with_context(|| format!("dependency spec '{spec}' is missing '@' separator"))?;
    let package_part = &spec[..at_pos];
    let version_req = &spec[at_pos + 1..];

    let slash_pos = package_part
        .find('/')
        .with_context(|| format!("dependency spec '{spec}' is missing scope separator '/'"))?;
    let scope = &package_part[..slash_pos];
    let name = &package_part[slash_pos + 1..];

    if scope.is_empty() || name.is_empty() || version_req.is_empty() {
        bail!("dependency spec '{spec}' has empty scope, name, or version");
    }

    validate_identifier(scope, "package scope")?;
    validate_identifier(name, "package name")?;

    Ok((scope.to_string(), name.to_string(), version_req.to_string()))
}

/// A client for the Wally package registry.
pub struct RegistryClient {
    pub api_url: String,
    client: reqwest::Client,
}

impl RegistryClient {
    /// Create a client pointed at the default Wally registry.
    pub fn default_wally() -> Result<Self> {
        Ok(Self {
            api_url: "https://api.wally.run".to_string(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .context("failed to build HTTP client")?,
        })
    }

    /// Fetch all published versions for a package.
    pub async fn fetch_versions(
        &self,
        scope: &str,
        name: &str,
    ) -> Result<Vec<IndexEntry>> {
        let url = format!(
            "{}/v1/package-versions/{}/{}",
            self.api_url, scope, name
        );
        let resp = self
            .client
            .get(&url)
            .header("Wally-Version", "0.3.2")
            .send()
            .await
            .with_context(|| format!("failed to fetch versions from {url}"))?;

        if !resp.status().is_success() {
            bail!(
                "registry returned {} for {}/{}",
                resp.status(),
                scope,
                name
            );
        }

        let entries: Vec<IndexEntry> = resp
            .json()
            .await
            .context("failed to parse registry response as JSON")?;
        Ok(entries)
    }

    /// Download the package zip archive for a specific version.
    pub async fn download_package(
        &self,
        scope: &str,
        name: &str,
        version: &str,
    ) -> Result<Vec<u8>> {
        let url = format!(
            "{}/v1/package-contents/{}/{}/{}",
            self.api_url, scope, name, version
        );
        let resp = self
            .client
            .get(&url)
            .header("Wally-Version", "0.3.2")
            .send()
            .await
            .with_context(|| format!("failed to download {scope}/{name}@{version}"))?;

        if !resp.status().is_success() {
            bail!(
                "registry returned {} for {scope}/{name}@{version}",
                resp.status()
            );
        }

        const MAX_PACKAGE_BYTES: u64 = 200 * 1024 * 1024; // 200 MiB

        if let Some(len) = resp.content_length() {
            if len > MAX_PACKAGE_BYTES {
                bail!("package {scope}/{name}@{version} is {len} bytes, exceeding {MAX_PACKAGE_BYTES} byte limit");
            }
        }

        let bytes = resp
            .bytes()
            .await
            .context("failed to read package bytes")?;

        if bytes.len() as u64 > MAX_PACKAGE_BYTES {
            bail!("package response exceeded size limit");
        }

        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_req_valid() {
        let (scope, name, ver) = parse_version_req("roblox/roact@^17.0.0").unwrap();
        assert_eq!(scope, "roblox");
        assert_eq!(name, "roact");
        assert_eq!(ver, "^17.0.0");
    }

    #[test]
    fn parse_version_req_missing_at() {
        let result = parse_version_req("roblox/roact");
        assert!(result.is_err());
    }

    #[test]
    fn parse_index_entry() {
        let json = r#"{
            "name": "roblox/roact",
            "version": "17.0.1",
            "realm": "shared",
            "dependencies": {},
            "server-dependencies": {},
            "description": "A declarative UI library"
        }"#;
        let entry: IndexEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.name, "roblox/roact");
        assert_eq!(entry.version, "17.0.1");
        assert_eq!(entry.realm, "shared");
    }

    #[test]
    fn validate_identifier_accepts_valid() {
        assert!(validate_identifier("roblox", "scope").is_ok());
        assert!(validate_identifier("my-pkg_01", "name").is_ok());
    }

    #[test]
    fn validate_identifier_rejects_path_traversal() {
        assert!(validate_identifier("../evil", "scope").is_err());
        assert!(validate_identifier("foo/bar", "scope").is_err());
    }

    #[test]
    fn validate_identifier_rejects_empty() {
        assert!(validate_identifier("", "scope").is_err());
    }

    #[test]
    fn parse_version_req_rejects_traversal_scope() {
        let result = parse_version_req("../evil/roact@^1.0.0");
        assert!(result.is_err());
    }
}
