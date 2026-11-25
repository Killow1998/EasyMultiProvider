use super::{Path, Result, comments, read};
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use std::collections::BTreeMap;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{AttrStyle, Attribute, Meta, Token};

const BASELINE: &[(&str, usize)] = &[
    ("emp-app", 70),
    ("emp-codex", 15),
    ("emp-config", 3),
    ("emp-context", 1),
    ("emp-protocol", 2),
    ("emp-router", 1),
    ("emp-storage", 3),
    ("emp-updater", 1),
];

pub(super) fn check(root: &Path, paths: &[String], errors: &mut Vec<String>) -> Result {
    let mut counts: BTreeMap<_, usize> = BASELINE.iter().map(|&(group, _)| (group, 0)).collect();
    for path in paths {
        if !path.ends_with(".rs") || comments::test_file(path) || !root.join(path).is_file() {
            continue;
        }
        let mut parts = path.split('/');
        let top = parts.next().expect("source path");
        let group = if top == "crates" {
            parts.next().expect("crate name")
        } else {
            top
        };
        let cfg = inspect(&read(root, path)?).map_err(|error| format!("{path}: {error}"))?;
        *counts.entry(group).or_default() += cfg.count;
        for line in cfg.non_item_lines {
            errors.push(format!(
                "[cfg-test] {path}:{line}: 生产代码中涉及 test 的条件编译只能用于条目；请移到模块边界，或把探针移进钩子方法内部"
            ));
        }
    }
    for (group, actual) in counts {
        let baseline = BASELINE
            .iter()
            .find_map(|&(name, count)| (name == group).then_some(count))
            .unwrap_or_default();
        errors.extend(mismatch(group, actual, baseline));
    }
    Ok(())
}

fn mismatch(group: &str, actual: usize, baseline: usize) -> Option<String> {
    (actual != baseline).then(|| format!(
        "[cfg-test] {group}: 非测试文件的测试专用 cfg 共 {actual} 项，基线为 {baseline}；新增探针应收拢到现有测试模块，减少后须同步调低基线"
    ))
}

#[derive(Default)]
struct Findings {
    count: usize,
    non_item_lines: Vec<usize>,
}

fn inspect(source: &str) -> syn::Result<Findings> {
    if !source.contains("cfg") || !source.contains("test") {
        return Ok(Findings::default());
    }
    let file = syn::parse_file(source)?;
    let mut cfg = TestCfg::default();
    cfg.visit_file(&file);
    Ok(cfg.findings)
}

#[derive(Default)]
struct TestCfg<'ast> {
    findings: Findings,
    item_attributes: &'ast [Attribute],
    allows_inner_attributes: bool,
    test_scope: bool,
}

impl<'ast> TestCfg<'ast> {
    fn inspect_macro_tokens(&mut self, tokens: TokenStream) {
        let mut tokens = tokens.into_iter().peekable();
        while let Some(token) = tokens.next() {
            match token {
                TokenTree::Punct(punct) if punct.as_char() == '#' => {
                    if matches!(tokens.peek(), Some(TokenTree::Punct(punct)) if punct.as_char() == '!')
                    {
                        tokens.next();
                    }
                    if let Some(TokenTree::Group(group)) =
                        tokens.next_if(|token| matches!(token, TokenTree::Group(group) if group.delimiter() == Delimiter::Bracket))
                        && let Ok(meta) = syn::parse2::<Meta>(group.stream())
                        && condition_mentions_test(&meta)
                    {
                        self.findings.count += 1;
                        if !self.test_scope {
                            self.findings.non_item_lines.push(punct.span().start().line);
                        }
                    }
                }
                TokenTree::Group(group) => self.inspect_macro_tokens(group.stream()),
                _ => {}
            }
        }
    }

    fn visit_declaration<T>(
        &mut self,
        item: &'ast T,
        attributes: &'ast [Attribute],
        allows_inner_attributes: bool,
        visit: impl FnOnce(&mut Self, &'ast T),
    ) {
        let previous_attributes = std::mem::replace(&mut self.item_attributes, attributes);
        let previous_inner =
            std::mem::replace(&mut self.allows_inner_attributes, allows_inner_attributes);
        let previous_scope = self.test_scope;
        self.test_scope |= attributes.iter().any(|attr| {
            (allows_inner_attributes || matches!(attr.style, AttrStyle::Outer))
                && comments::test_only_attribute(attr)
        });
        visit(self, item);
        self.test_scope = previous_scope;
        self.allows_inner_attributes = previous_inner;
        self.item_attributes = previous_attributes;
    }
}

macro_rules! visit_declarations {
    ($method:ident, $kind:ident; $($variant:ident),+) => {
        fn $method(&mut self, item: &'ast syn::$kind) {
            let attrs = match item {
                $(syn::$kind::$variant(item) => &item.attrs,)+
                _ => return,
            };
            self.visit_declaration(item, attrs, false, visit::$method);
        }
    };
}

impl<'ast> Visit<'ast> for TestCfg<'ast> {
    fn visit_file(&mut self, file: &'ast syn::File) {
        self.visit_declaration(file, &file.attrs, true, visit::visit_file);
    }

    fn visit_item(&mut self, item: &'ast syn::Item) {
        use syn::Item::*;
        let attrs = match item {
            Const(item) => &item.attrs,
            Enum(item) => &item.attrs,
            ExternCrate(item) => &item.attrs,
            Fn(item) => &item.attrs,
            ForeignMod(item) => &item.attrs,
            Impl(item) => &item.attrs,
            Macro(item) => &item.attrs,
            Mod(module) => {
                self.visit_declaration(module, &module.attrs, true, Self::visit_item_mod);
                return;
            }
            Static(item) => &item.attrs,
            Struct(item) => &item.attrs,
            Trait(item) => &item.attrs,
            TraitAlias(item) => &item.attrs,
            Type(item) => &item.attrs,
            Union(item) => &item.attrs,
            Use(item) => &item.attrs,
            _ => return,
        };
        self.visit_declaration(item, attrs, false, visit::visit_item);
    }

