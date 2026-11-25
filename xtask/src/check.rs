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
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use std::fs;
use std::path::Path;
use std::process::Command;
use syn::ext::IdentExt as _;
use syn::{Expr, Macro, Token, parse::Parser as _, punctuated::Punctuated};

pub fn run(args: &[String]) -> Result {
    if !args.is_empty() {
        return Err("usage: cargo xtask check".into());
    }
    let root = project_root();
    let mut errors = Vec::new();
    let (test_scopes, app_tests) = arch::check(&root, &mut errors)?;
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
            module_paths::check_conditional(&path, source, &mut errors)?;
            production_sockets::check(&path, source, scopes, &mut errors)?;
            test_drop::check(&path, source, scopes, &mut errors)?;
            shared_test_module(&path, source, &mut errors)?;
        }
        test_boundary(&path, &bytes, scopes.integration, &mut errors);
        if prose::includes(&path) {
            prose::check(&path, &read(&root, &path)?, &mut errors);
        } else if path.starts_with(".github/workflows/")
            && (path.ends_with(".yml") || path.ends_with(".yaml"))
        {
            let contents = read(&root, &path)?;
            workflows::check(&path, &contents, &app_tests, &mut errors);
        }
    }
    if errors.is_empty() {
        println!(
            "check: arch, module-paths, conditional-module-path, module-version, test-boundary, docs-prose, no-python-on-main, workflow-contexts, workflow-test-targets, comments, cfg-test, disguised-sleep, msvc-only, domain-purity, single-http-driver, production-sockets, test-deadlines, test-socket-timeouts, test-fixture-root, test-thread-observations, test-drop, shared-test-module passed"
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

fn test_boundary(path: &str, bytes: &[u8], integration: bool, errors: &mut Vec<String>) {
    let in_tests_directory = path.starts_with("crates/") && path.split('/').nth(2) == Some("tests");
    if !in_tests_directory && !integration {
        return;
    }
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

    let tokens = source
        .parse::<TokenStream>()
        .map_err(|error| format!("{path}: {error}"))?;
    check_shared_test_tokens(path, tokens, errors);
    Ok(())
}

fn check_shared_test_tokens(path: &str, tokens: TokenStream, errors: &mut Vec<String>) {
    let tokens: Vec<_> = tokens.into_iter().collect();
    for (index, token) in tokens.iter().enumerate() {
        let restriction = match &tokens[index..] {
            [TokenTree::Punct(hash), rest @ ..] if hash.as_char() == '#' => {
                let rest = match rest {
                    [TokenTree::Punct(bang), rest @ ..] if bang.as_char() == '!' => rest,
                    _ => rest,
                };
                let [TokenTree::Group(attribute), ..] = rest else {
                    continue;
                };
                if attribute.delimiter() != Delimiter::Bracket
                    || !contains_test_ident(attribute.stream())
                {
                    continue;
                }
                "属性中禁止出现 test 标识符"
            }
            [
                TokenTree::Ident(name),
                TokenTree::Punct(bang),
                TokenTree::Ident(_),
                TokenTree::Group(_),
                ..,
            ] if name.unraw() == "macro_rules" && bang.as_char() == '!' => {
                "禁止定义 macro_rules! 宏"
            }
            [TokenTree::Group(group), ..] => {
                check_shared_test_tokens(path, group.stream(), errors);
                continue;
            }
            _ => continue,
        };
        let line = token.span().start().line;
        errors.push(format!(
            "[shared-test-module] {path}:{line}: {restriction}；请将用例移到独立的集成测试目标，辅助逻辑写成函数"
        ));
    }
}

fn contains_test_ident(tokens: TokenStream) -> bool {
    tokens.into_iter().any(|token| match token {
        TokenTree::Ident(name) => name.unraw() == "test",
        TokenTree::Group(group) => contains_test_ident(group.stream()),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_conditional_module_paths() {
        let source = r#"
#[cfg_attr(all(not(test), unix), path = "unix.rs")]
#[cfg_attr(all(not(test), not(unix)), path = "other.rs")]
mod backend;
#[cfg_attr(feature = "test-support", allow(dead_code), path = "fixture.rs")]
mod transport;
#[cfg(test)]
mod fixtures {
    #[cfg_attr(unix, cfg_attr(test, path = "platform_tests.rs"))]
    mod platform;
    macro_rules! declare {
        ($predicate:meta, $attribute:meta, $file:literal) => {
            #[cfg_attr($predicate, $attribute, path = $file)]
            pub(crate) mod generated;
        };
    }
}
#[cfg(unix)] #[path = "native.rs"] mod native;
#[cfg(not(unix))] #[path = "portable.rs"] mod native;
#[cfg_attr(test, allow(dead_code))] mod support;
"#;
        let mut errors = Vec::new();
        module_paths::check_conditional("lib.rs", source, &mut errors).unwrap();
        assert_eq!(errors.len(), 5, "{errors:?}");
        for (error, line) in errors.iter().zip([2, 3, 5, 9, 13]) {
            assert!(
                error.starts_with(&format!("[conditional-module-path] lib.rs:{line}:")),
                "{error}"
            );
        }
    }

    #[test]
    fn rejects_test_attributes_only_in_shared_integration_modules() {
        let source = r##"#[test]
fn sync_case() {}
mod nested {
    #[tokio::test(flavor = "current_thread")]
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
    fn rejects_test_tokens_and_macro_definitions_in_shared_modules() {
        let source = r#"#![cfg(test)]
macro_rules! cases {
    (#[cfg_attr($p:meta, test)] $item:item) => {
        #[cfg_attr($p, $extra, test)]
        $item
    };
}
macro_rules! helper { () => {} }
#[cfg_attr(unix, allow(dead_code), cfg(any(unix, r#test)))]
fn gated() {}
"#;
        let path = "crates/example/tests/support/cases.rs";
        let mut errors = Vec::new();
        shared_test_module(path, source, &mut errors).unwrap();
        assert_eq!(errors.len(), 6, "{errors:?}");
        for (error, line) in errors.iter().zip([1, 2, 3, 4, 8, 9]) {
            assert!(
                error.starts_with(&format!("[shared-test-module] {path}:{line}:")),
                "{error}"
            );
        }
    }

    #[test]
    fn allows_helper_functions_and_non_test_attributes_in_shared_modules() {
        let source = r##"#[allow(dead_code)]
#[cfg(feature = "test")]
fn test() {
    trace!("#[test]");
}
"##;
        let path = "crates/example/tests/support/cases.rs";
        let mut errors = Vec::new();
        shared_test_module(path, source, &mut errors).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn test_boundary_checks_binary_and_http_access() {
        let cases: &[(&[u8], usize)] = &[
            (b"Command::new(binary); http.get(\"/api/config\");", 0),
            (b"let state: ServerState;\nemp_app::VERSION;", 2),
        ];
        for &(contents, count) in cases {
            let mut errors = Vec::new();
            test_boundary("crates/emp-app/src/e2e.rs", contents, true, &mut errors);
            assert_eq!(errors.len(), count);
            if count > 0 {
                assert!(errors[0].contains("e2e.rs:1: ServerState"));
                assert!(errors[1].contains("e2e.rs:2: emp_app::"));
            }
        }
    }

    #[test]
    fn test_boundary_checks_node_contracts_in_the_tests_directory() {
        let path = "crates/emp-app/tests/frontend_contract.cjs";
        let source = br#"const backend = readFileSync("../src/server.rs", "utf8");
assert.match(backend, /ServerState/);"#;
        let mut errors = Vec::new();
        test_boundary(path, source, false, &mut errors);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains(&format!("[test-boundary] {path}:2: ServerState")));
    }
}
