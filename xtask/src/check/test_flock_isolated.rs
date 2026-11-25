use super::{Result, test_scope::TestScopes};
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::ToTokens as _;
use syn::{
    ext::IdentExt as _,
    parse::Parser as _,
    spanned::Spanned as _,
    visit::{self, Visit},
};

pub(super) fn check(
    path: &str,
    source: &str,
    scopes: &TestScopes,
    errors: &mut Vec<String>,
) -> Result {
    let tests_directory = path.split('/').any(|part| part == "tests");
    if !path.ends_with(".rs")
        || !(scopes.has_test_scope() || tests_directory)
        || !["drop", "unlock", "Unlock", "LOCK_UN"]
            .iter()
            .any(|name| source.contains(name))
    {
        return Ok(());
    }
    let file = syn::parse_file(source).map_err(|error| format!("{path}: {error}"))?;
    Functions {
        path,
        scopes,
        tests_directory,
        errors,
    }
    .visit_file(&file);
    Ok(())
}

struct Functions<'a> {
    path: &'a str,
    scopes: &'a TestScopes,
    tests_directory: bool,
    errors: &'a mut Vec<String>,
}

impl Functions<'_> {
    fn inspect(&mut self, name: &syn::Ident, block: &syn::Block) {
        if !(self.tests_directory || self.scopes.contains(block.span().start().line)) {
            return;
        }
        let mut calls = LockCalls::default();
        calls.scan(block.to_token_stream(), &name.unraw().to_string());
        if let Some(line) = calls.reacquired
            && !calls.isolated
        {
            self.errors.push(format!(
                "[test-flock-isolated] {}:{line}: {name} 在释放文件锁后再次加锁；使用 emp_test_support::isolated::command 运行该测试",
                self.path
            ));
        }
    }
}

impl<'ast> Visit<'ast> for Functions<'_> {
    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        self.inspect(&function.sig.ident, &function.block);
        visit::visit_item_fn(self, function);
    }

    fn visit_impl_item_fn(&mut self, function: &'ast syn::ImplItemFn) {
        self.inspect(&function.sig.ident, &function.block);
        visit::visit_impl_item_fn(self, function);
    }

    fn visit_trait_item_fn(&mut self, function: &'ast syn::TraitItemFn) {
        if let Some(block) = &function.default {
            self.inspect(&function.sig.ident, block);
        }
        visit::visit_trait_item_fn(self, function);
    }
}

#[derive(Default)]
struct LockCalls {
    file_api: bool,
    acquired: bool,
    released: bool,
    reacquired: Option<usize>,
    isolated: bool,
}

impl LockCalls {
    fn scan(&mut self, tokens: TokenStream, test_name: &str) {
        let tokens: Vec<_> = tokens.into_iter().collect();
        for (index, token) in tokens.iter().enumerate() {
            let TokenTree::Group(arguments) = token else {
                continue;
            };
            self.scan(arguments.stream(), test_name);
            if arguments.delimiter() != Delimiter::Parenthesis {
                continue;
            }
            let [prefix @ .., TokenTree::Ident(function)] = &tokens[..index] else {
                continue;
            };
            let qualifier = match prefix {
                [
                    ..,
                    TokenTree::Ident(owner),
                    TokenTree::Punct(first),
                    TokenTree::Punct(second),
                ] if first.as_char() == ':' && second.as_char() == ':' => {
                    Some(owner.unraw().to_string())
                }
                _ => None,
            };
            self.file_api |= matches!(qualifier.as_deref(), Some("File" | "OpenOptions"));
            match function.unraw().to_string().as_str() {
                "command" if qualifier.as_deref() == Some("isolated") => {
                    self.isolated |= syn::punctuated::Punctuated::<syn::LitStr, syn::Token![,]>::parse_terminated
                        .parse2(arguments.stream())
                        .is_ok_and(|names| {
                            names.len() == 1
                                && names[0].value().rsplit("::").next() == Some(test_name)
                        });
                }
                "drop" => self.released |= self.acquired,
                "unlock" | "UnlockFileEx" => self.released = true,
                "flock"
                    if arguments.stream().into_iter().any(|token| {
                        matches!(token, TokenTree::Ident(name) if name == "LOCK_UN" || name == "Unlock")
                    }) =>
                {
                    self.released = true;
                }
                "acquire" if matches!(qualifier.as_deref(), Some("IntegrationFileLock" | "ServiceOwner")) => {
                    self.acquire(function, true);
                }
                "lock" | "lock_shared" => self.acquire(function, self.file_api),
                "try_lock" | "try_lock_shared" | "lock_exclusive" | "try_lock_exclusive"
                | "flock" | "LockFileEx" | "operation_lock" | "acquire_coordination_lock"
                | "repair_before_native_restore" | "repair_idle_before_native_restore"
                | "repair_thread_before_native_resume" => self.acquire(function, true),
                _ => {}
            }
        }
    }

    fn acquire(&mut self, function: &syn::Ident, file_lock: bool) {
        self.acquired = true;
        if self.released && file_lock {
            self.reacquired.get_or_insert(function.span().start().line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::test_scope::tests::fixture;

    const RELOCK: &str = r#"
        let lock = File::open(path).unwrap();
        lock.lock().unwrap();
        drop(lock);
        assert!(File::open(path).unwrap().try_lock().is_ok());
    "#;

    #[test]
    fn accepts_relock_in_the_isolated_test() {
        let path = "tests/file_lock.rs";
        let source = format!(
            r#"
            #[test]
            fn relock() {{
                if std::env::var_os("EMP_TEST_RELOCK").is_none() {{
                    let mut command = isolated::command(
                        "tests::relock",
                    );
                    command.env("EMP_TEST_RELOCK", "1");
                    isolated::run(command, deadline);
                    return;
                }}
                {RELOCK}
                isolated::report(Ok(()));
            }}
            "#
        );
        let mut errors = Vec::new();
        check(path, &source, &fixture(path, &source), &mut errors).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn rejects_relock_after_drop_in_the_same_process() {
        let path = "src/lib.rs";
        let source = format!("#[cfg(test)] mod tests {{ #[test] fn relock() {{ {RELOCK} }} }}");
        let mut errors = Vec::new();
        check(path, &source, &fixture(path, &source), &mut errors).unwrap();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("[test-flock-isolated] src/lib.rs:5: relock"));
    }
}
