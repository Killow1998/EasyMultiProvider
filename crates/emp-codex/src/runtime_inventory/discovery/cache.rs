//! Narrow Windows App cache fallback, matched to the registered official resource.
use crate::runtime_inventory::ExecutableIdentity;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

type Digests = Vec<(ExecutableIdentity, [u8; 32])>;
static DIGESTS: OnceLock<Mutex<Digests>> = OnceLock::new();

fn no_redirect(path: &Path) -> bool {
    path.ancestors().all(|part| {
        let Ok(meta) = std::fs::symlink_metadata(part) else {
            return false;
        };
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            meta.file_attributes() & 0x400 == 0
        }
        #[cfg(not(windows))]
        {
            !meta.file_type().is_symlink()
        }
    })
}
fn digest(path: &Path) -> Option<[u8; 32]> {
    let before = ExecutableIdentity::read(path)?;
    // Bound hashing of the known engine asset, not user conversation data.
    if before.length > 1024 * 1024 * 1024 {
        return None;
    }
    let mut cache = DIGESTS.get_or_init(|| Mutex::new(Vec::new())).lock().ok()?;
    if let Some((_, hash)) = cache.iter().find(|(identity, _)| identity == &before) {
        return Some(*hash);
    }
    let mut file = std::fs::File::open(path).ok()?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut read = 0_u64;
    loop {
        let count = file.read(&mut buffer).ok()?;
        if count == 0 {
            break;
        }
        read += count as u64;
        if read > before.length {
            return None;
        }
        hash.update(&buffer[..count]);
    }
    if read != before.length || ExecutableIdentity::read(path).as_ref() != Some(&before) {
        return None;
    }
    let hash: [u8; 32] = hash.finalize().into();
    cache.retain(|(identity, _)| identity.canonical != before.canonical);
    if cache.len() >= 128 {
        cache.remove(0);
    }
    cache.push((before, hash));
    Some(hash)
}

pub(super) fn matches_resource(resource: &Path, candidate: &Path) -> bool {
    if !resource.is_file()
        || !candidate.is_file()
        || !no_redirect(resource)
        || !no_redirect(candidate)
    {
        return false;
    }
    match (digest(resource), digest(candidate)) {
        (Some(expected), Some(actual)) => expected == actual,
        _ => false,
    }
}

pub(super) fn verified_candidates(resource: &Path, cache_root: &Path) -> Vec<PathBuf> {
    if !resource.is_file() || !no_redirect(resource) || !no_redirect(cache_root) {
        return Vec::new();
    }
    let Some(root) = cache_root.canonicalize().ok() else {
        return Vec::new();
    };
    let Some(length) = resource.metadata().ok().map(|meta| meta.len()) else {
        return Vec::new();
    };
    let Ok(entries) = cache_root.read_dir() else {
        return Vec::new();
    };
    let entries = entries.take(65).collect::<Vec<_>>();
    if entries.len() > 64 {
        return Vec::new();
    }
    let mut verified = Vec::new();
    for entry in entries.into_iter().flatten() {
        let path = entry.path().join("codex.exe");
        if !path.is_file()
            || !no_redirect(&path)
            || path.metadata().ok().map(|m| m.len()) != Some(length)
        {
            continue;
        }
        let Some(path) = path.canonicalize().ok().filter(|p| p.starts_with(&root)) else {
            continue;
        };
        if matches_resource(resource, &path) {
            verified.push(path);
        }
    }
    verified.sort(); // Stable selection only among independently verified identical bytes.
    verified
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_requires_official_bytes_and_invalidates_replaced_files() {
        let dir = tempfile::tempdir().unwrap();
        let resource = dir.path().join("official.exe");
        let cache = dir.path().join("cache");
        let engine = cache.join("opaque/codex.exe");
        std::fs::create_dir_all(engine.parent().unwrap()).unwrap();
        std::fs::write(&resource, b"official engine").unwrap();
        std::fs::write(&engine, b"incorrect bytes").unwrap();
        assert!(verified_candidates(&resource, &cache).is_empty());
        std::fs::copy(&resource, &engine).unwrap();
        assert_eq!(
            verified_candidates(&resource, &cache),
            vec![engine.canonicalize().unwrap()]
        );
        std::fs::write(&resource, b"new official version").unwrap();
        assert!(verified_candidates(&resource, &cache).is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn denied_resource_uses_only_verified_executable_cache() {
        use crate::runtime_inventory::{
            discovery::Candidate, observe_candidate, trust::tests::private_dir,
        };
        use std::os::unix::fs::PermissionsExt;
        let dir = private_dir();
        let resource = dir.path().join("official.exe");
        let cache = dir.path().join("cache");
        let engine = cache.join("opaque/codex.exe");
        std::fs::create_dir_all(engine.parent().unwrap()).unwrap();
        std::fs::write(&resource, b"#!/bin/sh\necho codex-cli 0.155.0-alpha.9.2\n").unwrap();
        std::fs::copy(&resource, &engine).unwrap();
        std::fs::set_permissions(&resource, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o700)).unwrap();
        let candidate = Candidate {
            source: "codex_app",
            name: "ChatGPT App (Codex)",
            path: resource.clone(),
            unavailable: false,
            host_version: Some("26.915.4065.0".into()),
            fallbacks: verified_candidates(&resource, &cache),
        };
        let observed = observe_candidate(&candidate);
        assert_eq!(observed["installed"], "0.155.0-alpha.9.2");
        assert_eq!(observed["status"], "available");
        assert_eq!(
            observed["path"],
            engine.canonicalize().unwrap().to_str().unwrap()
        );
    }
    #[cfg(unix)]
    #[test]
    fn cache_rejects_redirected_children() {
        let dir = tempfile::tempdir().unwrap();
        let resource = dir.path().join("codex.exe");
        let cache = dir.path().join("cache");
        std::fs::create_dir(&cache).unwrap();
        std::fs::write(&resource, b"official engine").unwrap();
        std::os::unix::fs::symlink(dir.path(), cache.join("escaped")).unwrap();
        assert!(verified_candidates(&resource, &cache).is_empty());
    }
}
