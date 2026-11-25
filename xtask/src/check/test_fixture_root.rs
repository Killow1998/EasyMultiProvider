use super::{Path, Result, comments, imports::Imports, read};
use std::collections::BTreeSet;
use syn::{
    Expr, Pat,
    parse::Parser as _,
    punctuated::Punctuated,
    visit::{self, Visit},
};

const PRODUCTION_ROOTS: &[(&str, usize)] = &[
    ("crates/emp-app/src/services/claude_cli/local_query.rs", 1),
    (
        "crates/emp-app/src/services/claude_cli/tui/session/setup.rs",
        1,
    ),
    ("crates/emp-codex/src/quota/process.rs", 2),
    (
        "crates/emp-codex/src/quota/process/subscription_login.rs",
        1,
    ),
    ("crates/emp-codex/src/runtime_inventory/version.rs", 2),
    ("xtask/src/main.rs", 1),
];

const FIXTURE_ROOT_GUIDANCE: &str = "临时夹具使用 emp_test_support::fixture_dir 或 fixture_root";
const PRODUCTION_ROOT_GUIDANCE: &str = "显式传入目录，或将生产用途加入 PRODUCTION_ROOTS";

pub(super) fn check(root: &Path, paths: &[String], errors: &mut Vec<String>) -> Result {
    for path in paths.iter().filter(|path| {
        (path.starts_with("crates/") && !path.starts_with("crates/emp-mod-")
            || path.starts_with("xtask/")
            || path.starts_with("tests/"))
            && path.ends_with(".rs")
    }) {
        if !root.join(path).is_file() {
            continue;
        }
        for (line, guidance) in violations(path, &read(root, path)?)? {
            errors.push(format!("[test-fixture-root] {path}:{line}: {guidance}"));
        }
    }
    Ok(())
}

fn violations(path: &str, contents: &str) -> Result<Vec<(usize, &'static str)>> {
    if !contents.contains("temp") && !contents.contains("Temp") {
        return Ok(Vec::new());
    }
    let file = syn::parse_file(contents).map_err(|error| format!("{path}: {error}"))?;
    let mut visitor = FixtureRoots {
        test_file: comments::test_file(path),
        xtask: path.starts_with("xtask/"),
        scopes: comments::TestScopes::default(),
        imports: Imports::module(&file.items),
        builder_bindings: BTreeSet::new(),
        production_roots_remaining: PRODUCTION_ROOTS
            .iter()
            .find_map(|&(source, count)| (source == path).then_some(count))
            .unwrap_or_default(),
        violations: Vec::new(),
    };
    if !visitor.test_file {
        visitor.scopes.visit_file(&file);
    }
    visitor.visit_file(&file);
    Ok(visitor.violations)
}

struct FixtureRoots {
    test_file: bool,
    xtask: bool,
    scopes: comments::TestScopes,
    imports: Imports,
    builder_bindings: BTreeSet<String>,
    production_roots_remaining: usize,
    violations: Vec<(usize, &'static str)>,
}

impl FixtureRoots {
    fn is_builder(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Call(call) => {
                let Expr::Path(path) = call.func.as_ref() else {
                    return false;
                };
                let parts = self.imports.resolve(&path.path);
                parts == ["tempfile", "Builder", "new"]
                    || parts == ["tempfile", "Builder", "default"]
            }
            Expr::MethodCall(call)
                if matches!(
                    call.method.to_string().as_str(),
                    "prefix"
                        | "suffix"
                        | "rand_bytes"
                        | "permissions"
                        | "append"
                        | "disable_cleanup"
                ) =>
            {
                self.is_builder(&call.receiver)
            }
            Expr::Path(path) => path
                .path
                .get_ident()
                .is_some_and(|name| self.builder_bindings.contains(&name.to_string())),
            Expr::Paren(paren) => self.is_builder(&paren.expr),
            Expr::Group(group) => self.is_builder(&group.expr),
            _ => false,
        }
    }

