use super::*;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const NORMAL_BUILD: &[(&str, &str)] = &[
    ("emp-app", "emp-codex"),
    ("emp-app", "emp-core"),
    ("emp-app", "emp-history"),
    ("emp-app", "emp-integration"),
    ("emp-app", "emp-modules"),
    ("emp-app", "emp-protocol"),
    ("emp-app", "emp-router"),
    ("emp-app", "emp-state"),
    ("emp-app", "emp-transport"),
    ("emp-codex", "emp-history"),
    ("emp-codex", "emp-state"),
    ("emp-codex", "emp-transport"),
    ("emp-history", "emp-core"),
    ("emp-integration", "emp-state"),
    ("emp-mod-aggregate", "emp-modules"),
    ("emp-mod-decision", "emp-modules"),
    ("emp-mod-gateway", "emp-modules"),
    ("emp-mod-team", "emp-modules"),
    ("emp-mod-team", "emp-state"),
    ("emp-router", "emp-core"),
    ("emp-router", "emp-protocol"),
    ("emp-router", "emp-state"),
    ("emp-router", "emp-transport"),
    ("emp-state", "emp-core"),
    ("emp-state", "emp-modules"),
    ("xtask", "emp-modules"),
    ("xtask", "emp-state"),
];

const DEV: &[(&str, &str)] = &[
    ("emp-app", "emp-mod-aggregate"),
    ("emp-app", "emp-mod-decision"),
    ("emp-app", "emp-mod-gateway"),
];

pub(super) fn check(root: &Path, errors: &mut Vec<String>) -> Result {
    let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .current_dir(root)
        .args([
            "metadata",
            "--offline",
            "--locked",
            "--no-deps",
            "--format-version",
            "1",
        ])
        .output()
        .map_err(|error| format!("[arch] Cargo.toml: cargo metadata: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "[arch] Cargo.toml: cargo metadata 失败；修正 workspace 清单后重试：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let metadata: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("[arch] cargo metadata JSON: {error}"))?;
    dependencies(&metadata, errors)
}

fn dependencies(metadata: &Value, errors: &mut Vec<String>) -> Result {
    let packages = metadata["packages"]
        .as_array()
        .ok_or("[arch] cargo metadata 缺少 packages")?;
    let members = metadata["workspace_members"]
        .as_array()
        .ok_or("[arch] cargo metadata 缺少 workspace_members")?;
    let packages = packages
        .iter()
        .filter(|package| members.contains(&package["id"]))
        .collect::<Vec<_>>();
    let names = packages
        .iter()
        .filter_map(|package| {
            let manifest = package["manifest_path"].as_str()?;
            Some((package["name"].as_str()?, Path::new(manifest).parent()?))
        })
        .collect::<BTreeMap<_, _>>();
    let mut edges = BTreeSet::new();
    for package in &packages {
        let from = package["name"].as_str().ok_or("[arch] package 缺少 name")?;
        for dependency in package["dependencies"]
            .as_array()
            .ok_or("[arch] package 缺少 dependencies")?
        {
            let to = dependency["name"]
                .as_str()
                .ok_or("[arch] dependency 缺少 name")?;
            let Some(path) = dependency["path"].as_str() else {
                continue;
            };
            if names.get(to).copied() != Some(Path::new(path)) {
                continue;
            }
            let dev = dependency["kind"].as_str() == Some("dev");
            if edges.insert((from, to, dev))
                && !(if dev { DEV } else { NORMAL_BUILD }).contains(&(from, to))
            {
                let kind = if dev { "dev" } else { "normal/build" };
                errors.push(format!("[arch] {} [dependencies, {kind}]: 新的 crate 依赖 {from} → {to} 不在允许清单里；如果确实需要，先更新 PLAN §3.2 和 xtask 清单", package["manifest_path"].as_str().unwrap_or("Cargo.toml")));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn arch_checks_dependency_kinds_against_snapshot() {
        for (from, to, kind, valid) in [
            ("emp-app", "emp-core", Value::Null, true),
            ("emp-app", "emp-core", json!("build"), true),
            ("emp-app", "emp-mod-decision", json!("dev"), true),
            ("emp-core", "emp-app", Value::Null, false),
            ("emp-app", "emp-mod-decision", Value::Null, false),
        ] {
            let metadata = json!({"workspace_members": [from, to], "packages": [
                {"id": from, "name": from, "manifest_path": format!("/workspace/{from}/Cargo.toml"),
                 "dependencies": [{"name": to, "path": format!("/workspace/{to}"), "kind": kind}]},
                {"id": to, "name": to, "manifest_path": format!("/workspace/{to}/Cargo.toml"), "dependencies": []}
            ]});
            let mut errors = Vec::new();
            dependencies(&metadata, &mut errors).unwrap();
            assert_eq!(errors.is_empty(), valid, "{from} → {to}: {errors:?}");
            if !valid {
                assert!(errors[0].contains(&format!("{from} → {to}")));
                assert!(errors[0].contains("PLAN §3.2"));
            }
        }
    }
}
