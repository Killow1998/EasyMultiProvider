use super::{
    Path, Result,
    imports::Imports,
    macro_arguments, read,
    test_scope::{Index, TestScopes},
};
use std::collections::BTreeMap;
use syn::{
    Expr, Pat,
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

pub(super) fn check(
    root: &Path,
    paths: &[String],
    scopes: &Index,
    errors: &mut Vec<String>,
) -> Result {
    for path in paths.iter().filter(|path| {
        (path.starts_with("crates/") && !path.starts_with("crates/emp-mod-")
            || path.starts_with("xtask/")
            || path.starts_with("tests/"))
            && path.ends_with(".rs")
    }) {
        if !root.join(path).is_file() {
            continue;
        }
        for (line, guidance) in inspect_scoped(path, &read(root, path)?, scopes.get(path))? {
            errors.push(format!("[test-fixture-root] {path}:{line}: {guidance}"));
        }
    }
    Ok(())
}

fn inspect_scoped(
    path: &str,
    contents: &str,
    scopes: &TestScopes,
) -> Result<Vec<(usize, &'static str)>> {
    if !contents.contains("temp") && !contents.contains("Temp") {
        return Ok(Vec::new());
    }
    let file = syn::parse_file(contents).map_err(|error| format!("{path}: {error}"))?;
    let mut visitor = FixtureRoots {
        xtask: path.starts_with("xtask/"),
        scopes,
        imports: Imports::file(path, &file.items),
        builder_bindings: Vec::new(),
        production_roots_remaining: PRODUCTION_ROOTS
            .iter()
            .find_map(|&(source, count)| (source == path).then_some(count))
            .unwrap_or_default(),
        violations: Vec::new(),
    };
    visitor.visit_file(&file);
    Ok(visitor.violations)
}

struct FixtureRoots<'source> {
    xtask: bool,
    scopes: &'source TestScopes,
    imports: Imports<'source>,
    builder_bindings: Vec<BTreeMap<String, bool>>,
    production_roots_remaining: usize,
    violations: Vec<(usize, &'static str)>,
}

impl FixtureRoots<'_> {
    fn with_parameters(&mut self, signature: &syn::Signature, visit: impl FnOnce(&mut Self)) {
        let mut bindings = BTreeMap::new();
        for input in &signature.inputs {
            if let syn::FnArg::Typed(input) = input {
                PatternBindings(&mut bindings).visit_pat(&input.pat);
            }
        }
        self.builder_bindings.push(bindings);
        visit(self);
        self.builder_bindings.pop();
    }

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
            Expr::Path(path) => path.path.get_ident().is_some_and(|name| {
                let name = name.to_string();
                self.builder_bindings
                    .iter()
                    .rev()
                    .find_map(|scope| scope.get(&name))
                    .copied()
                    .unwrap_or_default()
            }),
            Expr::Paren(paren) => self.is_builder(&paren.expr),
            Expr::Group(group) => self.is_builder(&group.expr),
            _ => false,
        }
    }

    fn in_test(&self, line: usize) -> bool {
        self.scopes.contains(line)
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

struct PatternBindings<'scope>(&'scope mut BTreeMap<String, bool>);

impl<'ast> Visit<'ast> for PatternBindings<'_> {
    fn visit_pat_ident(&mut self, pattern: &'ast syn::PatIdent) {
        self.0.insert(pattern.ident.to_string(), false);
        visit::visit_pat_ident(self, pattern);
    }

    fn visit_block(&mut self, _: &'ast syn::Block) {}
}

