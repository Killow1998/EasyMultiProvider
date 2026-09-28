//! `--emp-migrate-config SOURCE TARGET`: the Linux installer moves an older
//! configuration, its `state/` directory and the files it references into the
//! desktop configuration directory. Anything replaced is moved into a sibling
//! backup directory whose path is printed; any failure restores the target.
use serde_json::Value;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const PATH_FIELDS: [&str; 3] = [
    "account_store_path",
    "secret_store_path",
    "native_catalog_path",
];

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Copies a tree without following symbolic links, which are recreated as-is.
fn copy_entry(source: &Path, destination: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(fs::read_link(source)?, destination)?;
        #[cfg(not(unix))]
        return Err(invalid("symbolic links are not supported here"));
    } else if metadata.is_dir() {
        fs::create_dir(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_entry(&entry.path(), &destination.join(entry.file_name()))?;
        }
        fs::set_permissions(destination, metadata.permissions())?;
    } else {
        fs::copy(source, destination)?;
    }
    Ok(())
}

struct Migration {
    source_root: PathBuf,
    target_root: PathBuf,
    stage: PathBuf,
}

impl Migration {
    /// Stages a referenced file that lives under the source directory and
    /// returns the value pointing at its new location.
    fn copy_reference(&self, value: &Value) -> io::Result<Value> {
        let Some(text) = value.as_str().filter(|text| !text.trim().is_empty()) else {
            return Ok(value.clone());
        };
        let raw = match (text.strip_prefix("~/"), std::env::var_os("HOME")) {
            (Some(rest), Some(home)) => Path::new(&home).join(rest),
            _ => PathBuf::from(text),
        };
        let source = if raw.is_absolute() {
            raw.clone()
        } else {
            self.source_root.join(&raw)
        };
        let resolved = source.canonicalize().unwrap_or_else(|_| source.clone());
        let Some(relative) = resolved
            .strip_prefix(&self.source_root)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
        else {
            return Ok(value.clone());
        };
        if fs::symlink_metadata(&source).is_ok() {
            let staged = self.stage.join(relative);
            if fs::symlink_metadata(&staged).is_err() {
                fs::create_dir_all(staged.parent().expect("staged path has a parent"))?;
                copy_entry(&source, &staged)?;
            }
        }
        if raw.is_absolute() {
            Ok(Value::String(
                self.target_root
                    .join(relative)
                    .to_string_lossy()
                    .into_owned(),
            ))
        } else {
            Ok(value.clone())
        }
    }

    fn rewrite(&self, config: &mut Value) -> io::Result<()> {
        for field in PATH_FIELDS {
            if let Some(value) = config.get(field) {
                config[field] = self.copy_reference(value)?;
            }
        }
        for (list, field) in [("accounts", "auth_file"), ("providers", "api_key_file")] {
            let Some(items) = config.get_mut(list).and_then(Value::as_array_mut) else {
                continue;
            };
            for item in items.iter_mut().filter(|item| item.is_object()) {
                if let Some(value) = item.get(field) {
                    item[field] = self.copy_reference(value)?;
                }
            }
        }
        Ok(())
    }
}

fn private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    io::Write::write_all(&mut options.open(path)?, contents)
}

