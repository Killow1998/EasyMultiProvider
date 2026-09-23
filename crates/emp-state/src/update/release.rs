//! Stable release metadata validation and platform asset selection.
use super::{Result, UpdateError};
use serde::Deserialize;

pub const REPOSITORY_URL: &str = "https://github.com/Killow1998/EasyMultiProvider";
pub const RELEASE_API_URL: &str =
    "https://api.github.com/repos/Killow1998/EasyMultiProvider/releases/latest";
pub const MAX_RELEASE_BYTES: usize = 1024 * 1024;
pub const MAX_PACKAGE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct UpdateEndpoints {
    pub repository_url: String,
    pub release_api_url: String,
    pub allow_loopback_http: bool,
}

impl Default for UpdateEndpoints {
    fn default() -> Self {
        Self {
            repository_url: REPOSITORY_URL.to_owned(),
            release_api_url: RELEASE_API_URL.to_owned(),
            allow_loopback_http: false,
        }
    }
}

impl UpdateEndpoints {
    /// Construct an endpoint set for a controlled update source, such as a local release server.
    /// Plain HTTP remains limited to loopback addresses by the client policy.
    pub fn for_source(
        repository_url: impl Into<String>,
        release_api_url: impl Into<String>,
    ) -> Self {
        Self {
            repository_url: repository_url.into(),
            release_api_url: release_api_url.into(),
            allow_loopback_http: true,
        }
    }

