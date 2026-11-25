use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use syn::{
    spanned::Spanned,
    visit::{self, Visit},
};

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum SourceKind {
    Production,
    Fixture,
    Integration,
}

impl SourceKind {
    pub(super) fn in_test(self, attrs: &[syn::Attribute]) -> Self {
        if self == Self::Production && attrs.iter().any(test_only_attribute) {
            Self::Fixture
        } else {
            self
        }
    }
}

#[derive(Default)]
pub(super) struct Index(BTreeMap<PathBuf, TestScopes>);

impl Index {
    pub(super) fn record(&mut self, path: &Path, source: &syn::File, kind: SourceKind) {
        let scopes = TestScopes::scan(source, kind);
        self.0
            .entry(path.to_owned())
            .and_modify(|previous| previous.merge(&scopes))
            .or_insert(scopes);
    }

    pub(super) fn get(&self, path: &str) -> &TestScopes {
        static EMPTY: TestScopes = TestScopes {
            entire_file: false,
            integration: false,
            inline: Vec::new(),
        };
        self.0.get(Path::new(path)).unwrap_or(&EMPTY)
    }
}

#[derive(Default)]
pub(super) struct TestScopes {
    entire_file: bool,
    pub(super) integration: bool,
    inline: Vec<std::ops::RangeInclusive<usize>>,
}

impl TestScopes {
    pub(super) fn contains(&self, line: usize) -> bool {
        self.entire_file || self.contains_inline(line)
    }

    pub(super) fn has_test_scope(&self) -> bool {
        self.entire_file || !self.inline.is_empty()
    }

    pub(super) fn entire_file(&self) -> bool {
        self.entire_file
    }

    pub(super) fn contains_inline(&self, line: usize) -> bool {
        self.inline.iter().any(|range| range.contains(&line))
    }

    pub(super) fn scan(file: &syn::File, kind: SourceKind) -> Self {
        let mut scopes = Self {
            entire_file: kind.in_test(&file.attrs) != SourceKind::Production,
            integration: kind == SourceKind::Integration,
            ..Self::default()
        };
        scopes.visit_file(file);
        scopes
    }

    pub(super) fn merge(&mut self, other: &Self) {
        self.entire_file &= other.entire_file;
        self.integration |= other.integration;
    }

    fn add(&mut self, item: &impl Spanned, attrs: &[syn::Attribute]) -> bool {
        let test_only = attrs.iter().any(test_only_attribute);
        if test_only {
            let span = item.span();
            self.inline.push(span.start().line..=span.end().line);
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
                .is_ok_and(|meta| requires_test_scope(&meta, true)))
        || path
            .segments
            .iter()
            .map(|segment| &segment.ident)
            .eq(["tokio", "test"])
}

pub(super) fn requires_test(meta: &syn::Meta) -> bool {
    requires_test_scope(meta, false)
}

fn requires_test_scope(meta: &syn::Meta, support: bool) -> bool {
    if matches!(meta, syn::Meta::Path(path) if path.is_ident("test")) {
        return true;
    }
    if support
        && let syn::Meta::NameValue(value) = meta
        && value.path.is_ident("feature")
        && let syn::Expr::Lit(expr) = &value.value
        && matches!(&expr.lit, syn::Lit::Str(name) if name.value() == "test-support")
    {
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
        parts.iter().any(|meta| requires_test_scope(meta, support))
    } else {
        list.path.is_ident("any")
            && !parts.is_empty()
            && parts.iter().all(|meta| requires_test_scope(meta, support))
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::check::{module_paths, test_drop};
    use std::fs;

    pub(in crate::check) fn fixture(path: &str, source: &str) -> TestScopes {
        let kind = if path.split('/').any(|part| part == "tests") {
            SourceKind::Integration
        } else if path.ends_with("/tests.rs") || path.starts_with("crates/emp-test-support/") {
            SourceKind::Fixture
        } else {
            SourceKind::Production
        };
        TestScopes::scan(&syn::parse_file(source).unwrap(), kind)
    }

    #[test]
    fn checks_drop_in_modules_that_inherit_test_scope() {
        let directory = emp_test_support::fixture_dir().unwrap();
        let root = directory.path();
        for dir in ["src/checks", "src/fixtures", "tests"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(
            root.join("src/lib.rs"),
            r#"
#[cfg(test)] mod checks;
#[cfg(feature = "test-support")] mod feature_checks;
#[cfg(any(test, feature = "test-support"))]
#[path = "fixtures/suite.rs"] mod suite;
#[cfg(any(test, unix))] mod platform;
mod shared;
"#,
        )
        .unwrap();
        fs::write(
            root.join("tests/contract.rs"),
            "#[path = \"../src/shared.rs\"] mod common;",
        )
        .unwrap();
        fs::write(root.join("src/fixtures/suite.rs"), "mod moved;").unwrap();
        let failing_drop = "impl Drop for Worker { fn drop(&mut self) { self.join().unwrap(); } }";
        fs::write(
            root.join("src/checks.rs"),
            format!("mod nested;\n{failing_drop}"),
        )
        .unwrap();
        for path in [
            "src/checks/nested.rs",
            "src/feature_checks.rs",
            "src/fixtures/moved.rs",
            "src/platform.rs",
            "src/shared.rs",
        ] {
            fs::write(root.join(path), failing_drop).unwrap();
        }

        let mut scopes = Index::default();
        let mut errors = Vec::new();
        let library = root.join("src/lib.rs");
        let contract = root.join("tests/contract.rs");
        module_paths::check(
            root,
            root,
            [
                (library.as_path(), SourceKind::Production),
                (contract.as_path(), SourceKind::Integration),
            ]
            .into_iter(),
            &mut Default::default(),
            &mut scopes,
            &mut errors,
        )
        .unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        for (path, expected) in [
            ("src/checks.rs", true),
            ("src/checks/nested.rs", true),
            ("src/feature_checks.rs", true),
            ("src/fixtures/moved.rs", true),
            ("src/platform.rs", false),
            ("src/shared.rs", false),
        ] {
            let source = fs::read_to_string(root.join(path)).unwrap();
            let mut errors = Vec::new();
            test_drop::check(path, &source, scopes.get(path), &mut errors).unwrap();
            assert_eq!(errors.len(), usize::from(expected), "{path}: {errors:?}");
        }
        assert!(scopes.get("src/fixtures/suite.rs").entire_file());
        assert!(scopes.get("src/shared.rs").integration);
    }
}
