use super::*;
use std::collections::BTreeMap;
use syn::ext::IdentExt;
use syn::spanned::Spanned;
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
    for path in paths {
        let rust = path.ends_with(".rs");
        let group = group(path);
        if (!rust && group.is_none()) || !root.join(path).is_file() {
            continue;
        }
        let contents = read(root, path)?;
        let mut test_scopes = TestScopes::default();
        let inspect_sleep =
            rust && path != "xtask/src/check/comments.rs" && contents.contains("recv_timeout");
        if rust
            && (inspect_sleep
                || (group.is_some() && contents.contains("cfg") && contents.contains("test")))
        {
            let source = syn::parse_file(&contents).map_err(|error| format!("{path}: {error}"))?;
            test_scopes.visit_file(&source);
            if inspect_sleep {
                DisguisedSleep {
                    path,
                    test_scopes: &test_scopes,
                    errors,
                }
                .visit_file(&source);
            }
        }
        if rust && path != "xtask/src/check/comments.rs" {
            msvc_only(path, &contents, errors);
            domain_purity(path, &contents, errors)?;
        }
        if let Some(group) = group {
            *counts.entry(group).or_default() += count(&contents, &test_scopes);
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

pub(super) fn test_file(path: &str) -> bool {
    let mut parts = path.rsplit('/');
    let name = parts.next().expect("source path");
    name == "tests.rs" || name.ends_with("_tests.rs") || parts.any(|part| part == "tests")
}

fn group(path: &str) -> Option<&str> {
    if test_file(path) {
        return None;
    }
    let parts = path.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        ["crates", "emp-app", "web", name] if name.ends_with(".js") => Some("web"),
        ["crates", name, "src", ..] if path.ends_with(".rs") && !name.starts_with("emp-mod-") => {
            Some(*name)
        }
        ["xtask", "src", ..] if path.ends_with(".rs") => Some("xtask"),
        _ => None,
    }
}

fn count(contents: &str, test_scopes: &TestScopes) -> usize {
    let mut count = 0;
    let mut safety = false;
    for (index, line) in contents.lines().enumerate() {
        if test_scopes.contains(index + 1) {
            safety = false;
            continue;
        }
        let trimmed = line.trim_start();
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

#[derive(Default)]
pub(super) struct TestScopes(Vec<std::ops::RangeInclusive<usize>>);

impl TestScopes {
    pub(super) fn contains(&self, line: usize) -> bool {
        self.0.iter().any(|range| range.contains(&line))
    }

    fn add(&mut self, item: &impl Spanned, attrs: &[syn::Attribute]) -> bool {
        let test_only = attrs.iter().any(test_only_attribute);
        if test_only {
            let span = item.span();
            self.0.push(span.start().line..=span.end().line);
        }
        test_only
    }
}

pub(super) fn test_only_attribute(attr: &syn::Attribute) -> bool {
    let path = attr.path();
    path.is_ident("test")
        || (path.is_ident("cfg")
            && attr
                .parse_args::<syn::Meta>()
                .is_ok_and(|meta| requires_test(&meta)))
        || path
            .segments
            .iter()
            .map(|segment| &segment.ident)
            .eq(["tokio", "test"])
}

pub(super) fn requires_test(meta: &syn::Meta) -> bool {
    if matches!(meta, syn::Meta::Path(path) if path.is_ident("test")) {
        return true;
    }
    let syn::Meta::List(list) = meta else {
        return false;
    };
    let Ok(parts) = list.parse_args_with(
        syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
    ) else {
        return false;
    };
    if list.path.is_ident("all") {
        parts.iter().any(requires_test)
    } else {
        list.path.is_ident("any") && !parts.is_empty() && parts.iter().all(requires_test)
    }
}

macro_rules! visit_test_items {
    ($method:ident, $kind:ident; $($variant:ident),+) => {
        fn $method(&mut self, item: &'ast syn::$kind) {
            let attrs = match item {
                $(syn::$kind::$variant(item) => &item.attrs,)+
                _ => return,
            };
            if !self.add(item, attrs) {
                visit::$method(self, item);
            }
        }
    };
}

impl<'ast> Visit<'ast> for TestScopes {
    fn visit_file(&mut self, file: &'ast syn::File) {
        if !self.add(file, &file.attrs) {
            visit::visit_file(self, file);
        }
    }

    visit_test_items!(visit_item, Item; Const, Enum, ExternCrate, Fn, ForeignMod, Impl,
        Macro, Mod, Static, Struct, Trait, TraitAlias, Type, Union, Use);
    visit_test_items!(visit_impl_item, ImplItem; Const, Fn, Macro, Type);
    visit_test_items!(visit_trait_item, TraitItem; Const, Fn, Macro, Type);
    visit_test_items!(visit_foreign_item, ForeignItem; Fn, Macro, Static, Type);
}

struct DisguisedSleep<'a> {
    path: &'a str,
    test_scopes: &'a TestScopes,
    errors: &'a mut Vec<String>,
}

impl<'ast> Visit<'ast> for DisguisedSleep<'_> {
    fn visit_block(&mut self, block: &'ast syn::Block) {
        for (index, statement) in block.stmts.iter().enumerate() {
            let syn::Stmt::Local(local) = statement else {
                continue;
            };
            let line = local.span().start().line;
            if !test_file(self.path) && !self.test_scopes.contains(line) {
                continue;
            }
            let Some((sender, receiver)) = channel_bindings(local) else {
                continue;
            };
            let mut uses = ChannelUses {
                sender,
                receiver,
                sender_used: false,
                waits: false,
            };
            for statement in &block.stmts[index + 1..] {
                uses.visit_stmt(statement);
            }
            if uses.waits && !uses.sender_used {
                let path = self.path;
                self.errors.push(format!(r#"[disguised-sleep] {path}:{line}: 无人发送的 channel 不能用 recv_timeout 充当 sleep；轮询等待用 tests/support/modules.rs 的 eventually，固定间隔用 std::thread::sleep 并加 #[expect(clippy::disallowed_methods, reason = "…")]"#));
            }
        }
        visit::visit_block(self, block);
    }
}

fn channel_bindings(local: &syn::Local) -> Option<(Option<&syn::Ident>, &syn::Ident)> {
    let pattern = match &local.pat {
        syn::Pat::Type(typed) => &*typed.pat,
        pattern => pattern,
    };
    let syn::Pat::Tuple(tuple) = pattern else {
        return None;
    };
    let mut bindings = tuple.elems.iter();
    let sender = match bindings.next()? {
        syn::Pat::Ident(binding) => Some(&binding.ident),
        syn::Pat::Wild(_) => None,
        _ => return None,
    };
    let syn::Pat::Ident(receiver) = bindings.next()? else {
        return None;
    };
    let syn::Expr::Call(call) = &*local.init.as_ref()?.expr else {
        return None;
    };
    let name = called_name(&call.func)?;
    matches!(name.to_string().as_str(), "channel" | "sync_channel")
        .then_some((sender, &receiver.ident))
}

fn called_name(expr: &syn::Expr) -> Option<&syn::Ident> {
    let syn::Expr::Path(path) = expr else {
        return None;
    };
    path.path.segments.last().map(|segment| &segment.ident)
}

struct ChannelUses<'a> {
    sender: Option<&'a syn::Ident>,
    receiver: &'a syn::Ident,
    sender_used: bool,
    waits: bool,
}

impl<'ast> Visit<'ast> for ChannelUses<'_> {
    fn visit_expr_path(&mut self, expr: &'ast syn::ExprPath) {
        self.sender_used |= expr
            .path
            .get_ident()
            .is_some_and(|name| Some(name) == self.sender);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        let drops_sender = called_name(&call.func).is_some_and(|name| name == "drop")
            && call.args.len() == 1
            && call
                .args
                .first()
                .and_then(called_name)
                .is_some_and(|name| Some(name) == self.sender);
        if !drops_sender {
            visit::visit_expr_call(self, call);
        }
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.waits |=
            call.method == "recv_timeout" && called_name(&call.receiver) == Some(self.receiver);
        visit::visit_expr_method_call(self, call);
    }

    fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
        use syn::parse::Parser as _;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiline_test_static_does_not_hide_the_next_production_function() {
        let source = "#[cfg(test)]
static FIXTURE: u8 =
    // fixture setup
    1;
fn production() {
    // production note
}
";
        let mut test_scopes = TestScopes::default();
        test_scopes.visit_file(&syn::parse_file(source).unwrap());
        assert_eq!(count(source, &test_scopes), 1);
    }

    #[test]
    fn disguised_sleep_allows_an_underscore_prefixed_sender_that_sends() {
        let source = r"
#[test]
fn example() {
    let (_tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || _tx.send(()).unwrap());
    rx.recv_timeout(limit).unwrap();
}";
        let source = syn::parse_file(source).unwrap();
        let mut test_scopes = TestScopes::default();
        test_scopes.visit_file(&source);
        let mut errors = Vec::new();
        DisguisedSleep {
            path: "crates/example/src/lib.rs",
            test_scopes: &test_scopes,
            errors: &mut errors,
        }
        .visit_file(&source);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn disguised_sleep_detects_a_named_sender_held_only_to_keep_the_channel_open() {
        let source = r"
fn example() {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    assert!(rx.recv_timeout(limit).is_err());
    drop(tx);
}";
        let source = syn::parse_file(source).unwrap();
        let test_scopes = TestScopes::default();
        let mut errors = Vec::new();
        DisguisedSleep {
            path: "crates/example/src/lib.rs",
            test_scopes: &test_scopes,
            errors: &mut errors,
        }
        .visit_file(&source);
        assert!(errors.is_empty(), "{errors:?}");
        DisguisedSleep {
            path: "crates/example/tests/journey.rs",
            test_scopes: &test_scopes,
            errors: &mut errors,
        }
        .visit_file(&source);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("[disguised-sleep] crates/example/tests/journey.rs:3:"));
    }

    #[test]
    fn safety_continuation_lines_are_part_of_the_safety_note() {
        let source = "    // SAFETY: the handle is valid\n    // for the whole call.\n    unsafe { call() };\n    // stray note\n";
        assert_eq!(count(source, &TestScopes::default()), 1);
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
