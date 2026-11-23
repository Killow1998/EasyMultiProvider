use super::*;
use std::collections::BTreeMap;
use syn::ext::IdentExt;
use syn::visit::{self, Visit};

const ALLOWED: &[(&str, usize)] = &[
    ("emp-app", 4),
    ("emp-codex", 0),
    ("emp-config", 0),
    ("emp-domain", 0),
    ("emp-context", 0),
    ("emp-modules", 0),
    ("emp-protocol", 0),
    ("emp-router", 0),
    ("emp-state", 0),
    ("emp-storage", 0),
    ("emp-transport", 0),
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
            let contents = read(root, path)?;
            disguised_sleep(path, &contents, errors);
            msvc_only(path, &contents, errors);
            domain_purity(path, &contents, errors)?;
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
    let mut safety = false;
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
        let comment = trimmed.starts_with("//");
        safety = if trimmed.starts_with("// SAFETY:") {
            true
        } else {
            safety && comment
        };
        if comment
            && !safety
            && !["///", "//!"]
                .iter()
                .any(|prefix| trimmed.starts_with(prefix))
        {
            count += 1;
        }
    }
    count
}

fn msvc_only(path: &str, contents: &str, errors: &mut Vec<String>) {
    for (index, line) in contents.lines().enumerate() {
        if line
            .split_whitespace()
            .collect::<String>()
            .contains("target_env=\"msvc\"")
        {
            errors.push(format!("[msvc-only] {path}:{}: cargo xtask cross-check 用 windows-gnu 检查 Windows 代码，msvc 专属分支不会被本机检查覆盖；改用 cfg(windows)，或先扩展 cross-check", index + 1));
        }
    }
}

fn domain_purity(path: &str, contents: &str, errors: &mut Vec<String>) -> Result {
    if !path.starts_with("crates/emp-domain/src/") {
        return Ok(());
    }
    let source = syn::parse_file(contents).map_err(|error| {
        format!(
            "[domain-purity] {path}:{}: {error}",
            error.span().start().line
        )
    })?;
    DomainPurity { path, errors }.visit_file(&source);
    Ok(())
}

struct DomainPurity<'a> {
    path: &'a str,
    errors: &'a mut Vec<String>,
}

impl DomainPurity<'_> {
    fn check_path(&mut self, root: &syn::Ident, member: Option<&syn::Ident>) {
        let api = match root.unraw().to_string().as_str() {
            "std" => match member.map(|name| name.unraw().to_string()).as_deref() {
                Some("fs") => "std::fs",
                Some("env") => "std::env",
                Some("net") => "std::net",
                Some("process") => "std::process",
                _ => return,
            },
            "rusqlite" => "rusqlite",
            "tokio" => "tokio",
            _ => return,
        };
        let line = member.unwrap_or(root).span().start().line;
        let path = self.path;
        self.errors.push(format!("[domain-purity] {path}:{line}: emp-domain 只放值和纯规则，不能使用 {api}；把读写放进 emp-files、emp-storage 或 emp-config，由调用方传入结果"));
    }

    fn check_use(&mut self, tree: &syn::UseTree, root: Option<&syn::Ident>) {
        match tree {
            syn::UseTree::Path(path) => {
                self.check_import(&path.ident, root);
                if root.is_none() && path.ident.unraw() == "std" {
                    self.check_use(&path.tree, Some(&path.ident));
                }
            }
            syn::UseTree::Name(name) => self.check_import(&name.ident, root),
            syn::UseTree::Rename(rename) => self.check_import(&rename.ident, root),
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    self.check_use(item, root);
                }
            }
            syn::UseTree::Glob(_) => {}
        }
    }

    fn check_import(&mut self, name: &syn::Ident, root: Option<&syn::Ident>) {
        match root {
            Some(root) => self.check_path(root, Some(name)),
            None => self.check_path(name, None),
        }
    }
}

