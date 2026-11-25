use super::{Result, test_scope::TestScopes};
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::{
    ImplItem, Item,
    ext::IdentExt as _,
    spanned::Spanned,
    visit::{self, Visit},
};

pub(super) fn check(
    path: &str,
    source: &str,
    scopes: &TestScopes,
    errors: &mut Vec<String>,
) -> Result {
    if !path.ends_with(".rs") || !source.contains("Drop") {
        return Ok(());
    }
    let source = syn::parse_file(source).map_err(|error| format!("{path}: {error}"))?;
    DropImpls {
        scopes,
        body: DropBody { path, errors },
    }
    .visit_file(&source);
    Ok(())
}

struct DropImpls<'a> {
    scopes: &'a TestScopes,
    body: DropBody<'a>,
}

impl<'ast> Visit<'ast> for DropImpls<'_> {
    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if item.trait_.as_ref().is_some_and(|(_, path, _)| {
            path.segments
                .last()
                .is_some_and(|segment| segment.ident == "Drop")
        }) {
            for member in &item.items {
                if let ImplItem::Fn(method) = member
                    && method.sig.ident == "drop"
                    && self.scopes.contains(method.block.span().start().line)
                {
                    self.body.visit_block(&method.block);
                }
            }
        }
        visit::visit_item_impl(self, item);
    }
}

struct DropBody<'a> {
    path: &'a str,
    errors: &'a mut Vec<String>,
}

impl DropBody<'_> {
    fn reject(&mut self, ident: &syn::Ident) {
        let path = self.path;
        let line = ident.span().start().line;
        self.errors.push(format!(
            "[test-drop] {path}:{line}: 测试夹具的 Drop 不能直接调用 {ident}；使用 emp_test_support::drop_check 或 join_check"
        ));
    }

    fn method(&mut self, ident: &syn::Ident) {
        if matches!(ident.unraw().to_string().as_str(), "unwrap" | "expect") {
            self.reject(ident);
        }
    }

    fn macro_name(&mut self, ident: &syn::Ident) {
        let name = ident.unraw().to_string();
        if matches!(name.as_str(), "panic" | "unreachable")
            || name.starts_with("assert")
            || name.starts_with("debug_assert")
        {
            self.reject(ident);
        }
    }

    fn macro_tokens(&mut self, tokens: TokenStream) {
        let tokens: Vec<_> = tokens.into_iter().collect();
        for window in tokens.windows(3) {
            match window {
                [
                    TokenTree::Punct(dot),
                    TokenTree::Ident(ident),
                    TokenTree::Group(args),
                ] if dot.as_char() == '.' && args.delimiter() == Delimiter::Parenthesis => {
                    self.method(ident);
                }
                [
                    TokenTree::Ident(ident),
                    TokenTree::Punct(bang),
                    TokenTree::Group(_),
                ] if bang.as_char() == '!' => {
                    self.macro_name(ident);
                }
                _ => {}
            }
        }
        for token in tokens {
            if let TokenTree::Group(group) = token {
                self.macro_tokens(group.stream());
            }
        }
    }
}

impl<'ast> Visit<'ast> for DropBody<'_> {
    fn visit_item(&mut self, _: &'ast Item) {}

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.method(&call.method);
        visit::visit_expr_method_call(self, call);
    }

    fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
        if let Some(segment) = invocation.path.segments.last() {
            self.macro_name(&segment.ident);
        }
        self.macro_tokens(invocation.tokens.clone());
    }
}
