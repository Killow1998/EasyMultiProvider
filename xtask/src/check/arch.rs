use super::*;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const NORMAL_BUILD: &[(&str, &str)] = &[
    ("emp-app", "emp-codex"),
    ("emp-app", "emp-domain"),
    ("emp-app", "emp-context"),
    ("emp-app", "emp-modules"),
    ("emp-app", "emp-protocol"),
    ("emp-app", "emp-router"),
    ("emp-app", "emp-state"),
    ("emp-app", "emp-storage"),
    ("emp-app", "emp-transport"),
    ("emp-app", "emp-updater"),
    ("emp-codex", "emp-context"),
    ("emp-codex", "emp-state"),
    ("emp-codex", "emp-storage"),
    ("emp-codex", "emp-transport"),
    ("emp-config", "emp-domain"),
    ("emp-config", "emp-files"),
    ("emp-config", "emp-storage"),
    ("emp-context", "emp-domain"),
    ("emp-mod-aggregate", "emp-modules"),
    ("emp-mod-decision", "emp-modules"),
    ("emp-mod-gateway", "emp-modules"),
    ("emp-mod-team", "emp-modules"),
    ("emp-mod-team", "emp-state"),
    ("emp-router", "emp-domain"),
    ("emp-router", "emp-protocol"),
    ("emp-router", "emp-transport"),
    ("emp-state", "emp-config"),
    ("emp-state", "emp-domain"),
    ("emp-state", "emp-files"),
    ("emp-state", "emp-storage"),
    ("emp-storage", "emp-domain"),
    ("emp-storage", "emp-files"),
    ("emp-test-support", "emp-domain"),
    ("emp-updater", "emp-domain"),
    ("emp-updater", "emp-files"),
    ("xtask", "emp-modules"),
    ("xtask", "emp-updater"),
];

const DEV: &[(&str, &str)] = &[
    ("emp-app", "emp-mod-aggregate"),
    ("emp-app", "emp-mod-decision"),
    ("emp-app", "emp-mod-gateway"),
    ("emp-app", "emp-test-support"),
    ("emp-codex", "emp-test-support"),
    ("emp-config", "emp-test-support"),
    ("emp-files", "emp-test-support"),
    ("emp-router", "emp-config"),
    ("emp-router", "emp-storage"),
    ("emp-router", "emp-test-support"),
    ("emp-state", "emp-test-support"),
    ("emp-storage", "emp-test-support"),
    ("emp-transport", "emp-test-support"),
    ("emp-updater", "emp-test-support"),
    ("xtask", "emp-test-support"),
];

const FORBIDDEN: &[(&str, &str, &str)] = &[
    (
        "*",
        "emp-test-support",
        "共用测试夹具只能由 dev-dependencies 引用",
    ),
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
    ("emp-updater", "emp-config", "更新器只依赖领域值和文件层"),
    ("emp-updater", "emp-storage", "更新器只依赖领域值和文件层"),
    ("emp-updater", "emp-codex", "更新器只依赖领域值和文件层"),
    ("emp-files", "emp-*", "文件层是叶子"),
    ("emp-domain", "emp-*", "领域值是叶子"),
];

/// Forbidden paths that still exist until the named PLAN step removes them.
const TRANSITIONAL: &[(&str, &str, &str)] = &[];

pub(super) type TestTargets = BTreeMap<String, BTreeSet<String>>;

pub(super) fn check(
    root: &Path,
    tracked_paths: &BTreeSet<&Path>,
    errors: &mut Vec<String>,
) -> Result<(test_scope::Index, TestTargets)> {
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
    dependencies(&metadata, TRANSITIONAL, errors)?;
    let mut test_scopes = test_scope::Index::default();
    let mut test_targets = TestTargets::new();
    for package in workspace_packages(&metadata)? {
        let name = package["name"].as_str().ok_or("[arch] package 缺少 name")?;
        let package_tests = test_targets.entry(name.to_owned()).or_default();
        let directory = package["manifest_path"]
            .as_str()
            .and_then(|path| Path::new(path).parent())
            .ok_or("[arch] package 缺少 manifest_path")?;
        let targets = package["targets"]
            .as_array()
            .ok_or("[arch] package 缺少 targets")?;
        let files = targets.iter().filter_map(|target| {
            use test_scope::SourceKind;
            let file = Path::new(target["src_path"].as_str()?);
            let kind = if target["kind"].as_array()?.iter().any(|kind| kind == "test") {
                package_tests.insert(target["name"].as_str()?.to_owned());
                SourceKind::Integration
            } else if name == "emp-test-support" {
                SourceKind::Fixture
            } else {
                SourceKind::Production
            };
            Some((file, kind))
        });
        module_paths::check(
            root,
            directory,
            files,
            tracked_paths,
            &mut test_scopes,
            errors,
        )?;
    }
    Ok((test_scopes, test_targets))
}

