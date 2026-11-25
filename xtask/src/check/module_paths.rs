use super::{
    Path, Result, contains_ident, fs,
    test_scope::{Index, SourceKind},
};
use proc_macro2::{Span, TokenStream, TokenTree};
use quote::ToTokens as _;
use std::{
    collections::BTreeSet,
    path::{Component, PathBuf},
};
use syn::{
    Expr, Lit, LitStr, Macro, MacroDelimiter, Meta, Token,
    ext::IdentExt,
    parse::ParseStream,
    punctuated::Punctuated,
    spanned::Spanned,
    visit::{self, Visit},
};

fn path_is(path: &syn::Path, name: &str) -> bool {
    matches!(path.get_ident(), Some(ident) if ident.unraw() == name)
}

fn conditional_path(meta: &Meta) -> bool {
    let Meta::List(list) = meta else { return false };
    path_is(&list.path, "cfg_attr")
        && matches!(list.delimiter, MacroDelimiter::Paren(_))
        && list
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .is_ok_and(|parts| {
                parts.iter().skip(1).any(|part| match part {
                    Meta::NameValue(attr) => path_is(&attr.path, "path"),
                    _ => conditional_path(part),
                })
            })
}

fn explicit_path(meta: &Meta) -> Option<&LitStr> {
    match meta {
        Meta::NameValue(attr) if path_is(&attr.path, "path") => match &attr.value {
            Expr::Lit(value) => match &value.lit {
                Lit::Str(path) => Some(path),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

pub(super) fn check<'a>(
    root: &Path,
    crate_dir: &Path,
    targets: impl Iterator<Item = (&'a Path, SourceKind)>,
    checked: &mut BTreeSet<PathBuf>,
    test_scopes: &mut Index,
    errors: &mut Vec<String>,
) -> Result {
    let mut sources = CrateSources {
        root,
        resolved_root: resolve(root),
        crate_dir: crate_dir
            .canonicalize()
            .map_err(|error| format!("{}: {error}", crate_dir.display()))?,
        visited: BTreeSet::new(),
        checked,
        test_scopes,
        errors,
    };
    for (target, kind) in targets {
        sources.visit_file(target, None, kind);
    }
    Ok(())
}

struct CrateSources<'a> {
    root: &'a Path,
    resolved_root: PathBuf,
    crate_dir: PathBuf,
    visited: BTreeSet<(PathBuf, Option<String>, SourceKind)>,
    checked: &'a mut BTreeSet<PathBuf>,
    test_scopes: &'a mut Index,
    errors: &'a mut Vec<String>,
}

impl CrateSources<'_> {
    fn visit_file(&mut self, file: &Path, module: Option<String>, kind: SourceKind) {
        if !file.is_file() {
            return;
        }
        let resolved = resolve(file);
        if !self
            .visited
            .insert((resolved.clone(), module.clone(), kind))
        {
            return;
        }
        let source = fs::read_to_string(file)
            .map_err(|error| error.to_string())
            .and_then(|source| syn::parse_file(&source).map_err(|error| error.to_string()));
        match source {
            Ok(source) => {
                let path = resolved.strip_prefix(&self.resolved_root).ok();
                if self.checked.insert(resolved.clone()) {
                    let mut rules = SourceRules {
                        file: path.unwrap_or(&resolved),
                        errors: self.errors,
                    };
                    rules.visit_file(&source);
                    rules.check_macros(source.to_token_stream());
                }
                let kind = kind.in_test(&source.attrs);
                if let Some(path) = path {
                    self.test_scopes.record(path, &source, kind);
                }
                ModulePaths {
                    sources: self,
                    file,
                    directory: file.parent().expect("absolute Rust source path"),
                    module,
                    kind,
                }
                .visit_file(&source);
            }
            Err(error) => self.errors.push(format!(
                "[module-paths] {}: {error}",
                file.strip_prefix(self.root).unwrap_or(file).display()
            )),
        }
    }
}

struct SourceRules<'a> {
    file: &'a Path,
    errors: &'a mut Vec<String>,
}

impl SourceRules<'_> {
    fn report(&mut self, rule: &str, span: Span, message: &str) {
        let line = span.start().line;
        self.errors.push(format!(
            "[{rule}] {}:{line}: {message}",
            self.file.display()
        ));
    }

    fn check_macros(&mut self, tokens: TokenStream) {
        let tokens: Vec<_> = tokens.into_iter().collect();
        for (index, token) in tokens.iter().enumerate() {
            if let TokenTree::Group(group) = token {
                self.check_macros(group.stream());
            }
            let [TokenTree::Ident(name), TokenTree::Punct(bang), tail @ ..] = &tokens[index..]
            else {
                continue;
            };
            if bang.as_char() != '!' {
                continue;
            }
            let (rule, message) = match tail {
                [TokenTree::Group(args), ..]
                    if name.unraw() == "include" && !out_dir_include(args.stream()) =>
                {
                    (
                        "no-source-include",
                        "工作区源码用 mod 声明，不要用 include! 拼接；只有构建脚本生成到 OUT_DIR 的代码可以用 include!",
                    )
                }
                [TokenTree::Ident(_), TokenTree::Group(body), ..]
                    if name == "macro_rules" && contains_ident(body.stream(), "mod") =>
                {
                    (
                        "no-mod-in-macro",
                        "模块声明必须直接写出，不能由 macro_rules! 生成",
                    )
                }
                _ => continue,
            };
            self.report(rule, name.span(), message);
        }
    }
}

