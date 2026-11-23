use super::*;

const ALLOWED: &[(&str, usize)] = &[
    ("docs/architecture.md", 16),
    ("docs/backend-workflows.zh-CN.md", 4),
    ("docs/claude-switch-failures.md", 4),
    ("docs/codex-compatibility.md", 3),
    ("docs/development-sop.md", 1),
    ("docs/diagnostic-journal-spec.md", 1),
    ("docs/external-collaboration.md", 1),
    ("docs/gateway.md", 1),
    ("docs/official-model-registry-spec.md", 1),
    ("docs/performance-profiling.md", 2),
    ("docs/request-limits.md", 1),
    ("docs/self-update.md", 1),
    ("docs/sidechat-history.md", 1),
    ("docs/usage-accounting.md", 1),
];

pub(super) fn includes(path: &str) -> bool {
    matches!(path, "README.md" | "README.zh-CN.md")
        || (path.starts_with("docs/")
            && path.ends_with(".md")
            && path != "docs/archive.md"
            && !Path::new(path)
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("CHANGELOG")))
}

pub(super) fn check(path: &str, contents: &str, errors: &mut Vec<String>) {
    let count = contents
        .lines()
        .filter(|line| {
            let lower = line.to_lowercase();
            [
                "no longer",
                "previously",
                "used to",
                "已取消",
                "已移除",
                "已删除",
                "不再",
            ]
            .iter()
            .any(|word| lower.contains(*word))
        })
        .count();
    let allowed = ALLOWED
        .iter()
        .find(|(file, _)| *file == path)
        .map_or(0, |(_, count)| *count);
    if count > allowed {
        errors.push(format!("[docs-prose] {path}: 当前命中 {count} 行，超过允许的 {allowed} 行；文档描述现状，改写为当前行为，变更历史写进 CHANGELOG；计数棘轮只降不升"));
    } else if count < allowed {
        println!(
            "[docs-prose] {path}: 当前命中 {count} 行，低于允许的 {allowed} 行；请将 xtask/src/check/prose.rs 中的计数调低至 {count}（棘轮只降不升）"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docs_prose_count_ratchet() {
        for (path, contents, valid) in [
            ("README.md", "EMP serves HTTP requests.", true),
            ("docs/request-limits.md", "Previously cached.", true),
            ("docs/request-limits.md", "Current behavior.", true),
            ("docs/current.md", "不再缓存。", false),
            (
                "docs/request-limits.md",
                "Previously cached.\nPreviously cached.",
                false,
            ),
        ] {
            let mut errors = Vec::new();
            check(path, contents, &mut errors);
            assert_eq!(errors.is_empty(), valid, "{path}: {errors:?}");
        }
        assert!(!includes("docs/archive.md"));
        assert!(!includes("docs/nested/CHANGELOG.md"));
        assert!(includes("docs/nested/current.md"));
    }
}