fn workspace_packages(metadata: &Value) -> Result<impl Iterator<Item = &Value>> {
    let packages = metadata["packages"]
        .as_array()
        .ok_or("[arch] cargo metadata 缺少 packages")?;
    let members = metadata["workspace_members"]
        .as_array()
        .ok_or("[arch] cargo metadata 缺少 workspace_members")?;
    Ok(packages
        .iter()
        .filter(move |package| members.contains(&package["id"])))
}

fn dependencies(
    metadata: &Value,
    transitional: &[(&str, &str, &str)],
    errors: &mut Vec<String>,
) -> Result {
    let packages = workspace_packages(metadata)?.collect::<Vec<_>>();
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
        test_support_features(package, errors);
        for dependency in package["dependencies"]
            .as_array()
            .ok_or("[arch] package 缺少 dependencies")?
        {
            let to = dependency["name"]
                .as_str()
                .ok_or("[arch] dependency 缺少 name")?;
            let kind = &dependency["kind"];
            let dev = kind.as_str() == Some("dev");
            if from == "emp-modules"
                && kind.is_null()
                && !matches!(to, "emp-domain" | "lsp-server" | "serde" | "serde_json")
            {
                errors.push(format!(
                    "[module-sdk-deps] {}: emp-modules → {to} 不在 SDK 的普通依赖白名单中，只允许 emp-domain、lsp-server、serde 和 serde_json",
                    package["manifest_path"].as_str().unwrap_or("Cargo.toml"),
                ));
            }
            let Some(path) = dependency["path"].as_str() else {
                continue;
            };
            if names.get(to).copied() != Some(Path::new(path)) {
                continue;
            }
            if to == "emp-test-support" {
                websocket_fixture_consumer(package, from, dependency, errors);
            }
            if !dev && enables_test_support(&dependency["features"]) {
                errors.push(format!("[arch] {} [dependencies]: {from} → {to} 在正常依赖里打开了 test-support；测试钩子只能由 [dev-dependencies] 或本 crate 的 test-support 特性打开", package["manifest_path"].as_str().unwrap_or("Cargo.toml")));
            }
            let allowed = match (from, to) {
                ("emp-modules", _) => true,
                ("emp-test-support", "emp-transport") => {
                    websocket_transport(package, dependency, errors);
                    true
                }
                _ => NORMAL_BUILD.contains(&(from, to)) || dev && DEV.contains(&(from, to)),
            };
            if edges.insert((from, to, dev)) && !allowed {
                let kind = if dev { "dev" } else { "normal/build" };
                errors.push(format!("[arch] {} [dependencies, {kind}]: 新的 crate 依赖 {from} → {to} 不在允许清单里；如果确实需要，先更新 PLAN §3.2 和 xtask 清单", package["manifest_path"].as_str().unwrap_or("Cargo.toml")));
            }
        }
    }
    forbidden(&edges, transitional, errors);
    Ok(())
}

fn enables_test_support(features: &Value) -> bool {
    features.as_array().into_iter().flatten().any(|feature| {
        feature
            .as_str()
            .is_some_and(|feature| feature.rsplit('/').next() == Some(TEST_SUPPORT))
    })
}

/// The WebSocket fixtures pull the whole transport stack (reqwest, TLS,
/// compression) into a consumer's test build; only the crates that drive
/// WebSocket traffic may enable them.
const WEBSOCKET_FIXTURE_CONSUMERS: &[&str] = &["emp-app", "emp-transport"];