fn out_dir_include(tokens: TokenStream) -> bool {
    syn::parse2::<Macro>(tokens).is_ok_and(|concat| {
        concat.path.is_ident("concat")
            && matches!(concat.delimiter, MacroDelimiter::Paren(_))
            && concat
                .parse_body_with(|input: ParseStream| {
                    let env: Macro = input.parse()?;
                    input.parse::<Token![,]>()?;
                    input.parse::<LitStr>()?;
                    Ok(env.path.is_ident("env")
                        && matches!(env.delimiter, MacroDelimiter::Paren(_))
                        && env
                            .parse_body::<LitStr>()
                            .is_ok_and(|name| name.value() == "OUT_DIR"))
                })
                .unwrap_or(false)
    })
}

impl<'ast> Visit<'ast> for SourceRules<'_> {
    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        if let Some(path) = explicit_path(&attr.meta) {
            let value = path.value();
            if !value.ends_with(".rs") {
                self.report("module-path-extension", path.span(), &format!("#[path = {value:?}] 的目标文件必须以 .rs 结尾；请将文件改名为 .rs 并更新 #[path]"));
            }
        }
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        for attr in &item.attrs {
            if conditional_path(&attr.meta) {
                self.report("conditional-module-path", attr.span(), "禁止通过 cfg_attr 设置模块的 path；请用 #[cfg(...)] 分别声明两个 mod，或把差异放到同一文件内的 cfg 块中");
            }
        }
        visit::visit_item_mod(self, item);
    }
}

struct ModulePaths<'a, 'source> {
    sources: &'a mut CrateSources<'source>,
    file: &'a Path,
    directory: &'a Path,
    module: Option<String>,
    kind: SourceKind,
}

impl<'ast> Visit<'ast> for ModulePaths<'_, '_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let kind = self.kind.in_test(&item.attrs);
        let explicit = item
            .attrs
            .iter()
            .filter_map(|attr| explicit_path(&attr.meta));
        let mut has_explicit_path = false;
        for path in explicit {
            has_explicit_path = true;
            let value = path.value();
            let portable = value.replace('\\', "/");
            let rooted = portable.starts_with('/')
                || matches!(portable.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic());
            let native = Path::new(&value);
            let mut targets = BTreeSet::from([self.directory.join(native)]);
            if native.is_absolute() || !rooted {
                let normalized = if cfg!(windows) && native.is_absolute() {
                    value.replace('/', "\\")
                } else {
                    portable
                };
                targets.insert(self.directory.join(normalized));
            }
            if rooted && !native.is_absolute()
                || targets
                    .iter()
                    .any(|target| !resolve(target).starts_with(&self.sources.crate_dir))
            {
                let file = self
                    .file
                    .strip_prefix(self.sources.root)
                    .unwrap_or(self.file);
                self.sources.errors.push(format!(
                    "[module-paths] {}:{}: #[path = {:?}] 指向本 crate 目录之外",
                    file.display(),
                    path.span().start().line,
                    value
                ));
            } else {
                for target in targets {
                    if let Some((_, items)) = &item.content {
                        self.visit_inline(&target, items, kind);
                    } else {
                        self.sources.visit_file(&target, None, kind);
                    }
                }
            }
        }

        if has_explicit_path {
            return;
        }
        let mut directory = self.directory.to_owned();
        if let Some(module) = &self.module {
            directory.push(module);
        }
        let name = item.ident.unraw().to_string();
        let child = directory.join(&name);
        if let Some((_, items)) = &item.content {
            self.visit_inline(&child, items, kind);
        } else {
            self.sources
                .visit_file(&child.with_extension("rs"), Some(name), kind);
            self.sources.visit_file(&child.join("mod.rs"), None, kind);
        }
    }
}

