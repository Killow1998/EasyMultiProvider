use super::{Path, Result, comments, read};
use std::collections::BTreeMap;
use syn::{
    Expr, Pat,
    parse::Parser as _,
    punctuated::Punctuated,
    spanned::Spanned,
    visit::{self, Visit},
};

const ALLOWED: &[(&str, usize)] = &[
    ("emp-app", 117),
    ("emp-codex", 3),
    ("emp-router", 3),
    ("emp-transport", 14),
];

pub(super) fn check(root: &Path, paths: &[String], errors: &mut Vec<String>) -> Result {
    let mut counts: BTreeMap<&str, Vec<String>> = ALLOWED
        .iter()
        .map(|&(name, _)| (name, Vec::new()))
        .collect();
    for path in paths {
        let Some(group) = group(path) else { continue };
        if !root.join(path).is_file() {
            continue;
        }
        for line in violations(path, &read(root, path)?)? {
            counts
                .entry(group)
                .or_default()
                .push(format!("{path}:{line}"));
        }
    }
    for (group, locations) in counts {
        let allowed = ALLOWED
            .iter()
            .find(|&&(name, _)| name == group)
            .map_or(0, |&(_, count)| count);
        check_baseline(group, &locations, allowed, errors);
    }
    Ok(())
}

fn check_baseline(group: &str, locations: &[String], allowed: usize, errors: &mut Vec<String>) {
    let actual = locations.len();
    if actual > allowed {
        errors.push(format!("[test-deadlines] {group}: 当前 {actual} 处裸写期限，基线 {allowed}；使用 test_timeout，计数棘轮只降不升\n{}", locations.join("\n")));
    } else if actual < allowed {
        errors.push(format!(
            "[test-deadlines] {group}: 当前 {actual} 处裸写期限，基线 {allowed}；请在同一提交将基线调低至 {actual}"
        ));
    }
}

fn group(path: &str) -> Option<&str> {
    if !path.ends_with(".rs") {
        return None;
    }
    let mut parts = path.split('/');
    match parts.next()? {
        "crates" => parts.next(),
        "xtask" => Some("xtask"),
        "tests" => Some("tests"),
        _ => None,
    }
}

fn violations(path: &str, contents: &str) -> Result<Vec<usize>> {
    let test_file = path.starts_with("crates/emp-test-support/src/") || comments::test_file(path);
    if !(test_file || (contents.contains("test") && contents.contains("#["))) {
        return Ok(Vec::new());
    }
    let source = syn::parse_file(contents).map_err(|error| format!("{path}: {error}"))?;
    let mut scopes = comments::TestScopes::default();
    if !test_file {
        scopes.visit_file(&source);
    }
    let mut visitor = Deadlines {
        test_file,
        emp_app: group(path) == Some("emp-app"),
        scopes,
        lines: Vec::new(),
    };
    visitor.visit_file(&source);
    visitor.lines.sort_unstable();
    Ok(visitor.lines)
}

struct Deadlines {
    test_file: bool,
    emp_app: bool,
    scopes: comments::TestScopes,
    lines: Vec<usize>,
}

impl Deadlines {
    fn inspect(&mut self, expr: &Expr, constants: bool) {
        let line = expr.span().start().line;
        if (self.test_file || self.scopes.contains(line))
            && bare_duration(expr, constants, self.emp_app, false)
        {
            self.lines.push(line);
        }
    }

    fn arguments(&mut self, name: &str, args: &syn::punctuated::Punctuated<Expr, syn::Token![,]>) {
        let argument = match name {
            "set_read_timeout"
            | "set_write_timeout"
            | "capped"
            | "read_timeout"
            | "write_timeout"
            | "timeout"
            | "recv_timeout"
            | "wait_timeout"
            | "wait_timeout_while"
            | "wait_exit_with_timeout"
            | "wait_for"
            | "eventually"
            | "drain_for_test" => args.first(),
            "connect_timeout"
            | "wait_for_state"
            | "wait_for_error"
            | "quiesce"
            | "wait_for_websocket_release" => args.last(),
            "probe_candidate_version" => args.iter().nth(3),
            "wait_for_candidate_version" => args.iter().nth(1),
            _ => None,
        };
        if let Some(expr) = argument {
            self.inspect(expr, true);
        }
    }
}