    pub(crate) fn asset_url(&self, tag: &str, name: &str) -> String {
        format!(
            "{}/releases/download/{tag}/{name}",
            self.repository_url.trim_end_matches('/')
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Asset {
    pub version: String,
    pub name: String,
    pub url: String,
    pub digest: String,
    pub size: u64,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    digest: Option<String>,
    size: serde_json::Value,
    browser_download_url: String,
}

pub(crate) fn parse_version(value: &str) -> Result<(u64, u64, u64)> {
    let value = value.strip_prefix('v').unwrap_or(value);
    parse_stable_version(value)
}

fn normalize_current_version(value: &str) -> Result<(u64, u64, u64)> {
    let stable = value
        .rsplit_once("beta")
        .filter(|(_, suffix)| suffix.bytes().all(|byte| byte.is_ascii_digit()))
        .map_or(value, |(prefix, _)| prefix);
    parse_version(stable)
}

fn parse_stable_version(value: &str) -> Result<(u64, u64, u64)> {
    let mut components = value.split('.');
    let major = components.next().and_then(|part| part.parse().ok());
    let minor = components.next().and_then(|part| part.parse().ok());
    let patch = components.next().and_then(|part| part.parse().ok());
    match (major, minor, patch, components.next()) {
        (Some(major), Some(minor), Some(patch), None) => Ok((major, minor, patch)),
        _ => Err(UpdateError("invalid_release")),
    }
}

pub(crate) fn asset_name(os: &str, arch: &str) -> Result<String> {
    match (os, arch) {
        ("windows", "x86_64") => Ok("EMP.exe".to_owned()),
        ("linux", "x86_64") => Ok("EMP-linux-x86_64.tar.gz".to_owned()),
        ("macos", "x86_64") => Ok("EMP-macos-x86_64.dmg".to_owned()),
        ("macos", "aarch64") => Ok("EMP-macos-arm64.dmg".to_owned()),
        _ => Err(UpdateError("unsupported_platform")),
    }
}

pub(crate) fn current_asset_name() -> Result<String> {
    asset_name(std::env::consts::OS, std::env::consts::ARCH)
}

pub(crate) fn latest_asset(
    raw: &[u8],
    desired_name: &str,
    current_version: &str,
    endpoints: &UpdateEndpoints,
) -> Result<Option<Asset>> {
    let decoded: serde_json::Value =
        serde_json::from_slice(raw).map_err(|_| UpdateError("update_failed"))?;
    let release: Release =
        serde_json::from_value(decoded).map_err(|_| UpdateError("invalid_release"))?;
    if release.draft || release.prerelease {
        return Err(UpdateError("invalid_release"));
    }
    let version = parse_version(&release.tag_name)?;
    let comparison_version = normalize_current_version(current_version)?;
    if version <= comparison_version {
        return Ok(None);
    }

    let mut matches = release
        .assets
        .iter()
        .filter(|asset| asset.name == desired_name);
    let asset = matches.next().ok_or(UpdateError("package_missing"))?;
    if matches.next().is_some() {
        return Err(UpdateError("package_missing"));
    }
    let digest = asset
        .digest
        .as_deref()
        .and_then(|value| value.strip_prefix("sha256:"))
        .filter(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or(UpdateError("checksum_missing"))?;
    if asset.browser_download_url != endpoints.asset_url(&release.tag_name, desired_name) {
        return Err(UpdateError("invalid_download_url"));
    }
    let size = asset
        .size
        .as_u64()
        .ok_or(UpdateError("invalid_package_size"))?;
    if size == 0 || size > MAX_PACKAGE_BYTES {
        return Err(UpdateError("invalid_package_size"));
    }
    Ok(Some(Asset {
        version: release.tag_name.trim_start_matches('v').to_owned(),
        name: desired_name.to_owned(),
        url: asset.browser_download_url.clone(),
        digest: digest.to_owned(),
        size,
    }))
}

#[cfg(test)]
mod tests {
    use super::{UpdateEndpoints, asset_name, latest_asset, parse_version};

    fn release(tag: &str, name: &str, size: serde_json::Value, digest: &str, url: &str) -> Vec<u8> {
        serde_json::json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "assets": [{
                "name": name,
                "digest": digest,
                "size": size,
                "browser_download_url": url,
            }],
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn release_version_tags_are_stable_and_current_beta_uses_its_base() {
        let endpoints =
            UpdateEndpoints::for_source("https://example.test/emp", "https://example.test/latest");
        let url = endpoints.asset_url("v0.9.9", "EMP-linux-x86_64.tar.gz");
        let raw = release(
            "v0.9.9",
            "EMP-linux-x86_64.tar.gz",
            100.into(),
            &format!("sha256:{}", "a".repeat(64)),
            &url,
        );
        assert!(
            latest_asset(&raw, "EMP-linux-x86_64.tar.gz", "0.9.9beta", &endpoints)
                .unwrap()
                .is_none()
        );
        assert!(
            latest_asset(&raw, "EMP-linux-x86_64.tar.gz", "0.9.8beta", &endpoints)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            parse_version("v0.9.9beta"),
            Err(super::UpdateError("invalid_release"))
        );
        assert_eq!(
            parse_version("v0.9.9-beta"),
            Err(super::UpdateError("invalid_release"))
        );
    }

    #[test]
    fn release_asset_names_match_the_python_platform_contract() {
        assert_eq!(asset_name("windows", "x86_64").unwrap(), "EMP.exe");
        assert_eq!(
            asset_name("linux", "x86_64").unwrap(),
            "EMP-linux-x86_64.tar.gz"
        );
        assert_eq!(
            asset_name("macos", "x86_64").unwrap(),
            "EMP-macos-x86_64.dmg"
        );
        assert_eq!(
            asset_name("macos", "aarch64").unwrap(),
            "EMP-macos-arm64.dmg"
        );
        assert_eq!(
            asset_name("linux", "aarch64"),
            Err(super::UpdateError("unsupported_platform"))
        );
    }

    #[test]
    fn release_metadata_requires_one_valid_digest_url_and_integer_size() {
        let endpoints =
            UpdateEndpoints::for_source("http://127.0.0.1:4200", "http://127.0.0.1:4200/latest");
        let name = "EMP-linux-x86_64.tar.gz";
        let url = endpoints.asset_url("v0.9.9", name);
        let valid_digest = format!("sha256:{}", "b".repeat(64));
        let raw = release("v0.9.9", name, true.into(), &valid_digest, &url);
        assert_eq!(
            latest_asset(&raw, name, "0.9.8", &endpoints),
            Err(super::UpdateError("invalid_package_size"))
        );
        let raw = release("v0.9.9", name, 1.into(), "sha256:BAD", &url);
        assert_eq!(
            latest_asset(&raw, name, "0.9.8", &endpoints),
            Err(super::UpdateError("checksum_missing"))
        );
        let raw = release(
            "v0.9.9",
            name,
            1.into(),
            &valid_digest,
            "http://127.0.0.1:4200/elsewhere",
        );
        assert_eq!(
            latest_asset(&raw, name, "0.9.8", &endpoints),
            Err(super::UpdateError("invalid_download_url"))
        );
        let raw = release("v0.9.9beta", name, 1.into(), &valid_digest, &url);
        assert_eq!(
            latest_asset(&raw, name, "0.9.8", &endpoints),
            Err(super::UpdateError("invalid_release"))
        );
    }
}