fn websocket_fixture_consumer(
    package: &Value,
    from: &str,
    dependency: &Value,
    errors: &mut Vec<String>,
) {
    if WEBSOCKET_FIXTURE_CONSUMERS.contains(&from) {
        return;
    }
    let name = dependency["rename"].as_str().unwrap_or("emp-test-support");
    let direct = dependency["features"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|feature| feature == "websocket");
    let forwarded = package["features"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(_, features)| features.as_array())
        .flatten()
        .filter_map(Value::as_str)
        .any(|feature| {
            feature.split_once('/').is_some_and(|(dep, enabled)| {
                dep.trim_end_matches('?') == name && enabled == "websocket"
            })
        });
    if direct || forwarded {
        errors.push(format!(
            "[arch] {}: {from} 打开了 emp-test-support 的 websocket 特性；只有 {} 可以使用 WebSocket 夹具",
            package["manifest_path"].as_str().unwrap_or("Cargo.toml"),
            WEBSOCKET_FIXTURE_CONSUMERS.join("、"),
        ));
    }
}

fn websocket_transport(package: &Value, dependency: &Value, errors: &mut Vec<String>) {
    let name = dependency["rename"].as_str().unwrap_or("emp-transport");
    let dep_feature = format!("dep:{name}");
    let enabled_by_websocket =
        package["features"]["websocket"]
            .as_array()
            .is_some_and(|features| {
                features
                    .iter()
                    .any(|feature| feature == dep_feature.as_str())
            });
    let enabled_elsewhere = package["features"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, _)| name.as_str() != "websocket")
        .filter_map(|(_, features)| features.as_array())
        .flatten()
        .filter_map(Value::as_str)
        .any(|feature| {
            feature == "websocket"
                || feature == dep_feature
                || feature.split_once('/').is_some_and(|(dep, _)| dep == name)
        });
    if !dependency["kind"].is_null()
        || dependency["optional"].as_bool() != Some(true)
        || !enabled_by_websocket
        || enabled_elsewhere
    {
        errors.push(format!(
            "[arch] {}: emp-test-support → emp-transport 必须是仅由 websocket 特性启用的可选正常依赖",
            package["manifest_path"].as_str().unwrap_or("Cargo.toml")
        ));
    }
}

const TEST_SUPPORT: &str = "test-support";

