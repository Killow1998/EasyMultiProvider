use super::{
    Path, Result,
    imports::Imports,
    macro_arguments, read,
    test_scope::{Index, TestScopes},
};
use proc_macro2::{TokenStream, TokenTree};
use std::collections::{BTreeMap, BTreeSet};
use syn::{
    Expr, Pat, UseTree,
    ext::IdentExt as _,
    parse::Parser as _,
    punctuated::Punctuated,
    spanned::Spanned,
    visit::{self, Visit},
};

const ALLOWED: &[(&str, usize)] = &[("emp-app", 62), ("emp-codex", 2), ("emp-transport", 9)];

const SOCKET_TIMEOUTS: &[(&str, usize)] = &[("emp-app", 5)];
const TEST_SUPPORT_LIB: &str = "crates/emp-test-support/src/lib.rs";

pub(super) fn check(
    root: &Path,
    paths: &[String],
    scopes: &Index,
    errors: &mut Vec<String>,
) -> Result {
    let mut counts: BTreeMap<&str, (Vec<String>, Vec<String>)> = ALLOWED
        .iter()
        .chain(SOCKET_TIMEOUTS)
        .map(|&(name, _)| (name, (Vec::new(), Vec::new())))
        .collect();
    for path in paths {
        let Some(group) = group(path) else { continue };
        if !root.join(path).is_file() {
            continue;
        }
        let violations = inspect_scoped(path, &read(root, path)?, scopes.get(path))?;
        let (bare_locations, socket_locations) = counts.entry(group).or_default();
        bare_locations.extend(
            violations
                .bare
                .into_iter()
                .map(|line| format!("{path}:{line}")),
        );
        socket_locations.extend(
            violations
                .sockets
                .into_iter()
                .map(|line| format!("{path}:{line}")),
        );
        for line in violations.fixed_wait_in_tests {
            errors.push(format!("[test-deadlines] {path}:{line}: 测试不得写 FixedWait 标识符，只引用产品常量；测试余量使用 test_timeout"));
        }
        for line in violations.fixed_wait_renames {
            errors.push(format!(
                "[test-deadlines] {path}:{line}: FixedWait 不重命名，请保留原名"
            ));
        }
    }
    for (group, (bare, sockets)) in counts {
        for (rule, locations, baseline, label, fix) in [
            (
                "test-deadlines",
                bare,
                ALLOWED,
                "裸写期限",
                "使用 test_timeout",
            ),
            (
                "test-socket-timeouts",
                sockets,
                SOCKET_TIMEOUTS,
                "直接设置 socket 超时",
                "使用 Deadline::io 或带期限的读取接口",
            ),
        ] {
            let allowed = baseline
                .iter()
                .find(|&&(name, _)| name == group)
                .map_or(0, |&(_, count)| count);
            check_baseline(rule, group, &locations, allowed, label, fix, errors);
        }
    }
    Ok(())
}

pub(super) fn check_baseline(
    rule: &str,
    group: &str,
    locations: &[String],
    allowed: usize,
    label: &str,
    fix: &str,
    errors: &mut Vec<String>,
) {
    let actual = locations.len();
    if actual > allowed {
        errors.push(format!("[{rule}] {group}: 当前 {actual} 处{label}，基线 {allowed}；{fix}，计数棘轮只降不升\n{}", locations.join("\n")));
    } else if actual < allowed {
        errors.push(format!(
            "[{rule}] {group}: 当前 {actual} 处{label}，基线 {allowed}；请在同一提交将基线调低至 {actual}"
        ));
    }
}

