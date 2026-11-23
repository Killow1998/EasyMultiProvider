use super::*;
use std::collections::BTreeMap;

const ALLOWED: &[(&str, usize)] = &[
    ("emp-app", 13),
    ("emp-codex", 3),
    ("emp-core", 0),
    ("emp-history", 0),
    ("emp-integration", 0),
    ("emp-modules", 0),
    ("emp-protocol", 0),
    ("emp-router", 1),
    ("emp-state", 0),
    ("emp-transport", 1),
    ("xtask", 0),
    ("web", 1),
];

pub(super) fn check(root: &Path, paths: &[String], errors: &mut Vec<String>) -> Result {
    let mut counts: BTreeMap<_, usize> = ALLOWED.iter().map(|&(name, _)| (name, 0)).collect();
    for path in paths
        .iter()
        .filter(|path| path.ends_with(".rs") && *path != "xtask/src/check/comments.rs")
    {
        if root.join(path).is_file() {
            disguised_sleep(path, &read(root, path)?, errors);
        }
    }
    for path in paths {
        if let Some(group) = group(path)
            && root.join(path).is_file()
        {
            *counts.entry(group).or_default() += count(&read(root, path)?, path.ends_with(".rs"));
        }
    }
    for (group, count) in counts {
        let allowed = ALLOWED
            .iter()
            .find(|(name, _)| *name == group)
            .map_or(0, |(_, count)| *count);
        if count > allowed {
            errors.push(format!("[comments] {group}: 当前普通注释 {count} 行，超过允许的 {allowed} 行；把约束写成命名、类型、测试或 docs/，或在 PR 说明里解释；计数棘轮只降不升"));
        } else if count < allowed {
            println!(
                "[comments] {group}: 当前普通注释 {count} 行，低于允许的 {allowed} 行；请将 xtask/src/check/comments.rs 中的计数调低至 {count}（棘轮只降不升）"
            );
        }
    }
    Ok(())
}

fn group(path: &str) -> Option<&str> {
    let parts = path.split('/').collect::<Vec<_>>();
    let name = *parts.last()?;
    if parts.contains(&"tests") || name == "tests.rs" || name.ends_with("_tests.rs") {
        return None;
    }
    match parts.as_slice() {
        ["crates", "emp-app", "web", name] if name.ends_with(".js") => Some("web"),
        ["crates", name, "src", ..] if path.ends_with(".rs") && !name.starts_with("emp-mod-") => {
            Some(*name)
        }
        ["xtask", "src", ..] if path.ends_with(".rs") => Some("xtask"),
        _ => None,
    }
}

fn count(contents: &str, rust: bool) -> usize {
    let mut count = 0;
    let mut test_indent = None;
    for line in contents.lines() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if let Some(start) = test_indent {
            if indent == start
                && (trimmed.starts_with('}')
                    || (!trimmed.starts_with('#') && trimmed.trim_end().ends_with(';')))
            {
                test_indent = None;
            }
            continue;
        }
        if rust && trimmed.starts_with("#[cfg(") {
            let attribute = trimmed.split_whitespace().collect::<String>();
            if attribute == "#[cfg(test)]" || attribute.starts_with("#[cfg(all(test,") {
                test_indent = Some(indent);
                continue;
            }
        }
        if trimmed.starts_with("//")
            && !["///", "//!", "// SAFETY:"]
                .iter()
                .any(|prefix| trimmed.starts_with(prefix))
        {
            count += 1;
        }
    }
    count
}

fn disguised_sleep(path: &str, contents: &str, errors: &mut Vec<String>) {
    for (index, line) in contents.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.contains("let (_") && trimmed.contains("channel") {
            errors.push(format!("[disguised-sleep] {path}:{}: 丢弃 sender 的 channel 只能当 sleep 用；轮询等待用 tests/support/modules.rs 的 eventually，固定间隔用 std::thread::sleep 并加 #[expect(clippy::disallowed_methods, reason = \"…\")]", index + 1));
        }
    }
}