fn test_support_features(package: &Value, errors: &mut Vec<String>) {
    let Some(features) = package["features"].as_object() else {
        return;
    };
    for (name, enabled) in features {
        if name != TEST_SUPPORT && enables_test_support(enabled) {
            errors.push(format!("[arch] {} [features] {name}: 只有 test-support 特性能打开依赖的 test-support，否则测试钩子会进入生产构建", package["manifest_path"].as_str().unwrap_or("Cargo.toml")));
        }
    }
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
    fn module_sdk_limits_normal_dependencies_including_external_crates() {
        for (to, path, kind, valid) in [
            (
                "emp-domain",
                json!("/workspace/emp-domain"),
                Value::Null,
                true,
            ),
            ("serde", Value::Null, Value::Null, true),
            ("serde_json", Value::Null, Value::Null, true),
            ("lsp-server", Value::Null, Value::Null, true),
            ("tokio", Value::Null, Value::Null, false),
            ("tokio", Value::Null, json!("build"), true),
            ("tokio", Value::Null, json!("dev"), true),
            (
                "emp-config",
                json!("/workspace/emp-config"),
                Value::Null,
                false,
            ),
            (
                "emp-config",
                json!("/workspace/emp-config"),
                json!("dev"),
                true,
            ),
        ] {
            let metadata = json!({"workspace_members": ["emp-modules", "emp-domain", "emp-config"], "packages": [
                {"id": "emp-modules", "name": "emp-modules", "manifest_path": "/workspace/emp-modules/Cargo.toml",
                 "dependencies": [{"name": to, "path": path, "kind": kind}]},
                {"id": "emp-domain", "name": "emp-domain", "manifest_path": "/workspace/emp-domain/Cargo.toml", "dependencies": []},
                {"id": "emp-config", "name": "emp-config", "manifest_path": "/workspace/emp-config/Cargo.toml", "dependencies": []}
            ]});
            let mut errors = Vec::new();
            dependencies(&metadata, &[], &mut errors).unwrap();
            assert_eq!(errors.is_empty(), valid, "emp-modules → {to}: {errors:?}");
            if !valid {
                assert!(errors[0].starts_with("[module-sdk-deps]"));
                assert!(errors[0].contains(&format!("emp-modules → {to}")));
            }
        }
    }

    #[test]
    fn arch_accepts_snapshot_edges_and_dev_edges_parallel_to_normal_ones() {
        for (from, to, kind, valid) in [
            ("emp-app", "emp-domain", Value::Null, true),
            ("emp-app", "emp-domain", json!("build"), true),
            ("emp-app", "emp-mod-decision", json!("dev"), true),
            ("emp-domain", "emp-app", Value::Null, false),
            ("emp-app", "emp-mod-decision", Value::Null, false),
            ("emp-app", "emp-state", json!("dev"), true),
            ("emp-domain", "emp-app", json!("dev"), false),
            ("emp-app", "emp-test-support", json!("dev"), true),
            ("emp-updater", "emp-test-support", json!("dev"), true),
            ("emp-app", "emp-test-support", Value::Null, false),
            ("emp-updater", "emp-test-support", json!("build"), false),
        ] {
            let metadata = json!({"workspace_members": [from, to], "packages": [
                {"id": from, "name": from, "manifest_path": format!("/workspace/{from}/Cargo.toml"),
                 "dependencies": [{"name": to, "path": format!("/workspace/{to}"), "kind": kind}]},
                {"id": to, "name": to, "manifest_path": format!("/workspace/{to}/Cargo.toml"), "dependencies": []}
            ]});
            let mut errors = Vec::new();
            dependencies(&metadata, &[], &mut errors).unwrap();
            assert_eq!(errors.is_empty(), valid, "{from} → {to}: {errors:?}");
            if !valid {
                assert!(errors[0].contains(&format!("{from} → {to}")));
                assert!(errors[0].contains("PLAN §3.2"));
                if to == "emp-test-support" {
                    assert!(
                        errors
                            .iter()
                            .any(|error| error.contains("只能由 dev-dependencies 引用"))
                    );
                }
            }
        }
    }

    #[test]
    fn test_support_is_reachable_only_from_dev_dependencies_and_its_own_feature() {
        let check = |kind: Value, features: Value| {
            let metadata = json!({"workspace_members": ["emp-app", "emp-state"], "packages": [
                {"id": "emp-app", "name": "emp-app", "manifest_path": "/workspace/emp-app/Cargo.toml",
                 "dependencies": [{"name": "emp-state", "path": "/workspace/emp-state", "kind": kind,
                                   "features": ["test-support"]}]},
                {"id": "emp-state", "name": "emp-state", "manifest_path": "/workspace/emp-state/Cargo.toml",
                 "dependencies": [], "features": features}
            ]});
            let mut errors = Vec::new();
            dependencies(&metadata, &[], &mut errors).unwrap();
            errors
        };
        let own = json!({"test-support": ["emp-files/test-support"]});
        assert!(check(json!("dev"), own.clone()).is_empty());
        let errors = check(Value::Null, own);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("emp-app → emp-state 在正常依赖里打开了 test-support"));
        let errors = check(json!("dev"), json!({"default": ["emp-files/test-support"]}));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("[features] default"));
    }

    #[test]
    fn websocket_fixtures_are_enabled_only_by_their_assigned_consumers() {
        let check = |from: &str, dependency_features: Value, features: Value| {
            let metadata = json!({"workspace_members": [from, "emp-test-support"], "packages": [
                {"id": from, "name": from, "manifest_path": format!("/workspace/{from}/Cargo.toml"),
                 "dependencies": [{"name": "emp-test-support", "path": "/workspace/emp-test-support",
                                   "kind": "dev", "features": dependency_features}],
                 "features": features},
                {"id": "emp-test-support", "name": "emp-test-support",
                 "manifest_path": "/workspace/emp-test-support/Cargo.toml", "dependencies": []}
            ]});
            let mut errors = Vec::new();
            dependencies(&metadata, &[], &mut errors).unwrap();
            errors
        };
        assert!(check("emp-app", json!(["websocket"]), json!({})).is_empty());
        assert!(check("emp-files", json!([]), json!({})).is_empty());
        for (dependency_features, features) in [
            (json!(["websocket"]), json!({})),
            (json!([]), json!({"extra": ["emp-test-support/websocket"]})),
            (json!([]), json!({"extra": ["emp-test-support?/websocket"]})),
        ] {
            let errors = check("emp-files", dependency_features, features);
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert!(errors[0].contains("emp-files 打开了 emp-test-support 的 websocket 特性"));
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
