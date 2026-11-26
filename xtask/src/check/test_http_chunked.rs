use super::{Result, test_scope::TestScopes};
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::{Expr, Lit, Token, parse::Parser as _, punctuated::Punctuated};

pub(super) fn check(
    path: &str,
    source: &str,
    scopes: &TestScopes,
    errors: &mut Vec<String>,
) -> Result {
    if path == "crates/emp-test-support/src/http.rs"
        || !scopes.has_test_scope()
        || !source.contains("from_str_radix")
    {
        return Ok(());
    }
    let tokens = source.parse().map_err(|error| format!("{path}: {error}"))?;
    inspect(tokens, scopes, &mut |line| {
        errors.push(format!(
            "[test-http-chunked] {path}:{line}: 测试读取 HTTP 分块请用 emp_test_support::http"
        ));
    });
    Ok(())
}

fn inspect(tokens: TokenStream, scopes: &TestScopes, report: &mut impl FnMut(usize)) {
    let mut tokens = tokens.into_iter().peekable();
    while let Some(token) = tokens.next() {
        match token {
            TokenTree::Ident(name) if name == "from_str_radix" => {
                let line = name.span().start().line;
                if scopes.contains(line)
                    && let Some(TokenTree::Group(group)) = tokens.peek()
                    && group.delimiter() == Delimiter::Parenthesis
                    && let Ok(args) =
                        Punctuated::<Expr, Token![,]>::parse_terminated.parse2(group.stream())
                    && args.len() == 2
                    && let Some(Expr::Lit(radix)) = args.last()
                    && let Lit::Int(radix) = &radix.lit
                    && radix.base10_parse::<u32>().is_ok_and(|value| value == 16)
                {
                    report(line);
                }
            }
            TokenTree::Group(group) => inspect(group.stream(), scopes, report),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::test_scope::tests::fixture;

    #[test]
    fn rejects_hex_parsing_in_test_scope() {
        let path = "crates/emp-app/src/example.rs";
        let source = "#[test]\nfn fixture() { assert!(usize::from_str_radix(\"a\", 16).is_ok()); }";
        let mut errors = Vec::new();
        check(path, source, &fixture(path, source), &mut errors).unwrap();
        assert_eq!(
            errors,
            [format!(
                "[test-http-chunked] {path}:2: 测试读取 HTTP 分块请用 emp_test_support::http"
            )]
        );
    }

    #[test]
    fn permits_other_radices_and_hex_parsing_outside_test_scope() {
        let path = "crates/emp-app/src/example.rs";
        let source = r#"
fn hex() { let _ = u8::from_str_radix("a", 16); }
#[test] fn decimal() { let _ = u8::from_str_radix("1", 10); }
"#;
        let mut errors = Vec::new();
        check(path, source, &fixture(path, source), &mut errors).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
    }
}