    fn in_test(&self, line: usize) -> bool {
        self.test_file || self.scopes.contains(line)
    }

    fn uses_implicit_root(&mut self, line: usize) {
        if self.in_test(line) {
            self.violations.push((line, FIXTURE_ROOT_GUIDANCE));
        } else if self.production_roots_remaining == 0 {
            self.violations.push((line, PRODUCTION_ROOT_GUIDANCE));
        } else {
            self.production_roots_remaining -= 1;
        }
    }
}

impl<'ast> Visit<'ast> for FixtureRoots {
    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        if let Some((_, items)) = &module.content {
            let outer_imports = std::mem::replace(&mut self.imports, Imports::module(items));
            let outer_bindings = std::mem::take(&mut self.builder_bindings);
            visit::visit_item_mod(self, module);
            self.builder_bindings = outer_bindings;
            self.imports = outer_imports;
        }
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        self.imports.enter_block(block);
        let outer_bindings = self.builder_bindings.clone();
        visit::visit_block(self, block);
        self.builder_bindings = outer_bindings;
        self.imports.leave_block();
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        visit::visit_local(self, local);
        let pat = match &local.pat {
            Pat::Type(typed) => typed.pat.as_ref(),
            pat => pat,
        };
        if let Pat::Ident(pat) = pat {
            let name = pat.ident.to_string();
            if local
                .init
                .as_ref()
                .is_some_and(|init| self.is_builder(&init.expr))
            {
                self.builder_bindings.insert(name);
            } else {
                self.builder_bindings.remove(&name);
            }
        }
    }

    fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
        if let Ok(expressions) =
            Punctuated::<Expr, syn::Token![,]>::parse_terminated.parse2(invocation.tokens.clone())
        {
            for expression in &expressions {
                self.visit_expr(expression);
            }
        }
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(path) = call.func.as_ref() {
            let mut segments = path
                .path
                .segments
                .iter()
                .rev()
                .map(|segment| &segment.ident);
            if let Some(name) = segments.next() {
                let line = name.span().start().line;
                let owner = segments.next();
                if name == "tempdir"
                    || name == "temp_dir"
                    || name == "tempfile"
                    || name == "spooled_tempfile"
                    || name == "make"
                        && self.imports.resolve(&path.path) == ["tempfile", "Builder", "make"]
                    || name == "new" && owner.is_some_and(|owner| owner == "SpooledTempFile")
                    || (name == "new" || name == "with_prefix" || name == "with_suffix")
                        && owner.is_some_and(|owner| owner == "TempDir" || owner == "NamedTempFile")
                {
                    self.uses_implicit_root(line);
                } else if self.xtask && name == "temporary_directory" && self.in_test(line) {
                    self.violations.push((line, FIXTURE_ROOT_GUIDANCE));
                }
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "tempdir"
            || call.method == "tempfile"
            || call.method == "make" && self.is_builder(&call.receiver)
        {
            self.uses_implicit_root(call.method.span().start().line);
        }
        visit::visit_expr_method_call(self, call);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_implicit_fixture_roots_in_integration_and_unit_tests() {
        let integration = r#"
use tempfile::{NamedTempFile, TempDir, tempdir};
fn fixture() {
    tempdir();
    tempfile::TempDir::new();
    tempfile::Builder::new().prefix("case-").tempdir();
    tempfile::NamedTempFile::new();
    std::env::temp_dir();
    assert!(tempfile::tempdir().is_ok());
    TempDir::with_prefix("case-");
    TempDir::with_suffix("-case");
    NamedTempFile::with_prefix("case-");
    NamedTempFile::with_suffix(".json");
    emp_test_support::fixture_dir();
    let root = emp_test_support::fixture_root();
    tempfile::Builder::new().prefix("case-").tempdir_in(&root);
    TempDir::with_prefix_in("case-", &root);
    TempDir::with_suffix_in("-case", &root);
    NamedTempFile::with_prefix_in("case-", &root);
    NamedTempFile::with_suffix_in(".json", &root);
}
"#;
        assert_eq!(
            violations("crates/example/tests/support.rs", integration).unwrap(),
            (4..=13)
                .map(|line| (line, FIXTURE_ROOT_GUIDANCE))
                .collect::<Vec<_>>()
        );
        let unit = r#"
#[cfg(all(test, unix))]
mod tests {
    fn fixture() { crate::temporary_directory("case-"); }
    fn direct() { tempfile::tempdir(); }
}
"#;
        assert_eq!(
            violations("xtask/src/package.rs", unit).unwrap(),
            [(4, FIXTURE_ROOT_GUIDANCE), (5, FIXTURE_ROOT_GUIDANCE)]
        );
    }

    #[test]
    fn requires_explicit_roots_for_builder_make_and_spooled_files() {
        let source = r#"
fn fixture() {
    tempfile::Builder::new().prefix("case-").make(create);
    tempfile::Builder::make(&builder, create);
    tempfile::spooled_tempfile(0);
    tempfile::SpooledTempFile::new(0);
    tempfile::Builder::new().make_in(root, create);
    tempfile::spooled_tempfile_in(0, root);
    tempfile::SpooledTempFile::new_in(0, root);
    Factory::new().prefix("case-").make(create);
    Factory::make(&factory, create);
    other::Builder::make(&builder, create);
    factory.make(create);
}
"#;
        assert_eq!(
            violations("crates/example/tests/wire.rs", source).unwrap(),
            (3..=6)
                .map(|line| (line, FIXTURE_ROOT_GUIDANCE))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn rejects_implicit_builder_root_through_local_binding() {
        let source =
            "fn fixture() { let builder = tempfile::Builder::new(); builder.make(create); }";
        assert_eq!(
            violations("crates/example/tests/wire.rs", source).unwrap(),
            [(1, FIXTURE_ROOT_GUIDANCE)]
        );
    }

    #[test]
    fn rejects_implicit_builder_root_through_import_alias() {
        let source =
            "use tempfile::{Builder as Fixture}; fn fixture() { Fixture::new().make(create); }";
        assert_eq!(
            violations("crates/example/tests/wire.rs", source).unwrap(),
            [(1, FIXTURE_ROOT_GUIDANCE)]
        );
    }

    #[test]
    fn rejects_implicit_root_from_default_builder() {
        let source = "fn fixture() { tempfile::Builder::default().make(create); }";
        assert_eq!(
            violations("crates/example/tests/wire.rs", source).unwrap(),
            [(1, FIXTURE_ROOT_GUIDANCE)]
        );
    }

    #[test]
    fn allows_explicit_roots_with_local_and_imported_builders() {
        let source = r#"
use tempfile::Builder as Fixture;
fn fixture() {
    let mut builder = Fixture::default();
    builder.prefix("case-").make_in(root, create);
    builder.tempdir_in(root);
    Fixture::new().tempfile_in(root);
    other::Builder::new().make(create);
}
"#;
        assert!(
            violations("crates/example/tests/wire.rs", source)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn requires_review_for_implicit_roots_in_non_test_helpers() {
        let helper = "fn make_directory() { tempfile::Builder::new().tempdir(); }";
        assert_eq!(
            violations("xtask/src/scratch.rs", helper).unwrap(),
            [(1, PRODUCTION_ROOT_GUIDANCE)]
        );
        assert!(violations("xtask/src/main.rs", helper).unwrap().is_empty());

        let second_helper = format!("{helper}\nfn another_directory() {{ tempfile::tempdir(); }}");
        assert_eq!(
            violations("xtask/src/main.rs", &second_helper).unwrap(),
            [(2, PRODUCTION_ROOT_GUIDANCE)]
        );
    }
}