fn bare_duration(expr: &Expr, constants: bool, emp_app: bool, arithmetic: bool) -> bool {
    match expr {
        Expr::Paren(paren) => bare_duration(&paren.expr, constants, emp_app, arithmetic),
        Expr::Group(group) => bare_duration(&group.expr, constants, emp_app, arithmetic),
        Expr::Binary(binary) if matches!(binary.op, syn::BinOp::Add(_) | syn::BinOp::Sub(_)) => {
            if instant_now(&binary.left) {
                return false;
            }
            bare_duration(&binary.left, true, emp_app, true)
                || bare_duration(&binary.right, true, emp_app, true)
        }
        Expr::Call(call) => {
            let Expr::Path(path) = call.func.as_ref() else {
                return arithmetic;
            };
            let mut parts = path.path.segments.iter().rev();
            let Some(name) = parts.next() else {
                return false;
            };
            if name.ident == "Some" {
                return call
                    .args
                    .first()
                    .is_some_and(|expr| bare_duration(expr, constants, emp_app, arithmetic));
            }
            if name.ident == "test_timeout"
                || (emp_app && path_matches(path, &["crate", "server", "raw_tcp", "wire_timeout"]))
            {
                return false;
            }
            arithmetic
                || (matches!(
                    name.ident.to_string().as_str(),
                    "from_secs"
                        | "from_millis"
                        | "from_secs_f64"
                        | "from_micros"
                        | "from_nanos"
                        | "new"
                ) && parts.next().is_some_and(|part| part.ident == "Duration")
                    && call.args.iter().all(|arg| matches!(arg, Expr::Lit(_))))
        }
        Expr::Path(path) if constants => {
            if emp_app && path_matches(path, &["crate", "server", "REQUEST_DRAIN_TIMEOUT"]) {
                return false;
            }
            let name = path
                .path
                .segments
                .last()
                .expect("expression path")
                .ident
                .to_string();
            arithmetic
                || (!path.path.segments.iter().rev().nth(1).is_some_and(|part| {
                    part.ident == "Duration" && matches!(name.as_str(), "ZERO" | "MAX")
                }) && name.bytes().any(|byte| byte.is_ascii_uppercase())
                    && !name.bytes().any(|byte| byte.is_ascii_lowercase()))
        }
        Expr::Lit(literal) => matches!(literal.lit, syn::Lit::Int(_) | syn::Lit::Float(_)),
        _ => arithmetic,
    }
}

fn path_matches(path: &syn::ExprPath, expected: &[&str]) -> bool {
    path.path
        .segments
        .iter()
        .map(|part| &part.ident)
        .eq(expected.iter().copied())
}

fn instant_now(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Call(call)
            if matches!(call.func.as_ref(), Expr::Path(path) if path
                .path.segments.iter().rev().take(2).map(|part| &part.ident).eq(["now", "Instant"]))
    )
}

fn deadline_name(name: &syn::Ident) -> bool {
    name.to_string()
        .split('_')
        .any(|part| part.eq_ignore_ascii_case("timeout") || part.eq_ignore_ascii_case("deadline"))
}

