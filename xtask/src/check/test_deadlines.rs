use super::{Path, Result, comments, read};
use std::collections::BTreeMap;
use syn::{
    Expr, Pat,
    spanned::Spanned,
    visit::{self, Visit},
};

const ALLOWED: &[(&str, usize)] = &[
    ("emp-app", 139),
    ("emp-codex", 5),
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
        if locations.len() > allowed {
            errors.push(format!("[test-deadlines] {group}: 当前 {} 处裸写期限，基线 {allowed}；使用 test_timeout，计数棘轮只降不升\n{}", locations.len(), locations.join("\n")));
        } else if locations.len() < allowed {
            println!(
                "[test-deadlines] {group}: 请将期限基线从 {allowed} 调低至 {}",
                locations.len()
            );
        }
    }
    Ok(())
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
    let test_file = comments::test_file(path);
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
        scopes,
        lines: Vec::new(),
    };
    visitor.visit_file(&source);
    visitor.lines.sort_unstable();
    Ok(visitor.lines)
}

struct Deadlines {
    test_file: bool,
    scopes: comments::TestScopes,
    lines: Vec<usize>,
}

impl Deadlines {
    fn inspect(&mut self, expr: &Expr, constants: bool) {
        let line = expr.span().start().line;
        if (self.test_file || self.scopes.contains(line)) && bare_duration(expr, constants) {
            self.lines.push(line);
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
            | "wait_for_state"
            | "wait_for_error"
            | "probe_candidate_version"
            | "quiesce"
            | "wait_for_websocket_release" => args.last(),
            "probe_candidate_version_observed" => args.iter().nth(3),
            "wait_for_candidate_version" => args.iter().nth(1),
            _ => None,
        };
        if let Some(expr) = argument {
            self.inspect(expr, true);
        }
    }
}

fn bare_duration(expr: &Expr, constants: bool) -> bool {
    match expr {
        Expr::Paren(paren) => bare_duration(&paren.expr, constants),
        Expr::Call(call) => {
            let Expr::Path(path) = call.func.as_ref() else {
                return false;
            };
            let mut parts = path.path.segments.iter().rev();
            let Some(name) = parts.next() else {
                return false;
            };
            if name.ident == "Some" {
                return call
                    .args
                    .first()
                    .is_some_and(|expr| bare_duration(expr, constants));
            }
            matches!(
                name.ident.to_string().as_str(),
                "from_secs"
                    | "from_millis"
                    | "from_secs_f64"
                    | "from_micros"
                    | "from_nanos"
                    | "new"
            ) && parts.next().is_some_and(|part| part.ident == "Duration")
                && call.args.iter().all(|arg| matches!(arg, Expr::Lit(_)))
        }
        Expr::Path(path) if constants => {
            let name = path
                .path
                .segments
                .last()
                .expect("expression path")
                .ident
                .to_string();
            !matches!(name.as_str(), "ZERO" | "MAX")
                && name.bytes().any(|byte| byte.is_ascii_uppercase())
                && !name.bytes().any(|byte| byte.is_ascii_lowercase())
        }
        _ => false,
    }
}

fn deadline_name(name: &syn::Ident) -> bool {
    name.to_string()
        .split('_')
        .any(|part| part.eq_ignore_ascii_case("timeout") || part.eq_ignore_ascii_case("deadline"))
}

impl<'ast> Visit<'ast> for Deadlines {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(path) = call.func.as_ref()
            && let Some(name) = path.path.segments.last()
        {
            self.arguments(&name.ident.to_string(), &call.args);
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
        if matches!(expr.op, syn::BinOp::Add(_))
            && let Expr::Call(call) = expr.left.as_ref()
            && let Expr::Path(path) = call.func.as_ref()
            && path
                .path
                .segments
                .iter()
                .rev()
                .take(2)
                .map(|part| &part.ident)
                .eq(["now", "Instant"])
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
}
