mod arch;
mod cfg_test;
mod ci_profiles;
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

use crate::{Result, ci::PROFILES, project_root};
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::ToTokens as _;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;
use syn::ext::IdentExt as _;
use syn::{Expr, Macro, Token, parse::Parser as _, punctuated::Punctuated};

const RULES: &[&str] = &[
    "arch",
    "cfg-test",
    "ci-profiles",
    "comments",
    "conditional-module-path",
    "disguised-sleep",
    "docs-prose",
    "domain-purity",
    "module-path-extension",
    "module-paths",
    "module-version",
    "msvc-only",
    "no-mod-in-macro",
    "no-python-on-main",
    "no-source-include",
    "production-sockets",
    "rust-extension-case",
    "shared-test-module",
    "single-http-driver",
    "test-boundary",
    "test-deadlines",
    "test-drop",
    "test-fixture-root",
    "test-socket-timeouts",
    "test-thread-observations",
    "workflow-cargo-test",
    "workflow-location",
];

pub fn run(args: &[String]) -> Result {
    if !args.is_empty() {
        return Err("usage: cargo xtask check".into());
    }
    let root = project_root();
    let mut errors = Vec::new();
    let (test_scopes, test_targets) = arch::check(&root, &mut errors)?;
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
    let referenced_profiles = check_files(&root, &paths, &test_scopes, &mut errors)?;
    ci_profiles::check_profiles(PROFILES, &test_targets, &referenced_profiles, &mut errors);
    if errors.is_empty() {
        println!("check: {} passed", RULES.join(", "));
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn check_files(
    root: &Path,
    paths: &[String],
    test_scopes: &test_scope::Index,
    errors: &mut Vec<String>,
) -> Result<BTreeSet<&'static str>> {
    let mut referenced_profiles = BTreeSet::new();
    for path in paths {
        let file = root.join(path);
        if !file.is_file() {
            continue;
        }
        let bytes = fs::read(file).map_err(|error| format!("{path}: {error}"))?;
        let source = std::str::from_utf8(&bytes);
        match path.rsplit_once('.') {
            Some((_, "rs")) => module_paths::check_source(
                path,
                source.map_err(|error| format!("{path}: {error}"))?,
                errors,
            )?,
            Some((_, extension)) if extension.eq_ignore_ascii_case("rs") => errors.push(format!(
                "[rust-extension-case] {path}: Rust 源文件后缀必须是小写 .rs"
            )),
            _ => {}
        }
        let scopes = test_scopes.get(path);
        if let Ok(source) = source {
            single_http_driver(path, source, errors);
            production_sockets::check(path, source, scopes, errors)?;
            test_drop::check(path, source, scopes, errors)?;
            shared_test_module(path, source, errors)?;
        }
        test_boundary(path, &bytes, scopes.integration, errors);
        if prose::includes(path) {
            prose::check(path, &read(root, path)?, errors);
        } else if let Some(file) = path.strip_prefix(".github/workflows/")
            && (file.ends_with(".yml") || file.ends_with(".yaml"))
        {
            if file.contains('/') {
                errors.push(format!(
                    "[workflow-location] {path}: 请将子目录中的 YAML 移出 .github/workflows/"
                ));
                continue;
            }
            let contents = read(root, path)?;
            workflows::check(path, &contents, errors);
            ci_profiles::check_references(
                path,
                &contents,
                PROFILES,
                &mut referenced_profiles,
                errors,
            );
        }
    }
    Ok(referenced_profiles)
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

fn visit_macro_attributes(tokens: TokenStream, visit: &mut impl FnMut(TokenStream, usize)) {
    let mut tokens = tokens.into_iter().peekable();
    while let Some(token) = tokens.next() {
        match token {
            TokenTree::Punct(punct) if punct.as_char() == '#' => {
                tokens.next_if(
                    |token| matches!(token, TokenTree::Punct(punct) if punct.as_char() == '!'),
                );
                if let Some(TokenTree::Group(group)) =
                    tokens.next_if(|token| matches!(token, TokenTree::Group(group) if group.delimiter() == Delimiter::Bracket))
                {
                    visit(group.stream(), punct.span().start().line);
                }
            }
            TokenTree::Group(group) => visit_macro_attributes(group.stream(), visit),
            _ => {}
        }
    }
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

    let file = syn::parse_file(source).map_err(|error| format!("{path}: {error}"))?;
    check_shared_test_tokens(path, file.into_token_stream(), errors);
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
                    || !contains_ident(attribute.stream(), "test")
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

fn contains_ident(tokens: TokenStream, expected: &str) -> bool {
    tokens.into_iter().any(|token| match token {
        TokenTree::Ident(name) => name.unraw() == expected,
        TokenTree::Group(group) => contains_ident(group.stream(), expected),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_uppercase_rust_extensions() {
        let directory = emp_test_support::fixture_dir().unwrap();
        let root = directory.path();
        let paths = ["helpers.RS", "lower/helpers.rs", "notes.rst"].map(str::to_owned);
        fs::create_dir(root.join("lower")).unwrap();
        for path in &paths {
            fs::write(root.join(path), "").unwrap();
        }
        let mut errors = Vec::new();
        check_files(root, &paths, &test_scope::Index::default(), &mut errors).unwrap();
        assert_eq!(
            errors,
            ["[rust-extension-case] helpers.RS: Rust 源文件后缀必须是小写 .rs"]
        );
    }

    #[test]
    fn rule_table_matches_diagnostic_prefixes() {
        let prefix = regex::Regex::new(r#""\[([a-z][a-z0-9-]*)\](?: |")"#).unwrap();
        let source = project_root().join("xtask/src");
        let mut paths = vec![source.join("check.rs"), source.join("check")];
        let mut rules = BTreeSet::new();
        while let Some(path) = paths.pop() {
            if path.is_dir() {
                paths.extend(
                    fs::read_dir(path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let contents = fs::read_to_string(path).unwrap();
                for capture in prefix.captures_iter(&contents) {
                    let name = &capture[1];
                    if !matches!(name, "features" | "lints" | "package") {
                        rules.insert(name.to_owned());
                    }
                }
            }
        }
        assert_eq!(rules.iter().map(String::as_str).collect::<Vec<_>>(), RULES);
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