impl<'ast> Visit<'ast> for Deadlines {
    fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
        let Some(name) = invocation.path.segments.last() else {
            return;
        };
        let expressions = match name.ident.to_string().as_str() {
            "assert" | "assert_eq" | "assert_ne" | "debug_assert" | "debug_assert_eq"
            | "debug_assert_ne" => Punctuated::<Expr, syn::Token![,]>::parse_terminated
                .parse2(invocation.tokens.clone())
                .map(|args| args.into_iter().collect()),
            "matches" | "assert_matches" | "debug_assert_matches" => {
                parse_match_arguments.parse2(invocation.tokens.clone())
            }
            _ => return,
        };
        if let Ok(expressions) = expressions {
            for expression in &expressions {
                self.visit_expr(expression);
            }
        }
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(path) = call.func.as_ref()
            && let Some(name) = path.path.segments.last()
        {
            if name.ident == "after"
                && path
                    .path
                    .segments
                    .iter()
                    .rev()
                    .nth(1)
                    .is_some_and(|part| part.ident == "Deadline")
            {
                if let Some(argument) = call.args.first() {
                    self.inspect(argument, true);
                }
            } else {
                self.arguments(&name.ident.to_string(), &call.args);
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        let name = call.method.to_string();
        if name == "wait_timeout" || name == "wait_timeout_while" {
            if let Some(expr) = call.args.iter().nth(1) {
                self.inspect(expr, true);
            }
        } else {
            self.arguments(&name, &call.args);
        }
        visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_binary(&mut self, expr: &'ast syn::ExprBinary) {
        if matches!(expr.op, syn::BinOp::Add(_) | syn::BinOp::Sub(_)) && instant_now(&expr.left) {
            self.inspect(&expr.right, true);
        }
        visit::visit_expr_binary(self, expr);
    }

    fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
        if deadline_name(&item.ident) {
            self.inspect(&item.expr, false);
        }
        visit::visit_item_const(self, item);
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        let pat = match &local.pat {
            Pat::Type(typed) => typed.pat.as_ref(),
            pat => pat,
        };
        if let Pat::Ident(pat) = pat
            && deadline_name(&pat.ident)
            && let Some(init) = &local.init
        {
            self.inspect(&init.expr, false);
        }
        visit::visit_local(self, local);
    }
}

fn parse_match_arguments(input: syn::parse::ParseStream<'_>) -> syn::Result<Vec<Expr>> {
    let mut expressions = vec![input.parse()?];
    input.parse::<syn::Token![,]>()?;
    input.call(Pat::parse_multi_with_leading_vert)?;
    if input.peek(syn::Token![if]) {
        input.parse::<syn::Token![if]>()?;
        expressions.push(input.parse()?);
    }
    if !input.is_empty() {
        input.parse::<syn::Token![,]>()?;
        expressions.extend(input.call(Punctuated::<Expr, syn::Token![,]>::parse_terminated)?);
    }
    Ok(expressions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bare_waits_and_allows_simulated_time() {
        let source = r#"
fn production() { stream.set_read_timeout(Some(Duration::from_secs(5))); }
#[cfg(all(unix, test))]
mod fixture {
    const LIMIT: Duration = Duration::from_secs(5);
    const TIMEOUT: Duration = Duration::from_secs(5);
    fn wire() {
        let deadline = Instant::now() + LIMIT;
        stream.set_read_timeout(Some(LIMIT));
        stream.set_write_timeout(Some(Duration::from_secs(5)));
        tokio::time::timeout(
            Duration::from_millis(200), future);
        receiver.recv_timeout(test_timeout(Duration::from_secs(5)));
        let deadline = Instant::now() + test_timeout(Duration::from_secs(5));
        tokio::time::advance(Duration::from_secs(3));
        assert!(elapsed < Duration::from_secs(10));
        TcpStream::connect_timeout(&address, Duration::from_secs(5));
    }
}
"#;
        assert_eq!(
            violations("crates/app/src/lib.rs", source).unwrap(),
            [6, 8, 9, 10, 12, 17]
        );
        assert_eq!(
            violations("crates/app/tests/wire.rs", source).unwrap(),
            [2, 6, 8, 9, 10, 12, 17]
        );
    }

    #[test]
    fn inspects_assertion_operands_and_match_guards() {
        let source = r#"
#[test]
fn account_import() {
    assert!(
        done_rx
            .recv_timeout(Duration::from_millis(300))
            .is_err(),
        "import replaced credentials while a refresh held the account lock"
    );
    assert_eq!(rx.recv_timeout(LIMIT), Ok(()));
    assert_ne!(rx.recv_timeout(LIMIT), Ok(()));
    debug_assert!(rx.recv_timeout(LIMIT).is_ok());
    debug_assert_eq!(rx.recv_timeout(LIMIT), Ok(()));
    debug_assert_ne!(rx.recv_timeout(LIMIT), Ok(()));
    assert!(matches!(rx.recv_timeout(LIMIT), Ok(())));
    assert!(matches!(state, Ready if rx.recv_timeout(LIMIT).is_ok()));
}
"#;
        assert_eq!(
            violations("crates/emp-app/src/services/accounts/tests.rs", source).unwrap(),
            [6, 10, 11, 12, 13, 14, 15, 16]
        );
    }

    #[test]
    fn preserves_production_deadlines_and_scales_only_fixture_margin() {
        let source = r#"
#[test]
fn drain() {
    timeout(crate::server::REQUEST_DRAIN_TIMEOUT, future);
    rx.recv_timeout(crate::server::REQUEST_DRAIN_TIMEOUT + test_timeout(Duration::from_secs(5)));
    rx.recv_timeout(crate::server::REQUEST_DRAIN_TIMEOUT + Duration::from_secs(5));
    rx.recv_timeout(FIXTURE_WAIT + test_timeout(Duration::from_secs(5)));
    rx.recv_timeout(test_timeout(Duration::from_secs(5)) - FIXTURE_WAIT);
    rx.recv_timeout(crate::server::REQUEST_DRAIN_TIMEOUT - test_timeout(Duration::from_secs(5)));
    rx.recv_timeout(test_timeout(Duration::from_secs(5)) + (FIXTURE_WAIT - test_timeout(Duration::from_secs(1))));
    let budget = Deadline::after(Duration::from_secs(5));
    let budget = Deadline::after(test_timeout(Duration::from_secs(5)));
    rx.recv_timeout(crate::server::REQUEST_DRAIN_TIMEOUT + crate::server::raw_tcp::wire_timeout());
    rx.recv_timeout(crate::server::REQUEST_DRAIN_TIMEOUT + wire_timeout());
}
"#;
        assert_eq!(
            violations("crates/emp-app/tests/wire.rs", source).unwrap(),
            [6, 7, 8, 10, 11, 14]
        );
    }

    #[test]
    fn checks_ordinary_test_support_functions() {
        let source = "pub fn receive() { rx.recv_timeout(Duration::from_secs(5)); }";
        assert_eq!(
            violations("crates/emp-test-support/src/fixture/wire.rs", source).unwrap(),
            [1]
        );
        assert!(
            violations("crates/emp-app/src/wire.rs", source)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn baseline_requires_an_exact_match() {
        let mut errors = Vec::new();
        check_baseline("sample", &[], 1, &mut errors);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("在同一提交将基线调低至 0"));

        errors.clear();
        let locations = ["tests/wire.rs:42".to_owned()];
        check_baseline("sample", &locations, 1, &mut errors);
        assert!(errors.is_empty());
        check_baseline("sample", &locations, 0, &mut errors);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("tests/wire.rs:42"));
    }
}
