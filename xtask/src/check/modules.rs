use super::*;
use emp_updater::release::module_version;
use std::collections::BTreeMap;

pub(super) fn check(root: &Path, errors: &mut Vec<String>) -> Result {
    let base = match git(root, &["merge-base", "HEAD", "origin/main"]) {
        Ok(output) => output,
        Err(reason) => {
            println!(
                "[module-version] 跳过：无法取得 HEAD 与 origin/main 的共同基点；{reason}；在 CI checkout 设置 fetch-depth: 0"
            );
            return Ok(());
        }
    };
    let base = base.trim();
    let mut changed = git_paths(root, &["diff", "--name-only", "--no-renames", "-z", base])?;
    let list_untracked = ["ls-files", "--others", "--exclude-standard", "-z"];
    changed.extend(git_paths(root, &list_untracked)?);
    let baseline = git_paths(root, &["ls-tree", "-r", "--name-only", "-z", base])?;
    let mut modules = BTreeMap::new();
    for path in changed {
        let parts = path.split('/').collect::<Vec<_>>();
        if parts.len() < 3
            || !((parts[0] == "crates" && parts[1].starts_with("emp-mod-"))
                || (parts[0] == "modules" && parts[1] == "aero"))
            || path.ends_with(".md")
        {
            continue;
        }
        let module = format!("{}/{}", parts[0], parts[1]);
        if path == format!("{module}/Cargo.toml") {
            let before = if baseline.contains(&path) {
                git(root, &["show", &format!("{base}:{path}")])?
            } else {
                String::new()
            };
            let after = if root.join(&path).exists() {
                read(root, &path)?
            } else {
                String::new()
            };
            if without_lints(&before) == without_lints(&after) {
                continue;
            }
        }
        modules.entry(module).or_insert(path);
    }
    for (module, source) in modules {
        let (file, key) = if module == "modules/aero" {
            ("worker.py", "VERSION")
        } else {
            ("Cargo.toml", "version")
        };
        let path = format!("{module}/{file}");
        if !root.join(&module).exists() {
            continue;
        }
        let after = read(root, &path)?;
        if baseline.contains(&path) {
            require_bump(
                &source,
                &path,
                key,
                &git(root, &["show", &format!("{base}:{path}")])?,
                &after,
                errors,
            );
        } else if version(&after, key).is_none() {
            errors.push(format!(
                "[module-version] {path}: 新模组必须声明发布版本 {key}"
            ));
        }
    }
    Ok(())
}

fn without_lints(manifest: &str) -> Vec<&str> {
    let mut lints = false;
    manifest
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            if let Some(section) = trimmed.strip_prefix('[') {
                lints = section == "lints]" || section.starts_with("lints.");
            }
            !lints && !trimmed.is_empty()
        })
        .collect()
}

fn version<'a>(contents: &'a str, key: &str) -> Option<&'a str> {
    let mut package = key == "VERSION";
    for line in contents.lines().map(str::trim) {
        if let Some(section) = line.strip_prefix('[') {
            package = section == "package]";
        }
        if package
            && let Some((name, value)) = line.split_once('=')
            && name.trim() == key
        {
            let value = value.trim().strip_prefix('"')?.split('"').next()?;
            return (!value.is_empty()).then_some(value);
        }
    }
    None
}

fn require_bump(
    source: &str,
    manifest: &str,
    key: &str,
    before: &str,
    after: &str,
    errors: &mut Vec<String>,
) {
    let old = version(before, key);
    let new = version(after, key);
    let increasing = old
        .and_then(module_version)
        .zip(new.and_then(module_version))
        .is_some_and(|(old, new)| new > old);
    if !increasing {
        errors.push(format!("[module-version] {source}: 模组制品按版本发布，改源码必须同时升版本；更新 {manifest} 中的 {key}（基线 {:?}，当前 {:?}）", old, new));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_version_rejects_a_downgrade() {
        let mut errors = Vec::new();
        require_bump(
            "src/main.rs",
            "Cargo.toml",
            "version",
            "[package]\nversion = \"0.10.2-beta\"",
            "[package]\nversion = \"0.10.1-beta\"",
            &mut errors,
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("改源码必须同时升版本"));
    }

    #[test]
    fn module_version_requires_a_new_version() {
        let rust = "[package]\nversion = \"1.0.0\"";
        let aero = "VERSION = \"1.0.0\"";
        for (key, before, after, valid) in [
            ("version", rust, "[package]\nversion = \"1.0.1\"", true),
            ("VERSION", aero, "VERSION = \"1.0.1\"", true),
            ("version", rust, rust, false),
            ("VERSION", aero, "", false),
        ] {
            let mut errors = Vec::new();
            require_bump("source.rs", "Cargo.toml", key, before, after, &mut errors);
            assert_eq!(errors.is_empty(), valid, "{key}: {errors:?}");
            if !valid {
                assert!(errors[0].contains("source.rs"));
                assert!(errors[0].contains("改源码必须同时升版本"));
            }
        }
    }

    #[test]
    fn module_version_ignores_only_cargo_lint_configuration() {
        let manifest = "[package]\nversion = \"1.0.0\"\n\n[dependencies]\nserde = \"1\"\n";
        let lint_only = format!("{manifest}\n[lints]\nworkspace = true\n");
        assert_eq!(without_lints(manifest), without_lints(&lint_only));
        assert_ne!(
            without_lints(manifest),
            without_lints(&manifest.replace("serde = \"1\"", "serde = \"2\""))
        );
    }
}