pub(super) fn group(path: &str) -> Option<&str> {
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

#[derive(Default)]
struct Violations {
    bare: Vec<usize>,
    sockets: Vec<usize>,
    fixed_wait_in_tests: BTreeSet<usize>,
    fixed_wait_renames: BTreeSet<usize>,
}

fn inspect_scoped(path: &str, contents: &str, scopes: &TestScopes) -> Result<Violations> {
    if !(scopes.has_test_scope() || contents.contains("FixedWait")) {
        return Ok(Violations::default());
    }
    let source = syn::parse_file(contents).map_err(|error| format!("{path}: {error}"))?;
    let mut visitor = Deadlines {
        emp_app: group(path) == Some("emp-app"),
        pipe_capture: path == "crates/emp-test-support/src/process/capture.rs",
        socket_implementation: path == TEST_SUPPORT_LIB,
        allows_fixed_wait: path == TEST_SUPPORT_LIB,
        scopes,
        imports: Imports::file(path, &source.items),
        violations: Violations::default(),
    };
    visitor.visit_file(&source);
    visitor.violations.bare.sort_unstable();
    visitor.violations.sockets.sort_unstable();
    Ok(visitor.violations)
}

struct Deadlines<'source> {
    emp_app: bool,
    pipe_capture: bool,
    socket_implementation: bool,
    allows_fixed_wait: bool,
    scopes: &'source TestScopes,
    imports: Imports<'source>,
    violations: Violations,
}

