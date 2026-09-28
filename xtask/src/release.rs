//! Release gates: the complete asset set must match the source version and
//! its checksums, and the GitHub release notes come from CHANGELOG.md.
use crate::{Result, VERSION, project_root, sha256_file};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const PRIMARY_ARTIFACTS: [&str; 11] = [
    "EMP.exe",
    "EMP.zip",
    "EMP-linux-x86_64",
    "EMP-linux-x86_64.tar.gz",
    "EMP-linux-x86_64-install.sh",
    "EMP-macos-x86_64",
    "EMP-macos-x86_64.tar.gz",
    "EMP-macos-x86_64.dmg",
    "EMP-macos-arm64",
    "EMP-macos-arm64.tar.gz",
    "EMP-macos-arm64.dmg",
];

/// The downloads shown on the release page.
const PUBLIC_ARTIFACTS: [&str; 5] = [
    "EMP-linux-x86_64-install.sh",
    "EMP-linux-x86_64.tar.gz",
    "EMP-macos-arm64.dmg",
    "EMP-macos-x86_64.dmg",
    "EMP.exe",
];

pub fn validate_command(args: &[String]) -> Result {
    let usage = || {
        "usage: cargo xtask validate-release --tag TAG --artifacts DIR [--public-manifest FILE]"
            .to_owned()
    };
    let (mut tag, mut artifacts, mut manifest) = (None, None, None);
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(usage)?;
        match flag.as_str() {
            "--tag" => tag = Some(value.clone()),
            "--artifacts" => artifacts = Some(PathBuf::from(value)),
            "--public-manifest" => manifest = Some(PathBuf::from(value)),
            _ => return Err(usage()),
        }
    }
    let (tag, artifacts) = tag.zip(artifacts).ok_or_else(usage)?;
    validate(&tag, &artifacts, VERSION)
        .map_err(|error| format!("release validation failed: {error}"))?;
    if let Some(manifest) = manifest {
        let lines: String = PUBLIC_ARTIFACTS
            .iter()
            .map(|name| format!("{}\n", artifacts.join(name).display()))
            .collect();
        fs::write(&manifest, lines)
            .map_err(|error| format!("write {}: {error}", manifest.display()))?;
    }
    println!(
        "release assets verified: {} files; {} public downloads for v{VERSION}",
        PRIMARY_ARTIFACTS.len() * 2,
        PUBLIC_ARTIFACTS.len()
    );
    Ok(())
}

fn validate(tag: &str, artifacts: &Path, version: &str) -> Result {
    if tag != format!("v{version}") {
        return Err(format!(
            "release tag {tag:?} does not match source version \"v{version}\""
        ));
    }
    let entries = fs::read_dir(artifacts)
        .map_err(|_| format!("artifact directory does not exist: {}", artifacts.display()))?;
    let mut actual = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.path().is_file() {
            actual.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    let expected: BTreeSet<String> = PRIMARY_ARTIFACTS
        .iter()
        .flat_map(|name| [(*name).to_owned(), format!("{name}.sha256")])
        .collect();
    if actual != expected {
        let list = |names: Vec<&String>| {
            if names.is_empty() {
                "none".to_owned()
            } else {
                names.into_iter().cloned().collect::<Vec<_>>().join(", ")
            }
        };
        return Err(format!(
            "release manifest mismatch; missing: {}; unexpected: {}",
            list(expected.difference(&actual).collect()),
            list(actual.difference(&expected).collect())
        ));
    }
    for name in PRIMARY_ARTIFACTS {
        let sidecar = format!("{name}.sha256");
        let text = fs::read_to_string(artifacts.join(&sidecar))
            .map_err(|error| format!("{sidecar}: {error}"))?;
        let Some((digest, recorded)) = text.trim().split_once(char::is_whitespace) else {
            return Err(format!("invalid checksum sidecar: {sidecar}"));
        };
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(format!("invalid checksum digest: {sidecar}"));
        }
        if recorded.trim() != name {
            return Err(format!("checksum filename mismatch in {sidecar}"));
        }
        if sha256_file(&artifacts.join(name))? != digest {
            return Err(format!("checksum mismatch: {name}"));
        }
    }
    Ok(())
}

pub fn notes_command(args: &[String]) -> Result {
    let [tag] = args else {
        return Err("usage: cargo xtask release-notes TAG".to_owned());
    };
    let changelog = fs::read_to_string(project_root().join("CHANGELOG.md"))
        .map_err(|error| error.to_string())?;
    println!("{}", release_notes(&changelog, tag)?);
    Ok(())
}

fn release_notes<'a>(changelog: &'a str, tag: &str) -> Result<&'a str> {
    let version = tag.strip_prefix('v').unwrap_or(tag);
    let heading = format!("## {version} (");
    let start = changelog
        .match_indices(&heading)
        .find(|(index, _)| *index == 0 || changelog[..*index].ends_with('\n'))
        .and_then(|(index, _)| changelog[index..].find('\n').map(|end| index + end + 1));
    let section = start.map(|start| {
        let rest = &changelog[start..];
        let end = rest.find("\n## ").map_or(rest.len(), |end| end + 1);
        rest[..end].trim()
    });
    section
        .filter(|section| !section.is_empty())
        .ok_or_else(|| format!("Missing release notes for {tag}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANGELOG: &str = "# Changelog\n\n## Unreleased\n\n- next\n\n## 0.12.2 (2026-09-28)\n\n- first\n- second\n\n## 0.12.1 (2026-09-25)\n\n- older\n";

    #[test]
    fn release_notes_take_only_the_tagged_section() {
        assert_eq!(
            release_notes(CHANGELOG, "v0.12.2").unwrap(),
            "- first\n- second"
        );
        assert_eq!(release_notes(CHANGELOG, "v0.12.1").unwrap(), "- older");
        assert!(release_notes(CHANGELOG, "v0.12.3").is_err());
        assert!(release_notes("## 0.1.0 (x)\n\n## 0.0.9 (y)\n- z\n", "v0.1.0").is_err());
    }

    fn complete_set(directory: &Path) {
        for name in PRIMARY_ARTIFACTS {
            let path = directory.join(name);
            fs::write(&path, name).unwrap();
            fs::write(
                directory.join(format!("{name}.sha256")),
                format!("{}  {name}\n", sha256_file(&path).unwrap()),
            )
            .unwrap();
        }
    }

    #[test]
    fn validation_accepts_a_complete_matching_set() {
        let directory = tempfile::tempdir().unwrap();
        complete_set(directory.path());
        validate("v0.12.2", directory.path(), "0.12.2").unwrap();
    }

    #[test]
    fn validation_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        complete_set(directory.path());
        assert!(
            validate("v0.12.1", directory.path(), "0.12.2")
                .unwrap_err()
                .contains("does not match")
        );

        fs::write(directory.path().join("EMP.exe"), "tampered").unwrap();
        assert!(
            validate("v0.12.2", directory.path(), "0.12.2")
                .unwrap_err()
                .contains("checksum mismatch")
        );

        complete_set(directory.path());
        fs::write(directory.path().join("extra.txt"), "").unwrap();
        assert!(
            validate("v0.12.2", directory.path(), "0.12.2")
                .unwrap_err()
                .contains("unexpected: extra.txt")
        );

        fs::remove_file(directory.path().join("extra.txt")).unwrap();
        fs::remove_file(directory.path().join("EMP.zip.sha256")).unwrap();
        assert!(
            validate("v0.12.2", directory.path(), "0.12.2")
                .unwrap_err()
                .contains("missing: EMP.zip.sha256")
        );
    }
}
