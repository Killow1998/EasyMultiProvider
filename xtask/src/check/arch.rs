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
    ("emp-app", "emp-updater"),
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
    ("emp-state", "emp-files"),
    ("emp-state", "emp-modules"),
    ("emp-state", "emp-storage"),
    ("emp-storage", "emp-files"),
    ("emp-updater", "emp-files"),
    ("xtask", "emp-modules"),
    ("xtask", "emp-updater"),
];

const DEV: &[(&str, &str)] = &[
    ("emp-app", "emp-mod-aggregate"),
    ("emp-app", "emp-mod-decision"),
    ("emp-app", "emp-mod-gateway"),
];

const FORBIDDEN: &[(&str, &str, &str)] = &[
    ("emp-server", "emp-app", "服务端不能依赖桌面壳"),
    ("emp-server", "emp-mod-*", "生产服务端不链接任何模组"),
    ("emp-mod-*", "emp-server", "模组只经宿主协议访问 Core"),
    ("emp-mod-*", "emp-app", "模组只经宿主协议访问 Core"),
    (
        "emp-router",
        "emp-config",
        "路由只接收已解析的请求，不读配置",
    ),
    ("emp-router", "emp-storage", "路由不直接读写存储"),
    ("emp-updater", "emp-config", "更新器只依赖文件层"),
    ("emp-updater", "emp-storage", "更新器只依赖文件层"),
    ("emp-updater", "emp-codex", "更新器只依赖文件层"),
    ("emp-files", "emp-*", "文件层是叶子"),
    ("emp-domain", "emp-*", "领域值是叶子"),
];

/// Forbidden paths that still exist until the named PLAN step removes them.
const TRANSITIONAL: &[(&str, &str, &str)] = &[("emp-router", "emp-storage", "C7")];

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
    forbidden(&edges, TRANSITIONAL, errors);
    Ok(())
}

fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

fn forbidden(
    edges: &BTreeSet<(&str, &str, bool)>,
    transitional: &[(&str, &str, &str)],
    errors: &mut Vec<String>,
) {
    let mut used = BTreeSet::new();
    let mut graph = BTreeMap::<&str, Vec<&str>>::new();
    for &(from, to, dev) in edges {
        if !dev {
            graph.entry(from).or_default().push(to);
        }
    }
    for &start in graph.keys() {
        let mut parent = BTreeMap::from([(start, start)]);
        let mut queue = std::collections::VecDeque::from([start]);
        while let Some(node) = queue.pop_front() {
            for &next in graph.get(node).into_iter().flatten() {
                if parent.contains_key(next) {
                    continue;
                }
                parent.insert(next, node);
                queue.push_back(next);
                let Some((_, _, reason)) = FORBIDDEN
                    .iter()
                    .find(|(from, to, _)| matches(from, start) && matches(to, next))
                else {
                    continue;
                };
                if transitional
                    .iter()
                    .any(|(from, to, _)| *from == start && *to == next)
                {
                    used.insert((start, next));
                    continue;
                }
                let (mut path, mut node) = (vec![next], next);
                while node != start {
                    node = parent[node];
                    path.push(node);
                }
                path.reverse();
                errors.push(format!("[arch] 禁止的依赖 {start} ⇢ {next}（{}）：{reason}。这是 PLAN §3.2 的硬性约束，不能靠加允许清单绕过", path.join(" → ")));
            }
        }
    }
    for (from, to, step) in transitional {
        if !used.contains(&(*from, *to)) {
            errors.push(format!(
                "[arch] 过渡豁免 {from} ⇢ {to}（{step} 解除）已不再需要，从 TRANSITIONAL 删除"
            ));
        }
    }
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

    #[test]
    fn forbidden_dependencies_are_checked_transitively_and_ignore_dev_edges() {
        let edges = BTreeSet::from([
            ("emp-router", "emp-mid", false),
            ("emp-mid", "emp-storage", false),
            ("emp-files", "emp-domain", false),
            ("emp-domain", "emp-mod-team", true),
            ("emp-app", "emp-storage", false),
        ]);
        let mut errors = Vec::new();
        forbidden(&edges, &[], &mut errors);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors.iter().any(|error| {
            error.contains("emp-router ⇢ emp-storage（emp-router → emp-mid → emp-storage）")
        }));
        assert!(
            errors
                .iter()
                .any(|error| error.contains("emp-files ⇢ emp-domain（emp-files → emp-domain）"))
        );
    }

    #[test]
    fn transitional_exemptions_cover_named_paths_and_must_be_removed_when_unused() {
        let edges = BTreeSet::from([
            ("emp-router", "emp-state", false),
            ("emp-state", "emp-storage", false),
        ]);
        let mut errors = Vec::new();
        forbidden(&edges, &[("emp-router", "emp-storage", "C7")], &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        forbidden(&edges, &[("emp-router", "emp-config", "C7")], &mut errors);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[1].contains("过渡豁免 emp-router ⇢ emp-config（C7 解除）已不再需要"));
    }
}