impl Deadlines<'_> {
    fn in_test(&self, line: usize) -> bool {
        self.scopes.contains(line)
    }

    fn inspect_fixed_wait_renames(&mut self, tree: &UseTree, parent: Option<&syn::Ident>) {
        match tree {
            UseTree::Path(path) => self.inspect_fixed_wait_renames(&path.tree, Some(&path.ident)),
            UseTree::Group(group) => {
                for item in &group.items {
                    self.inspect_fixed_wait_renames(item, parent);
                }
            }
            UseTree::Rename(rename) => {
                let original = if rename.ident == "self" {
                    parent
                } else {
                    Some(&rename.ident)
                };
                if original.is_some_and(|ident| ident.unraw() == "FixedWait") {
                    self.violations
                        .fixed_wait_renames
                        .insert(rename.ident.span().start().line);
                }
            }
            UseTree::Name(_) | UseTree::Glob(_) => {}
        }
    }

    fn inspect(&mut self, expr: &Expr, constants: bool) {
        let line = expr.span().start().line;
        if self.in_test(line) && bare_duration(expr, constants, self.emp_app, false, &self.imports)
        {
            self.violations.bare.push(line);
        }
    }

    fn inspect_socket_timeout(&mut self, name: &str, line: usize) {
        if (!self.socket_implementation || self.scopes.contains_inline(line))
            && self.in_test(line)
            && (name == "set_read_timeout" || name == "set_write_timeout")
        {
            self.violations.sockets.push(line);
        }
    }

    fn arguments(&mut self, name: &str, args: &syn::punctuated::Punctuated<Expr, syn::Token![,]>) {
        let argument = match name {
            "set_read_timeout"
            | "set_write_timeout"
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
            | "poll_receive_text_with_timeout"
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

fn constant_name(name: &str) -> bool {
    name.bytes().any(|byte| byte.is_ascii_uppercase())
        && !name.bytes().any(|byte| byte.is_ascii_lowercase())
}

fn bare_duration(
    expr: &Expr,
    constants: bool,
    emp_app: bool,
    arithmetic: bool,
    imports: &Imports<'_>,
) -> bool {
    match expr {
        Expr::Paren(paren) => bare_duration(&paren.expr, constants, emp_app, arithmetic, imports),
        Expr::Group(group) => bare_duration(&group.expr, constants, emp_app, arithmetic, imports),
        Expr::MethodCall(call) if call.method == "duration" && call.args.is_empty() => {
            arithmetic && bare_duration(&call.receiver, constants, emp_app, true, imports)
        }
        Expr::Binary(binary) if matches!(binary.op, syn::BinOp::Add(_) | syn::BinOp::Sub(_)) => {
            if starts_at_now(&binary.left, imports) {
                return false;
            }
            bare_duration(&binary.left, true, emp_app, true, imports)
                || bare_duration(&binary.right, true, emp_app, true, imports)
        }
        Expr::Call(call) => {
            let Expr::Path(path) = call.func.as_ref() else {
                return arithmetic;
            };
            let path = imports.resolve(&path.path);
            let mut parts = path.iter().rev().map(String::as_str);
            let Some(name) = parts.next() else {
                return false;
            };
            if name == "Some" {
                return call.args.first().is_some_and(|expr| {
                    bare_duration(expr, constants, emp_app, arithmetic, imports)
                });
            }
            if name == "test_timeout"
                || (emp_app && path == ["crate", "server", "raw_tcp", "wire_timeout"])
            {
                return false;
            }
            arithmetic
                || (matches!(
                    name,
                    "from_secs"
                        | "from_millis"
                        | "from_secs_f64"
                        | "from_micros"
                        | "from_nanos"
                        | "new"
                ) && parts.next() == Some("Duration")
                    && call.args.iter().all(|arg| matches!(arg, Expr::Lit(_))))
        }
        Expr::Path(path) if constants => {
            let path = imports.resolve(&path.path);
            if emp_app && path == ["crate", "server", "REQUEST_DRAIN_TIMEOUT"] {
                return false;
            }
            let name = path.last().expect("expression path");
            arithmetic
                || (!path.iter().rev().nth(1).is_some_and(|part| {
                    part == "Duration" && matches!(name.as_str(), "ZERO" | "MAX")
                }) && constant_name(name))
        }
        Expr::Lit(literal) => matches!(literal.lit, syn::Lit::Int(_) | syn::Lit::Float(_)),
        _ => arithmetic,
    }
}

fn instant_now(expr: &Expr, imports: &Imports<'_>) -> bool {
    matches!(
        expr,
        Expr::Call(call)
            if matches!(call.func.as_ref(), Expr::Path(path) if imports
                .resolve(&path.path).iter().rev().take(2).map(String::as_str).eq(["now", "Instant"]))
    )
}

fn starts_at_now(expr: &Expr, imports: &Imports<'_>) -> bool {
    match expr {
        Expr::Paren(paren) => starts_at_now(&paren.expr, imports),
        Expr::Group(group) => starts_at_now(&group.expr, imports),
        Expr::Binary(binary) if matches!(binary.op, syn::BinOp::Add(_) | syn::BinOp::Sub(_)) => {
            starts_at_now(&binary.left, imports)
        }
        _ => instant_now(expr, imports),
    }
}

fn deadline_name(name: &syn::Ident) -> bool {
    name.to_string()
        .split('_')
        .any(|part| part.eq_ignore_ascii_case("timeout") || part.eq_ignore_ascii_case("deadline"))
}

impl<'ast> Visit<'ast> for Deadlines<'_> {
    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        if module.content.is_some() {
            self.imports.enter_module(module);
            visit::visit_item_mod(self, module);
            self.imports.leave_scope();
        } else {
            visit::visit_item_mod(self, module);
        }
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        self.imports.enter_block(block);
        visit::visit_block(self, block);
        self.imports.leave_scope();
    }

    fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
        visit::visit_macro(self, invocation);
        let expressions = macro_arguments(invocation).map_or_else(
            |_| assertion_expressions(invocation),
            |args| args.into_iter().collect(),
        );
        for expression in &expressions {
            self.visit_expr(expression);
        }
    }

    fn visit_ident(&mut self, ident: &'ast syn::Ident) {
        if self.allows_fixed_wait || ident.unraw() != "FixedWait" {
            return;
        }
        let line = ident.span().start().line;
        if self.in_test(line) {
            self.violations.fixed_wait_in_tests.insert(line);
        }
    }

    fn visit_token_stream(&mut self, tokens: &'ast TokenStream) {
        for token in tokens.clone() {
            match token {
                TokenTree::Ident(ident) => self.visit_ident(&ident),
                TokenTree::Group(group) => self.visit_token_stream(&group.stream()),
                _ => {}
            }
        }
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.inspect_fixed_wait_renames(&item.tree, None);
        visit::visit_item_use(self, item);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(path) = call.func.as_ref()
            && let Some(name) = path.path.segments.last()
        {
            let path = self.imports.resolve(&path.path);
            let resolved = path.last().expect("expression path");
            self.inspect_socket_timeout(resolved, name.ident.span().start().line);
            self.arguments(resolved, &call.args);
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        let name = call.method.to_string();
        self.inspect_socket_timeout(&name, call.method.span().start().line);
        if name == "wait_timeout" || name == "wait_timeout_while" {
            if let Some(expr) = call.args.iter().nth(1)
                && !(self.pipe_capture
                    && name == "wait_timeout"
                    && matches!(expr, Expr::Path(path) if path.path.is_ident("POLL_INTERVAL")))
            {
                self.inspect(expr, true);
            }
        } else {
            self.arguments(&name, &call.args);
        }
        visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_binary(&mut self, expr: &'ast syn::ExprBinary) {
        if matches!(expr.op, syn::BinOp::Add(_) | syn::BinOp::Sub(_))
            && starts_at_now(&expr.left, &self.imports)
        {
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

    fn visit_expr_assign(&mut self, assignment: &'ast syn::ExprAssign) {
        if let Expr::Path(path) = assignment.left.as_ref()
            && path.path.get_ident().is_some_and(deadline_name)
        {
            self.inspect(&assignment.right, false);
        }
        visit::visit_expr_assign(self, assignment);
    }
}

pub(super) fn assertion_expressions(invocation: &syn::Macro) -> Vec<Expr> {
    let Some(name) = invocation.path.segments.last() else {
        return Vec::new();
    };
    let expressions = match name.ident.to_string().as_str() {
        "assert" | "assert_eq" | "assert_ne" | "debug_assert" | "debug_assert_eq"
        | "debug_assert_ne" => macro_arguments(invocation).map(|args| args.into_iter().collect()),
        "matches" | "assert_matches" | "debug_assert_matches" => {
            parse_match_arguments.parse2(invocation.tokens.clone())
        }
        _ => return Vec::new(),
    };
    expressions.unwrap_or_default()
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

    fn inspect_source(path: &str, contents: &str) -> Result<Violations> {
        inspect_scoped(
            path,
            contents,
            &super::super::test_scope::tests::fixture(path, contents),
        )
    }

    fn violations(path: &str, source: &str) -> Result<Vec<usize>> {
        Ok(inspect_source(path, source)?.bare)
    }

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
        receiver.recv_timeout(test_timeout(Duration::from_secs(5)).duration());
        let deadline = Instant::now() + test_timeout(Duration::from_secs(5)).duration();
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
    rx.recv_timeout(crate::server::REQUEST_DRAIN_TIMEOUT + test_timeout(Duration::from_secs(5)).duration());
    rx.recv_timeout(crate::server::REQUEST_DRAIN_TIMEOUT + Duration::from_secs(5));
    rx.recv_timeout(FIXTURE_WAIT + test_timeout(Duration::from_secs(5)).duration());
    rx.recv_timeout(test_timeout(Duration::from_secs(5)).duration() - FIXTURE_WAIT);
    rx.recv_timeout(crate::server::REQUEST_DRAIN_TIMEOUT - test_timeout(Duration::from_secs(5)).duration());
    rx.recv_timeout(test_timeout(Duration::from_secs(5)).duration() + (FIXTURE_WAIT - test_timeout(Duration::from_secs(1)).duration()));
    rx.recv_timeout((base_budget + extra_budget).duration());
}
"#;
        assert_eq!(
            violations("crates/emp-app/tests/wire.rs", source).unwrap(),
            [6, 7, 8, 10]
        );
    }

    #[test]
    fn compares_imported_and_qualified_deadlines_by_path() {
        let source = r#"
use crate::server::{REQUEST_DRAIN_TIMEOUT as DRAIN, raw_tcp as wire};
use wire::wire_timeout;
fn drain() {
    timeout(wire_timeout().duration(), future);
    timeout(crate::server::raw_tcp::wire_timeout().duration(), future);
    timeout(DRAIN + wire_timeout().duration(), future);
    timeout(crate::server::REQUEST_DRAIN_TIMEOUT + crate::server::raw_tcp::wire_timeout().duration(), future);
    {
        use crate::fixture::wire_timeout;
        timeout(DRAIN + wire_timeout().duration(), future);
    }
    timeout(DRAIN + wire_timeout().duration(), future);
}
"#;
        assert_eq!(
            violations("crates/emp-app/tests/wire.rs", source).unwrap(),
            [11]
        );
    }

    #[test]
    fn resolves_relative_deadline_aliases_at_the_import() {
        let source = r#"
use std::time as time;
use self::time::Duration as Wait;
mod nested {
    use super::Wait as ParentWait;
    fn fixture() {
        use other::time;
        use self::ParentWait as LocalWait;
        rx.recv_timeout(LocalWait::from_secs(1));
        rx.recv_timeout(time::OtherWait::from_secs(1));
    }
}
"#;
        assert_eq!(
            violations("crates/example/tests/wire.rs", source).unwrap(),
            [9]
        );
    }

    #[test]
    fn checks_fixed_wait_tokens_and_import_renames() {
        let source = r#"
mod parent {
    use emp_domain::FixedWait as Wait;
    #[cfg(test)]
    mod tests {
        use super::*;
        fn fixture() { budget.with_product_wait(Wait::new(margin)); }
    }
}
#[cfg(test)] use emp_domain::FixedWait as Wait;
#[cfg(not(test))] use std::time::Duration as Wait;
#[test]
fn fixture() {
    budget.with_product_wait(Wait::new(margin));
    budget.with_product_wait(<emp_domain::FixedWait>::new(margin));
    wire! { wait => [other::FixedWait] }
    budget.with_product_wait(NETWORK_RETRY_BACKOFF[0]);
    log!("FixedWait");
}
"#;
        let violations = inspect_source("crates/example/src/lib.rs", source).unwrap();
        assert_eq!(violations.fixed_wait_in_tests, BTreeSet::from([10, 15, 16]));
        assert_eq!(violations.fixed_wait_renames, BTreeSet::from([3, 10]));
        assert!(violations.bare.is_empty());
        assert!(violations.sockets.is_empty());

        let violations = inspect_source(
            "crates/example/src/lib.rs",
            "use emp_domain::{r#FixedWait as Wait};",
        )
        .unwrap();
        assert!(violations.fixed_wait_in_tests.is_empty());
        assert_eq!(violations.fixed_wait_renames, BTreeSet::from([1]));

        let violations = inspect_source(
            "crates/example/tests/wire.rs",
            "use emp_domain::FixedWait;\nfn fixture(_: FixedWait) {}",
        )
        .unwrap();
        assert_eq!(violations.fixed_wait_in_tests, BTreeSet::from([1, 2]));
        assert!(violations.fixed_wait_renames.is_empty());
    }

    #[test]
    fn rejects_fixed_wait_self_rename_outside_test_scope() {
        let source = r#"use emp_domain::FixedWait::{self as Wait};
#[test]
fn fixture() {
    budget.with_product_wait(Wait::new(margin));
}
"#;
        let violations = inspect_source("crates/example/src/lib.rs", source).unwrap();
        assert!(violations.fixed_wait_in_tests.is_empty());
        assert_eq!(violations.fixed_wait_renames, BTreeSet::from([1]));
    }

    #[test]
    fn exempts_only_test_budget_file_from_fixed_wait_token_rule() {
        let source = r#"use emp_domain::{Dialect, FixedWait};
impl TestBudget {
    fn with_product_wait(self, wait: emp_domain::FixedWait) -> Self { self }
}
"#;
        let support = inspect_source(TEST_SUPPORT_LIB, source).unwrap();
        assert!(support.fixed_wait_in_tests.is_empty());
        assert!(support.fixed_wait_renames.is_empty());

        let other = inspect_source("crates/emp-test-support/src/fixture.rs", source).unwrap();
        assert_eq!(other.fixed_wait_in_tests, BTreeSet::from([1, 3]));

        let renamed =
            inspect_source(TEST_SUPPORT_LIB, "use emp_domain::{FixedWait as Wait};").unwrap();
        assert!(renamed.fixed_wait_in_tests.is_empty());
        assert_eq!(renamed.fixed_wait_renames, BTreeSet::from([1]));
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
    fn checks_every_offset_from_instant_now() {
        let source = r#"
#[test]
fn deadlines() {
    timeout_at(Instant::now() + test_timeout(margin).duration() + FIXTURE_WAIT, work);
    let deadline = (Instant::now() - FIXTURE_WAIT) + test_timeout(margin).duration() - OTHER_WAIT;
    let deadline = Instant::now() + test_timeout(margin).duration() - test_timeout(margin).duration();
}
"#;
        assert_eq!(
            violations("crates/emp-app/src/lib.rs", source).unwrap(),
            [4, 5, 5]
        );
    }

    #[test]
    fn checks_the_explicit_websocket_poll_timeout() {
        let source = r#"
fn poll() {
    socket.poll_receive_text_with_timeout(Duration::from_secs(5));
    ClientWebSocket::poll_receive_text_with_timeout(&mut socket, Duration::from_secs(5));
    socket.poll_receive_text_with_timeout(test_timeout(Duration::from_secs(5)).duration());
    socket.poll_receive_text_with_timeout(deadline.remaining());
}
"#;
        assert_eq!(
            violations("crates/emp-app/tests/wire.rs", source).unwrap(),
            [3, 4]
        );
    }

    #[test]
    fn socket_timeout_rule_checks_methods_and_qualified_calls_in_tests() {
        let source = r#"
fn production() { socket.set_read_timeout(Some(timeout)); }
#[cfg(test)]
mod tests {
    fn fixture() {
        socket.set_read_timeout(Some(test_timeout(limit).duration()));
        TcpStream::set_write_timeout(&socket, Some(limit));
        assert!(socket.set_write_timeout(None).is_ok());
        let reader = Deadline::after(test_timeout(limit)).io(socket);
    }
}
"#;
        for (path, expected) in [
            ("crates/app/src/lib.rs", &[6, 7, 8][..]),
            ("crates/app/tests/wire.rs", &[2, 6, 7, 8][..]),
            (
                "crates/emp-test-support/src/fixture/wire.rs",
                &[2, 6, 7, 8][..],
            ),
        ] {
            assert_eq!(inspect_source(path, source).unwrap().sockets, expected);
        }
    }

    #[test]
    fn baseline_requires_an_exact_match() {
        let mut errors = Vec::new();
        check_baseline(
            "test-socket-timeouts",
            "sample",
            &[],
            1,
            "调用",
            "使用 Deadline::io",
            &mut errors,
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("在同一提交将基线调低至 0"));

        errors.clear();
        let locations = ["tests/wire.rs:42".to_owned()];
        check_baseline(
            "test-socket-timeouts",
            "sample",
            &locations,
            1,
            "调用",
            "使用 Deadline::io",
            &mut errors,
        );
        assert!(errors.is_empty());
        check_baseline(
            "test-socket-timeouts",
            "sample",
            &locations,
            0,
            "调用",
            "使用 Deadline::io",
            &mut errors,
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("tests/wire.rs:42"));
    }
}
