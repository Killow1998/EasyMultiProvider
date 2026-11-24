use super::{Path, Result, comments, read};
use syn::{
    Expr,
    parse::Parser as _,
    punctuated::Punctuated,
    visit::{self, Visit},
};

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
        for line in violations(path, &read(root, path)?)? {
            errors.push(format!(
                "[test-fixture-root] {path}:{line}: 临时夹具使用 emp_test_support::fixture_dir 或 fixture_root"
            ));
        }
    }
    Ok(())
}

fn violations(path: &str, contents: &str) -> Result<Vec<usize>> {
    let test_file = comments::test_file(path);
    if !(test_file || (contents.contains("test") && contents.contains("#["))) {
        return Ok(Vec::new());
    }
    let file = syn::parse_file(contents).map_err(|error| format!("{path}: {error}"))?;
    let mut visitor = FixtureRoots {
        test_file,
        scopes: comments::TestScopes::default(),
        lines: Vec::new(),
    };
    if !test_file {
        visitor.scopes.visit_file(&file);
    }
    visitor.visit_file(&file);
    Ok(visitor.lines)
}

struct FixtureRoots {
    test_file: bool,
    scopes: comments::TestScopes,
    lines: Vec<usize>,
}

impl FixtureRoots {
    fn push(&mut self, line: usize) {
        if self.test_file || self.scopes.contains(line) {
            self.lines.push(line);
        }
    }
}

impl<'ast> Visit<'ast> for FixtureRoots {
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
            if let Some(name) = segments.next()
                && (name == "tempdir"
                    || name == "temp_dir"
                    || name == "tempfile"
                    || name == "new"
                        && segments
                            .next()
                            .is_some_and(|owner| owner == "TempDir" || owner == "NamedTempFile"))
            {
                self.push(name.span().start().line);
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "tempdir" || call.method == "tempfile" {
            self.push(call.method.span().start().line);
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
use tempfile::tempdir;
fn fixture() {
    tempdir();
    tempfile::TempDir::new();
    tempfile::Builder::new().prefix("case-").tempdir();
    tempfile::NamedTempFile::new();
    std::env::temp_dir();
    assert!(tempfile::tempdir().is_ok());
    emp_test_support::fixture_dir();
    tempfile::Builder::new().prefix("case-").tempdir_in(emp_test_support::fixture_root());
}
"#;
        assert_eq!(
            violations("crates/example/tests/support.rs", integration).unwrap(),
            [4, 5, 6, 7, 8, 9]
        );
        let unit = r#"
fn setup_runtime() { tempfile::tempdir(); }
#[cfg(all(test, unix))]
mod tests {
    fn fixture() { tempfile::tempdir(); }
}
"#;
        assert_eq!(violations("crates/example/src/lib.rs", unit).unwrap(), [5]);
    }
}