    visit_declarations!(visit_impl_item, ImplItem; Const, Fn, Macro, Type);
    visit_declarations!(visit_trait_item, TraitItem; Const, Fn, Macro, Type);
    visit_declarations!(visit_foreign_item, ForeignItem; Fn, Macro, Static, Type);

    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        if module.ident == "tests" {
            if let Some((_, items)) = &module.content {
                for item in items {
                    self.visit_item(item);
                }
            }
        } else {
            visit::visit_item_mod(self, module);
        }
    }

    fn visit_attribute(&mut self, attribute: &'ast Attribute) {
        if attribute.path().is_ident("cfg")
            && attribute
                .parse_args::<Meta>()
                .is_ok_and(|meta| comments::requires_test(&meta))
        {
            self.findings.count += 1;
        }

        if self.test_scope || !condition_mentions_test(&attribute.meta) {
            return;
        }
        let on_item = (self.allows_inner_attributes || matches!(attribute.style, AttrStyle::Outer))
            && self
                .item_attributes
                .iter()
                .any(|item_attr| std::ptr::eq(item_attr, attribute));
        if !on_item {
            self.findings
                .non_item_lines
                .push(attribute.span().start().line);
        }
    }

    fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
        self.inspect_macro_tokens(invocation.tokens.clone());
    }
}

fn condition_mentions_test(meta: &Meta) -> bool {
    let Meta::List(list) = meta else {
        return false;
    };
    if list.path.is_ident("cfg") {
        return list
            .parse_args::<Meta>()
            .is_ok_and(|meta| predicate_mentions_test(&meta));
    }
    if list.path.is_ident("cfg_attr")
        && let Ok(parts) = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
    {
        let mut parts = parts.iter();
        return parts.next().is_some_and(predicate_mentions_test)
            || parts.any(condition_mentions_test);
    }
    false
}

fn predicate_mentions_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list)
            if ["all", "any", "not"]
                .iter()
                .any(|name| list.path.is_ident(name)) =>
        {
            list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                .is_ok_and(|parts| parts.iter().any(predicate_mentions_test))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_test_cfg_on_non_items() {
        let cases = [
            "struct State { #[cfg(test)] hook: () }",
            "enum State { #[cfg(any(test, unix))] Hook }",
            "fn drive(#[cfg_attr(test, allow(unused))] hook: ()) {}",
            "fn drive() { #[cfg(all(unix, test))] let hook = (); }",
            "fn drive() { #[cfg(not(test))] hook(); }",
            "fn drive() { match () { #[cfg(test)] () => () } }",
            "fn drive() { State { #[cfg_attr(unix, cfg(test))] hook: () }; }",
            "fn drive() { #[cfg(test)] hook!(); }",
        ];
        for source in cases {
            assert_eq!(inspect(source).unwrap().non_item_lines, [1], "{source}");
        }
    }

    #[test]
    fn counts_and_rejects_test_cfg_in_macro_inputs() {
        let source = r##"
fn drive() {
    tokio::select! {
        #[cfg(test)] _ = work() => (),
        #[cfg(not(test))] _ = work() => (),
        #[cfg(any(test, unix))] _ = work() => (),
        #[cfg_attr(test, allow(unused))] _ = work() => (),
    }
    assert!(matches!({ #[cfg(test)] hook(); state() }, Ready));
    trace!("#[cfg(test)]");
}
"##;
        let cfg = inspect(source).unwrap();
        assert_eq!(cfg.non_item_lines, [4, 5, 6, 7, 9]);
        assert_eq!(cfg.count, 5);
    }

    #[test]
    fn allows_items_and_ignores_test_subtrees() {
        let source = r##"
#[cfg(test)]
mod tests;
#[cfg(all(test, unix))]
mod probes {
    struct State { #[cfg(test)] hook: () }
    fn probe() { #[cfg(test)] let hook = (); }
}
#[cfg(not(test))]
mod probes {}
#[cfg(any(test, unix))]
impl State {
    #[cfg(not(test))]
    fn drive() {}
}
#[cfg_attr(test, allow(unused))]
use std::io;
#[cfg(test)]
macro_rules! hook { () => {} }
fn nested() {
    #[cfg(test)]
    fn fixture() {}
}
const FIXTURE: &str = "#[cfg(test)]";
"##;
        let cfg = inspect(source).unwrap();
        assert!(cfg.non_item_lines.is_empty());
        assert_eq!(cfg.count, 5);
    }

    #[test]
    fn checks_production_siblings_and_modules_that_can_build_without_test() {
        let source = r##"
#[cfg(test)] mod fixtures { fn probe() { #[cfg(test)] hook(); } } fn drive() { #[cfg(test)] hook(); }
#[cfg(any(test, unix))] mod either { fn drive() { #[cfg(test)] hook(); } }
#[cfg(not(test))] mod production { fn drive() { #[cfg(not(test))] hook(); } }
"##;
        assert_eq!(inspect(source).unwrap().non_item_lines, [2, 3, 4]);
    }

    #[test]
    fn reports_increases_and_decreases_until_the_count_matches() {
        assert!(mismatch("example", 2, 1).is_some());
        assert!(mismatch("example", 1, 0).is_some());
        assert!(mismatch("example", 1, 1).is_none());
    }
}