impl ModulePaths<'_, '_> {
    fn visit_inline(&mut self, directory: &Path, items: &[syn::Item], kind: SourceKind) {
        let mut visitor = ModulePaths {
            sources: self.sources,
            file: self.file,
            directory,
            module: None,
            kind,
        };
        for item in items {
            visitor.visit_item(item);
        }
    }
}

fn resolve(path: &Path) -> PathBuf {
    if let Ok(resolved) = path.canonicalize() {
        return resolved;
    }
    let Some(parent) = path.parent() else {
        return path.to_owned();
    };
    let mut resolved = resolve(parent);
    match path.components().next_back() {
        Some(Component::ParentDir) => {
            resolved.pop();
        }
        Some(component) => resolved.push(component),
        None => {}
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostics(root: &Path, crate_dir: &Path, entry: &Path) -> Vec<String> {
        let mut errors = Vec::new();
        check(
            root,
            crate_dir,
            [(entry, SourceKind::Production)].into_iter(),
            &mut BTreeSet::new(),
            &mut Index::default(),
            &mut errors,
        )
        .unwrap();
        errors
    }

    #[test]
    fn checks_source_rules_once_across_module_paths() {
        let directory = emp_test_support::fixture_dir().unwrap();
        let root = directory.path();
        let entry = root.join("lib.rs");
        fs::write(
            &entry,
            r#"#[path = "./platform.rs"] mod alternate; mod platform;"#,
        )
        .unwrap();
        let source = r#"
#[cfg_attr(unix, allow(dead_code), path = "unix.rs")] mod backend;
mod fixtures { #[cfg_attr(unix, r#cfg_attr(test, path = "platform_tests.rs"))] mod platform; }
#[cfg_attr(unix, path = "fixture.rs")] fn probe() {}
#[cfg(unix)] #[path = "native.rs"] mod native;
#[cfg_attr(test, allow(dead_code))] mod support;
#[path = "helpers.inc"] mod helpers;
include!("platform.rs");
include!(concat!(env!("OUT_DIR"), "/x.rs"));
const TEXT: &str = include_str!("a.txt");
const BYTES: &[u8] = include_bytes!("b.bin");
macro_rules! declare { ($a:meta) => { #[$a] mod backend; } }
macro_rules! helper { () => { "mod" } }"#;
        fs::write(root.join("platform.rs"), source).unwrap();
        fs::write(root.join("helpers.inc"), "").unwrap();
        let errors = diagnostics(root, root, &entry);
        let expected = [
            ("conditional-module-path", 2),
            ("conditional-module-path", 3),
            ("module-path-extension", 7),
            ("no-source-include", 8),
            ("no-mod-in-macro", 12),
        ];
        assert_eq!(errors.len(), expected.len(), "{errors:?}");
        for (rule, line) in expected {
            let prefix = format!("[{rule}] platform.rs:{line}:");
            assert!(
                errors.iter().any(|error| error.starts_with(&prefix)),
                "{errors:?}"
            );
        }
    }

    #[test]
    fn module_paths_stay_inside_the_declaring_crate() {
        let root = emp_test_support::fixture_dir().unwrap();
        let crate_dir = root.path().join("app");
        let src = crate_dir.join("src");
        let entry = src.join("lib.rs");
        fs::create_dir_all(src.join("local/nested")).unwrap();
        fs::write(
            &entry,
            r#"
#[path = "../shared.rs"] mod shared;
#[path = "../../other.rs"] mod outside;
#[cfg(windows)] #[path = "../../missing.rs"] mod windows_only;
#[cfg(windows)] #[path = "missing.rs"] mod local_windows_only;
mod local;
"#,
        )
        .unwrap();
        fs::write(crate_dir.join("shared.rs"), "").unwrap();
        fs::write(root.path().join("other.rs"), "").unwrap();
        fs::write(
            src.join("local.rs"),
            r#"
mod nested {
    #[path = "../../visible.rs"] mod inside;
    #[path = "../../../../other.rs"] mod escaped;
}
"#,
        )
        .unwrap();
        fs::write(src.join("visible.rs"), "").unwrap();
        let errors = diagnostics(root.path(), &crate_dir, &entry);
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors[0].contains("lib.rs:3"));
        assert!(errors[1].contains("lib.rs:4"));
        assert!(errors[2].contains("local.rs:4"));
    }

    #[test]
    fn checks_native_absolute_paths_against_the_declaring_crate() {
        let root = emp_test_support::fixture_dir().unwrap();
        let crate_dir = root.path().join("app");
        let src = crate_dir.join("src");
        fs::create_dir_all(&src).unwrap();
        let entry = src.join("lib.rs");
        let inside = src.join("inside.rs");
        let outside = root.path().join("outside.rs");
        fs::write(&inside, "").unwrap();
        fs::write(&outside, "").unwrap();
        let src = src.canonicalize().unwrap();
        fs::write(
            &entry,
            format!(
                "#[path = {:?}] mod inside;\n#[path = {:?}] mod outside;\n",
                src.join("../src/inside.rs").to_str().unwrap(),
                src.join("../../outside.rs").to_str().unwrap(),
            ),
        )
        .unwrap();

        let errors = diagnostics(root.path(), &crate_dir, &entry);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("lib.rs:2"), "{errors:?}");
    }

    #[test]
    fn checks_windows_module_paths_on_every_host() {
        let root = emp_test_support::fixture_dir().unwrap();
        let crate_dir = root.path().join("emp-updater");
        let src = crate_dir.join("src");
        let entry = src.join("lib.rs");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(
            &entry,
            r#"
#[cfg(windows)] #[path = r"..\..\emp-test-support\src\lib.rs"] mod outside;
#[cfg(windows)] #[path = r"..\../emp-test-support\src/lib.rs"] mod mixed;
#[path = r"nested\inside.rs"] mod inside;
#[path = r"C:\src\outside.rs"] mod drive;
#[path = r"C:outside.rs"] mod drive_relative;
#[path = r"\\server\share\outside.rs"] mod unc;
#[path = r"\src\outside.rs"] mod rooted;
#[path = "//server/share/outside.rs"] mod slash_unc;
"#,
        )
        .unwrap();
        let support = root.path().join("emp-test-support/src");
        fs::create_dir_all(&support).unwrap();
        fs::write(support.join("lib.rs"), "").unwrap();
        fs::write(src.join("nested/inside.rs"), "").unwrap();

        let errors = diagnostics(root.path(), &crate_dir, &entry);
        assert_eq!(errors.len(), 7, "{errors:?}");
        for (error, line) in errors.iter().zip([2, 3, 5, 6, 7, 8, 9]) {
            assert!(error.contains(&format!("lib.rs:{line}:")), "{error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn normalizes_separators_in_native_absolute_paths() {
        let root = emp_test_support::fixture_dir().unwrap();
        let crate_dir = root.path().join("app");
        let src = crate_dir.join("src");
        let other = root.path().join("other/src");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::write(src.join("inside.rs"), "").unwrap();
        fs::write(other.join("lib.rs"), "").unwrap();
        let entry = src.join("lib.rs");
        let source = src.canonicalize().unwrap();
        fs::write(
            &entry,
            format!(
                "#[path = {:?}] mod inside;\n#[path = {:?}] mod outside;\n",
                source.join(r"..\src\inside.rs").to_str().unwrap(),
                source.join(r"..\..\other\src\lib.rs").to_str().unwrap(),
            ),
        )
        .unwrap();

        let errors = diagnostics(root.path(), &crate_dir, &entry);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("lib.rs:2:"), "{errors:?}");
    }
}