impl<'ast> Visit<'ast> for FixtureRoots<'_> {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let outer_bindings = std::mem::take(&mut self.builder_bindings);
        visit::visit_item(self, item);
        self.builder_bindings = outer_bindings;
    }

    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        if module.content.is_some() {
            self.imports.enter_module(module);
            visit::visit_item_mod(self, module);
            self.imports.leave_scope();
        }
    }

    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        self.with_parameters(&function.sig, |visitor| {
            visit::visit_item_fn(visitor, function);
        });
    }

    fn visit_impl_item_fn(&mut self, function: &'ast syn::ImplItemFn) {
        self.with_parameters(&function.sig, |visitor| {
            visit::visit_impl_item_fn(visitor, function);
        });
    }

    fn visit_trait_item_fn(&mut self, function: &'ast syn::TraitItemFn) {
        self.with_parameters(&function.sig, |visitor| {
            visit::visit_trait_item_fn(visitor, function);
        });
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        self.imports.enter_block(block);
        self.builder_bindings.push(BTreeMap::new());
        visit::visit_block(self, block);
        self.builder_bindings.pop();
        self.imports.leave_scope();
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        visit::visit_local(self, local);
        let pat = match &local.pat {
            Pat::Type(typed) => typed.pat.as_ref(),
            pat => pat,
        };
        let builder = match pat {
            Pat::Ident(binding) => local
                .init
                .as_ref()
                .filter(|init| self.is_builder(&init.expr))
                .map(|_| binding.ident.to_string()),
            _ => None,
        };
        let scope = self
            .builder_bindings
            .last_mut()
            .expect("local declaration in a block");
        PatternBindings(scope).visit_pat(pat);
        if let Some(name) = builder {
            scope.insert(name, true);
        }
    }

    fn visit_expr_assign(&mut self, assignment: &'ast syn::ExprAssign) {
        visit::visit_expr_assign(self, assignment);
        if let Expr::Path(path) = assignment.left.as_ref()
            && let Some(name) = path.path.get_ident()
            && self.is_builder(&assignment.right)
        {
            let name = name.to_string();
            if let Some(binding) = self
                .builder_bindings
                .iter_mut()
                .rev()
                .find_map(|scope| scope.get_mut(&name))
            {
                *binding = true;
            }
        }
    }

    fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
        if let Ok(expressions) = macro_arguments(invocation) {
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

    fn violations(path: &str, contents: &str) -> Result<Vec<(usize, &'static str)>> {
        inspect_scoped(
            path,
            contents,
            &super::super::test_scope::tests::fixture(path, contents),
        )
    }

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
        let source = r#"
fn fixture() {
    let mut initialized = tempfile::Builder::new();
    initialized = Default::default();
    initialized.make(create);
    let assigned;
    {
        assigned = tempfile::Builder::default();
    }
    assigned.make(create);
}
"#;
        assert_eq!(
            violations("crates/example/tests/wire.rs", source).unwrap(),
            [(5, FIXTURE_ROOT_GUIDANCE), (10, FIXTURE_ROOT_GUIDANCE)]
        );
    }

    #[test]
    fn rejects_implicit_builder_root_after_assigning_parameters_and_destructured_bindings() {
        let source = r#"
fn fixture(mut parameter: tempfile::Builder, (mut destructured,): (tempfile::Builder,)) {
    let (mut local, factory) = (tempfile::Builder::default(), Factory::new());
    {
        parameter = tempfile::Builder::new();
        destructured = tempfile::Builder::new();
        local = tempfile::Builder::new();
    }
    parameter.make(create);
    destructured.make(create);
    local.make(create);
    local.make_in(root, create);
    factory.make(create);
}
"#;
        assert_eq!(
            violations("crates/example/tests/wire.rs", source).unwrap(),
            [
                (9, FIXTURE_ROOT_GUIDANCE),
                (10, FIXTURE_ROOT_GUIDANCE),
                (11, FIXTURE_ROOT_GUIDANCE),
            ]
        );
    }

    #[test]
    fn rejects_implicit_builder_root_through_relative_imports() {
        let source = r#"
use tempfile as temp;
use self::temp::Builder as Fixture;
mod nested {
    use super::Fixture as Parent;
    mod child {
        use super::super::temp::Builder as Root;
        fn fixture() { Root::new().make(create); }
    }
    fn fixture() {
        use other::temp;
        use self::Parent as Local;
        Local::new().make(create);
        temp::Builder::new().make(create);
    }
}
fn fixture() { Fixture::new().make(create); }
"#;
        assert_eq!(
            violations("crates/example/tests/wire.rs", source).unwrap(),
            [
                (8, FIXTURE_ROOT_GUIDANCE),
                (13, FIXTURE_ROOT_GUIDANCE),
                (17, FIXTURE_ROOT_GUIDANCE),
            ]
        );
    }

    #[test]
    fn rejects_implicit_builder_root_through_import_alias() {
        let source = r#"
use tempfile as temp;
use temp::Builder as Fixture;
fn fixture() {
    Fixture::new().make(create);
    temp::Builder::new().make(create);
    {
        use other::temp;
        Fixture::new().make(create);
        temp::Builder::new().make(create);
    }
}
"#;
        assert_eq!(
            violations("crates/example/tests/wire.rs", source).unwrap(),
            [
                (5, FIXTURE_ROOT_GUIDANCE),
                (6, FIXTURE_ROOT_GUIDANCE),
                (9, FIXTURE_ROOT_GUIDANCE)
            ]
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
