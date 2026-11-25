use super::{
    Path, Result, comments, read,
    test_deadlines::{assertion_expressions, check_baseline, group},
};
use std::collections::BTreeMap;
use syn::{
    spanned::Spanned as _,
    visit::{self, Visit},
};

const ALLOWED: &[(&str, usize)] = &[("emp-app", 32), ("emp-transport", 6)];

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
        let lines = inspect_source(path, &read(root, path)?)?;
        counts
            .entry(group)
            .or_default()
            .extend(lines.into_iter().map(|line| format!("{path}:{line}")));
    }
    for (group, locations) in counts {
        let allowed = ALLOWED
            .iter()
            .find(|&&(name, _)| name == group)
            .map_or(0, |&(_, count)| count);
        check_baseline(
            "test-thread-observations",
            group,
            &locations,
            allowed,
            "同一函数中手写线程与通道接收",
            "使用 emp_test_support::Observation",
            errors,
        );
    }
    Ok(())
}

fn inspect_source(path: &str, contents: &str) -> Result<Vec<usize>> {
    if !(contents.contains("spawn") && contents.contains("recv")) {
        return Ok(Vec::new());
    }
    let test_file = comments::test_file(path);
    if !(test_file || (contents.contains("test") && contents.contains("#["))) {
        return Ok(Vec::new());
    }
    let source = syn::parse_file(contents).map_err(|error| format!("{path}: {error}"))?;
    let mut scopes = comments::TestScopes::default();
    if !test_file {
        scopes.visit_file(&source);
    }
    let mut visitor = TestFunctions {
        test_file,
        scopes,
        lines: Vec::new(),
    };
    visitor.visit_file(&source);
    Ok(visitor.lines)
}

struct TestFunctions {
    test_file: bool,
    scopes: comments::TestScopes,
    lines: Vec<usize>,
}

impl TestFunctions {
    fn inspect(&mut self, block: &syn::Block) {
        if self.test_file || self.scopes.contains(block.span().start().line) {
            let mut body = ThreadBody::default();
            body.visit_block(block);
            if body.receives {
                self.lines.extend(body.spawns);
            }
        }
    }
}

impl<'ast> Visit<'ast> for TestFunctions {
    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        self.inspect(&function.block);
        visit::visit_item_fn(self, function);
    }

    fn visit_impl_item_fn(&mut self, function: &'ast syn::ImplItemFn) {
        self.inspect(&function.block);
        visit::visit_impl_item_fn(self, function);
    }

    fn visit_trait_item_fn(&mut self, function: &'ast syn::TraitItemFn) {
        if let Some(block) = &function.default {
            self.inspect(block);
        }
        visit::visit_trait_item_fn(self, function);
    }
}

#[derive(Default)]
struct ThreadBody {
    spawns: Vec<usize>,
    receives: bool,
}

fn receive(name: &syn::Ident) -> bool {
    matches!(
        name.to_string().as_str(),
        "recv" | "recv_timeout" | "recv_deadline" | "try_recv"
    )
}

impl<'ast> Visit<'ast> for ThreadBody {
    fn visit_item(&mut self, _: &'ast syn::Item) {}

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(expr) = call.func.as_ref() {
            let segments = &expr.path.segments;
            let names = segments.iter().map(|segment| &segment.ident);
            if names.clone().eq(["thread", "spawn"]) || names.eq(["std", "thread", "spawn"]) {
                self.spawns.push(call.span().start().line);
            }
            if let Some(name) = segments.last()
                && segments
                    .iter()
                    .rev()
                    .nth(1)
                    .is_some_and(|part| part.ident == "Receiver")
            {
                self.receives |= receive(&name.ident);
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.receives |= receive(&call.method);
        visit::visit_expr_method_call(self, call);
    }

    fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
        for expression in &assertion_expressions(invocation) {
            self.visit_expr(expression);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_manual_observation_in_one_test_function() {
        let source = r#"
fn production() { thread::spawn(job); rx.recv(); }
#[cfg(test)]
mod tests {
    fn manual() {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || tx.send(result));
        assert_eq!(rx.recv_timeout(deadline.remaining()).unwrap(), expected);
    }
    fn qualified() {
        thread::spawn(job);
        Receiver::recv(&rx);
    }
    fn managed() {
        let worker = Observation::spawn(job);
        worker.begin(deadline, "result").wait();
    }
    fn separate_functions() {
        thread::spawn(job);
        fn receive_only() { rx.recv(); }
    }
}
"#;
        assert_eq!(
            inspect_source("crates/sample/src/lib.rs", source).unwrap(),
            [7, 11]
        );
        assert_eq!(
            inspect_source("crates/sample/tests/wire.rs", source).unwrap(),
            [2, 7, 11]
        );
    }

    #[test]
    fn counts_each_spawn_in_a_function_with_manual_observations() {
        let source = r#"
fn manual() {
    let (first_tx, first_rx) = mpsc::channel();
    let (second_tx, second_rx) = mpsc::channel();
    std::thread::spawn(move || first_tx.send(first));
    thread::spawn(move || second_tx.send(second));
    assert_eq!(first_rx.recv().unwrap(), expected_first);
    assert_eq!(Receiver::recv(&second_rx).unwrap(), expected_second);
}
"#;
        assert_eq!(
            inspect_source("crates/sample/tests/wire.rs", source).unwrap(),
            [5, 6]
        );
    }
}
