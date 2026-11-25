use super::{Result, comments};
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::{
    Attribute, Expr, ImplItem, Item, Lit, Meta, Token,
    ext::IdentExt as _,
    punctuated::Punctuated,
    visit::{self, Visit},
};

pub(super) fn check(path: &str, source: &str, errors: &mut Vec<String>) -> Result {
    if !path.ends_with(".rs") || !source.contains("Drop") {
        return Ok(());
    }
    let source = syn::parse_file(source).map_err(|error| format!("{path}: {error}"))?;
    DropImpls {
        test_scope: comments::test_file(path) || path.starts_with("crates/emp-test-support/src/"),
        body: DropBody { path, errors },
    }
    .visit_file(&source);
    Ok(())
}

fn fixture_attribute(attr: &Attribute) -> bool {
    comments::test_only_attribute(attr)
        || (attr.path().is_ident("cfg")
            && attr
                .parse_args::<Meta>()
                .is_ok_and(|meta| fixture_cfg(&meta)))
}

fn fixture_cfg(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::NameValue(value) => {
            value.path.is_ident("feature")
                && matches!(&value.value, Expr::Lit(expr) if matches!(&expr.lit, Lit::Str(feature) if feature.value() == "test-support"))
        }
        Meta::List(list) => {
            let Ok(parts) = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            else {
                return false;
            };
            if list.path.is_ident("all") {
                parts.iter().any(fixture_cfg)
            } else {
                list.path.is_ident("any") && !parts.is_empty() && parts.iter().all(fixture_cfg)
            }
        }
    }
}

struct DropImpls<'a> {
    test_scope: bool,
    body: DropBody<'a>,
}

impl<'ast> Visit<'ast> for DropImpls<'_> {
    fn visit_file(&mut self, file: &'ast syn::File) {
        self.test_scope |= file.attrs.iter().any(fixture_attribute);
        visit::visit_file(self, file);
    }

    fn visit_item(&mut self, item: &'ast Item) {
        let attrs = match item {
            Item::Const(item) => &item.attrs,
            Item::Fn(item) => &item.attrs,
            Item::Impl(item) => &item.attrs,
            Item::Mod(item) => &item.attrs,
            Item::Static(item) => &item.attrs,
            Item::Trait(item) => &item.attrs,
            _ => return,
        };
        let previous = self.test_scope;
        self.test_scope |= attrs.iter().any(fixture_attribute);
        visit::visit_item(self, item);
        self.test_scope = previous;
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if item.trait_.as_ref().is_some_and(|(_, path, _)| {
            path.segments
                .last()
                .is_some_and(|segment| segment.ident == "Drop")
        }) {
            for member in &item.items {
                if let ImplItem::Fn(method) = member
                    && method.sig.ident == "drop"
                    && (self.test_scope || method.attrs.iter().any(fixture_attribute))
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
            "[test-drop] {path}:{line}: 测试夹具的 Drop 不能直接调用 {ident}；使用 emp_test_support::drop_check"
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