impl<'ast> Visit<'ast> for DomainPurity<'_> {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.check_use(&item.tree, None);
        visit::visit_item_use(self, item);
    }

    fn visit_item_extern_crate(&mut self, item: &'ast syn::ItemExternCrate) {
        self.check_path(&item.ident, None);
        visit::visit_item_extern_crate(self, item);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let mut segments = path.segments.iter();
        if let Some(root) = segments.next() {
            self.check_path(&root.ident, segments.next().map(|segment| &segment.ident));
        }
        visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
        use syn::parse::Parser as _;

        visit::visit_macro(self, invocation);
        if let Ok(expressions) =
            syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
                .parse2(invocation.tokens.clone())
        {
            for expression in &expressions {
                self.visit_expr(expression);
            }
        }
    }
}

fn disguised_sleep(path: &str, contents: &str, errors: &mut Vec<String>) {
    for (index, line) in contents.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.contains("let (_") && trimmed.contains("channel") {
            errors.push(format!("[disguised-sleep] {path}:{}: 丢弃 sender 的 channel 只能当 sleep 用；轮询等待用 tests/support/modules.rs 的 eventually，固定间隔用 std::thread::sleep 并加 #[expect(clippy::disallowed_methods, reason = \"…\")]", index + 1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{count, domain_purity, msvc_only};

    #[test]
    fn safety_continuation_lines_are_part_of_the_safety_note() {
        let source = "    // SAFETY: the handle is valid\n    // for the whole call.\n    unsafe { call() };\n    // stray note\n";
        assert_eq!(count(source, true), 1);
    }

    #[test]
    fn msvc_only_branches_are_rejected_because_cross_check_uses_windows_gnu() {
        let mut errors = Vec::new();
        msvc_only(
            "a.rs",
            "#[cfg(windows)]\n#[cfg(all(windows, target_env = \"msvc\"))]\n",
            &mut errors,
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("[msvc-only] a.rs:2:"));
    }

    #[test]
    fn domain_sources_cannot_reach_files_environment_or_storage() {
        let mut errors = Vec::new();
        domain_purity(
            "crates/emp-domain/src/a.rs",
            "use std::fs;\nfn f() {}\n",
            &mut errors,
        )
        .unwrap();
        domain_purity("crates/emp-storage/src/a.rs", "use std::fs;\n", &mut errors).unwrap();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("[domain-purity] crates/emp-domain/src/a.rs:1:"));
    }

    #[test]
    fn domain_sources_reject_restricted_paths_grouped_imports_and_aliases() {
        let source = "
use ::std::{
    fs as files,
    env,
    net::{TcpStream as Stream},
    fmt,
};
use {rusqlite as sql, tokio::time as timer};
fn f() { ::std :: process :: abort(); }
fn g() { dbg!(std::fs::read(\"file\")); }
";
        let mut errors = Vec::new();
        domain_purity("crates/emp-domain/src/a.rs", source, &mut errors).unwrap();
        let expected = [
            (3, "std::fs"),
            (4, "std::env"),
            (5, "std::net"),
            (8, "rusqlite"),
            (8, "tokio"),
            (9, "std::process"),
            (10, "std::fs"),
        ];
        assert_eq!(errors.len(), expected.len(), "{errors:?}");
        for (line, api) in expected {
            assert!(
                errors.iter().any(|error| {
                    error.contains(&format!("a.rs:{line}:")) && error.contains(api)
                }),
                "{errors:?}"
            );
        }
    }

    #[test]
    fn domain_sources_ignore_comments_literals_and_unrelated_paths() {
        let source = r###"
//! use std::{fs, env};
/* Outer comment /* nested comment */ std::net */
use std::{fmt, collections::BTreeMap};
use other::std::net;
use other::tokio;
const NORMAL: &str = "std::fs";
const RAW: &str = r#"use std::{fs, env}; rusqlite"#;
fn f() { println!("std::process"); }
"###;
        let mut errors = Vec::new();
        domain_purity("crates/emp-domain/src/a.rs", source, &mut errors).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
    }
}