/// Runs the migration and returns the backup directory when anything was replaced.
pub(crate) fn migrate(source_config: &Path, target_config: &Path) -> io::Result<Option<PathBuf>> {
    let source_metadata = fs::symlink_metadata(source_config)?;
    if !source_metadata.is_file() {
        return Err(invalid(
            "the selected source configuration is not a regular file",
        ));
    }
    let source_root = source_config
        .parent()
        .ok_or_else(|| invalid("the source configuration has no directory"))?
        .canonicalize()?;
    let target_parent = target_config
        .parent()
        .ok_or_else(|| invalid("the destination configuration has no directory"))?;
    if fs::symlink_metadata(target_parent)?
        .file_type()
        .is_symlink()
    {
        return Err(invalid(
            "the destination configuration directory is a symbolic link",
        ));
    }
    let target_root = target_parent.canonicalize()?;
    if source_root == target_root {
        return Err(invalid(
            "the selected configuration is already in the destination",
        ));
    }
    let target_config = target_root.join(
        target_config
            .file_name()
            .ok_or_else(|| invalid("invalid destination"))?,
    );
    let mut config: Value = serde_json::from_slice(&fs::read(source_config)?)
        .map_err(|_| invalid("the selected source configuration is not valid JSON"))?;
    let backup_parent = target_root
        .parent()
        .ok_or_else(|| invalid("the destination configuration directory has no parent"))?;

    let stage = tempfile::Builder::new()
        .prefix(".emp-migration-")
        .tempdir_in(&target_root)?;
    let migration = Migration {
        source_root: source_root.clone(),
        target_root: target_root.clone(),
        stage: stage.path().to_owned(),
    };
    let state_source = source_root.join("state");
    if let Ok(metadata) = fs::symlink_metadata(&state_source) {
        if metadata.file_type().is_symlink() {
            return Err(invalid("the source state directory is a symbolic link"));
        }
        if metadata.is_dir() {
            copy_entry(&state_source, &stage.path().join("state"))?;
        }
    }
    migration.rewrite(&mut config)?;
    let mut rendered = serde_json::to_vec_pretty(&config).map_err(io::Error::other)?;
    rendered.push(b'\n');
    let mut content: Vec<PathBuf> = fs::read_dir(stage.path())?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<_>>()?;
    content.sort();

    let backup = tempfile::Builder::new()
        .prefix("easy-multi-provider-backup-")
        .tempdir_in(backup_parent)?;
    let mut backed_up: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut published: Vec<PathBuf> = Vec::new();
    let result = (|| -> io::Result<()> {
        let destinations = std::iter::once(target_config.clone()).chain(
            content
                .iter()
                .map(|path| target_root.join(path.file_name().expect("staged entry"))),
        );
        for destination in destinations {
            if fs::symlink_metadata(&destination).is_ok() {
                let stored = backup
                    .path()
                    .join(destination.file_name().expect("destination name"));
                fs::rename(&destination, &stored)?;
                backed_up.push((destination, stored));
            }
        }
        for path in &content {
            let destination = target_root.join(path.file_name().expect("staged entry"));
            fs::rename(path, &destination)?;
            published.push(destination);
        }
        private_file(&target_config, &rendered)?;
        published.push(target_config.clone());
        Ok(())
    })();
    if let Err(error) = result {
        for path in published.iter().rev() {
            let _ = if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
        }
        for (destination, stored) in backed_up.iter().rev() {
            let _ = fs::rename(stored, destination);
        }
        return Err(error);
    }
    if backed_up.is_empty() {
        return Ok(None);
    }
    Ok(Some(backup.keep()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("old/EasyMultiProvider");
        let target = root.path().join("config/easy-multi-provider");
        fs::create_dir_all(source.join("state/secrets")).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(source.join("state/secrets/key.enc"), "secret").unwrap();
        fs::write(source.join("auth.json"), "{}").unwrap();
        let config = json!({
            "port": 4200,
            "secret_store_path": source.join("state/secrets").to_string_lossy(),
            "accounts": [{"auth_file": source.join("auth.json").to_string_lossy()}, "ignored"],
            "providers": [{"api_key_file": "state/secrets/key.enc"}, {"api_key_file": "/elsewhere/key"}],
        });
        fs::write(source.join("config.json"), config.to_string()).unwrap();
        (root, source, target)
    }

    #[test]
    fn moves_state_and_referenced_files_and_rewrites_absolute_paths() {
        let (_root, source, target) = setup();
        let backup = migrate(&source.join("config.json"), &target.join("config.json")).unwrap();
        assert_eq!(backup, None);
        let target = target.canonicalize().unwrap();
        let config: Value =
            serde_json::from_slice(&fs::read(target.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            config["secret_store_path"],
            json!(target.join("state/secrets").to_string_lossy())
        );
        assert_eq!(
            config["accounts"][0]["auth_file"],
            json!(target.join("auth.json").to_string_lossy())
        );
        assert_eq!(config["accounts"][1], json!("ignored"));
        assert_eq!(
            config["providers"][0]["api_key_file"],
            json!("state/secrets/key.enc")
        );
        assert_eq!(
            config["providers"][1]["api_key_file"],
            json!("/elsewhere/key")
        );
        assert_eq!(
            fs::read_to_string(target.join("state/secrets/key.enc")).unwrap(),
            "secret"
        );
        assert_eq!(fs::read_to_string(target.join("auth.json")).unwrap(), "{}");
        assert!(
            source.join("config.json").is_file(),
            "the source is copied, not moved"
        );
        let leftovers: Vec<_> = fs::read_dir(&target)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with(".emp-migration-"))
            .collect();
        assert!(leftovers.is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(target.join("config.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn replaced_configuration_and_state_are_kept_in_a_backup() {
        let (_root, source, target) = setup();
        fs::write(target.join("config.json"), "previous").unwrap();
        fs::create_dir(target.join("state")).unwrap();
        fs::write(target.join("state/old"), "old state").unwrap();
        let backup = migrate(&source.join("config.json"), &target.join("config.json"))
            .unwrap()
            .expect("a backup was made");
        assert_eq!(
            fs::read_to_string(backup.join("config.json")).unwrap(),
            "previous"
        );
        assert_eq!(
            fs::read_to_string(backup.join("state/old")).unwrap(),
            "old state"
        );
        assert!(!target.join("state/old").exists());
        assert!(target.join("state/secrets/key.enc").is_file());
    }

    #[test]
    fn rejects_same_directory_symlinked_state_and_invalid_json() {
        let (_root, source, target) = setup();
        assert!(migrate(&source.join("config.json"), &source.join("config.json")).is_err());
        fs::write(source.join("config.json"), "not json").unwrap();
        assert!(migrate(&source.join("config.json"), &target.join("config.json")).is_err());
        #[cfg(unix)]
        {
            fs::write(source.join("config.json"), "{}").unwrap();
            fs::rename(source.join("state"), source.join("real-state")).unwrap();
            std::os::unix::fs::symlink(source.join("real-state"), source.join("state")).unwrap();
            assert!(migrate(&source.join("config.json"), &target.join("config.json")).is_err());
        }
        assert!(!target.join("config.json").exists());
    }
}
