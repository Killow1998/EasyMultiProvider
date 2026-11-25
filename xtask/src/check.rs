mod arch;
mod cfg_test;
mod comments;
mod imports;
mod module_paths;
mod modules;
mod production_sockets;
mod prose;
mod test_deadlines;
mod test_drop;
mod test_fixture_root;
mod test_scope;
mod test_thread_observations;
mod workflows;

use crate::{Result, project_root};
use std::fs;
use std::path::Path;
use std::process::Command;
use syn::spanned::Spanned as _;
use syn::visit::Visit;
use syn::{Expr, Macro, Token, parse::Parser as _, punctuated::Punctuated};

pub fn run(args: &[String]) -> Result {
    if !args.is_empty() {
        return Err("usage: cargo xtask check".into());
    }
    let root = project_root();
    let mut errors = Vec::new();
    let test_scopes = arch::check(&root, &mut errors)?;
    modules::check(&root, &mut errors)?;
    let list_files = [
        "ls-files",
        "--cached",
        "--others",
        "--exclude-standard",
        "-z",
    ];
    let paths = git_paths(&root, &list_files)?;
    comments::check(&root, &paths, &test_scopes, &mut errors)?;
    cfg_test::check(&root, &paths, &test_scopes, &mut errors)?;
    test_deadlines::check(&root, &paths, &test_scopes, &mut errors)?;
    test_fixture_root::check(&root, &paths, &test_scopes, &mut errors)?;
    test_thread_observations::check(&root, &paths, &test_scopes, &mut errors)?;
    for path in paths {
        if !root.join(&path).is_file() {
            continue;
        }
        let bytes = fs::read(root.join(&path)).map_err(|error| format!("{path}: {error}"))?;
        let scopes = test_scopes.get(&path);
        if let Ok(source) = std::str::from_utf8(&bytes) {
            single_http_driver(&path, source, &mut errors);
            production_sockets::check(&path, source, scopes, &mut errors)?;
            test_drop::check(&path, source, scopes, &mut errors)?;
            shared_test_module(&path, source, &mut errors)?;
        }
        if path.starts_with("crates/") && scopes.integration {
            test_boundary(&path, &bytes, &mut errors);
        } else if prose::includes(&path) {
            prose::check(&path, &read(&root, &path)?, &mut errors);
        } else if path.starts_with(".github/workflows/") && path.ends_with(".yml") {
            workflows::check(&path, &read(&root, &path)?, &mut errors);
        }
    }
    if errors.is_empty() {
        println!(
            "check: arch, module-paths, module-version, test-boundary, docs-prose, no-python-on-main, comments, cfg-test, disguised-sleep, msvc-only, domain-purity, single-http-driver, production-sockets, test-deadlines, test-socket-timeouts, test-fixture-root, test-thread-observations, test-drop, shared-test-module passed"
        );
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn single_http_driver(path: &str, source: &str, errors: &mut Vec<String>) {
    const RETIRED: &[&str] = &[
        concat!("server-", "candidate"),
        concat!("EMP_TEST_HTTP_", "DRIVER"),
        concat!("legacy_", "reply"),
        concat!("wire_", "response"),
        concat!("serve_", "legacy"),
        concat!("legacy_", "websocket_io"),
    ];
    for (index, line) in source.lines().enumerate() {
        for name in RETIRED.iter().filter(|name| line.contains(**name)) {
            errors.push(format!(
                "[single-http-driver] {path}:{}: {name} 已退役；使用正式的 Axum 入口",
                index + 1
            ));
        }
    }
}

fn macro_arguments(invocation: &Macro) -> syn::Result<Punctuated<Expr, Token![,]>> {
    Punctuated::parse_terminated.parse2(invocation.tokens.clone())
}

fn read(root: &Path, path: &str) -> Result<String> {
    fs::read_to_string(root.join(path)).map_err(|error| format!("{path}: {error}"))
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|error| format!("git {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| format!("git output must be UTF-8: {error}"))
}

fn git_paths(root: &Path, args: &[&str]) -> Result<Vec<String>> {
    let mut paths = git(root, args)?
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn test_boundary(path: &str, bytes: &[u8], errors: &mut Vec<String>) {
    for (index, line) in String::from_utf8_lossy(bytes).lines().enumerate() {
        if let Some(forbidden) = ["ServerState", "emp_app::"]
            .into_iter()
            .find(|word| line.contains(*word))
        {
            errors.push(format!("[test-boundary] {path}:{}: {forbidden} 跨越黑盒边界；通过 EMP 二进制和 HTTP 观察行为，预期版本使用 env!(\"CARGO_PKG_VERSION\")", index + 1));
        }
    }
}

fn shared_test_module(path: &str, source: &str, errors: &mut Vec<String>) -> Result {
    if !path.ends_with(".rs")
        || !matches!(
            path.splitn(5, '/').collect::<Vec<_>>().as_slice(),
            ["crates", _, "tests", "support", _]
        )
    {
        return Ok(());
    }

    struct TestAttributes<'a> {
        file: &'a str,
        errors: &'a mut Vec<String>,
    }

    impl<'ast> Visit<'ast> for TestAttributes<'_> {
        fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
            let path = attribute.path();
            if path.is_ident("test")
                || (path.segments.len() == 2
                    && path.segments[0].ident == "tokio"
                    && path.segments[1].ident == "test")
            {
                let file = self.file;
                let line = attribute.span().start().line;
                self.errors.push(format!(
                    "[shared-test-module] {file}:{line}: 共享测试辅助模块不能注册用例；请将用例移到独立的集成测试目标"
                ));
            }
        }
    }

    let file = syn::parse_file(source).map_err(|error| format!("{path}: {error}"))?;
    TestAttributes { file: path, errors }.visit_file(&file);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_test_attributes_only_in_shared_integration_modules() {
        let source = r##"#[test]
fn sync_case() {}
mod nested {
    #[::tokio::test(flavor = "current_thread")]
    async fn async_case() {}
}
const TEXT: &str = "#[test]";
"##;
        let mut errors = Vec::new();
        for path in [
            "crates/example/tests/suite.rs",
            "crates/example/tests/support.rs",
        ] {
            shared_test_module(path, source, &mut errors).unwrap();
        }
        assert!(errors.is_empty());

        let path = "crates/example/tests/support/nested/wire.rs";
        shared_test_module(path, source, &mut errors).unwrap();
        assert_eq!(errors.len(), 2);
        assert!(errors[0].starts_with(&format!("[shared-test-module] {path}:1:")));
        assert!(errors[1].starts_with(&format!("[shared-test-module] {path}:4:")));
    }

    #[test]
    fn test_boundary_checks_binary_and_http_access() {
        let cases: &[(&[u8], usize)] = &[
            (b"Command::new(binary); http.get(\"/api/config\");", 0),
            (b"let state: ServerState;\nemp_app::VERSION;", 2),
        ];
        for &(contents, count) in cases {
            let mut errors = Vec::new();
            test_boundary("crates/emp-app/tests/e2e.rs", contents, &mut errors);
            assert_eq!(errors.len(), count);
            if count > 0 {
                assert!(errors[0].contains("e2e.rs:1: ServerState"));
                assert!(errors[1].contains("e2e.rs:2: emp_app::"));
            }
        }
    }
}
