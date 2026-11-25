use super::{
    Path, Result, contains_ident, fs,
    test_scope::{Index, SourceKind},
};
use proc_macro2::{
    Delimiter, Span, TokenStream,
    TokenTree::{Group, Ident, Literal, Punct},
};
use quote::ToTokens as _;
use std::{
    collections::BTreeSet,
    path::{Component, PathBuf},
};
use syn::{
    Expr, Lit, LitStr, Macro, MacroDelimiter, Meta, Token, ext::IdentExt, parse::ParseStream,
    visit::Visit,
};

fn explicit_path(meta: &Meta) -> Option<&LitStr> {
    if let Meta::NameValue(attr) = meta
        && attr
            .path
            .get_ident()
            .is_some_and(|name| name.unraw() == "path")
        && let Expr::Lit(value) = &attr.value
        && let Lit::Str(path) = &value.lit
    {
        Some(path)
    } else {
        None
    }
}

pub(super) fn check<'a>(
    root: &Path,
    crate_dir: &Path,
    targets: impl Iterator<Item = (&'a Path, SourceKind)>,
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

pub(super) fn check_source(path: &str, source: &str, errors: &mut Vec<String>) -> Result {
    let file = syn::parse_file(source).map_err(|error| format!("{path}: {error}"))?;
    SourceRules { path, errors }.check_tokens(file.into_token_stream());
    Ok(())
}

struct SourceRules<'a> {
    path: &'a str,
    errors: &'a mut Vec<String>,
}

impl SourceRules<'_> {
    fn report(&mut self, prefix: &str, span: Span, message: &str) {
        let path = self.path;
        let line = span.start().line;
        self.errors
            .push(format!("{prefix} {path}:{line}: {message}"));
    }

    fn check_tokens(&mut self, tokens: TokenStream) {
        let tokens: Vec<_> = tokens.into_iter().collect();
        for index in 0..tokens.len() {
            match &tokens[index..] {
                [Group(group), ..] => self.check_tokens(group.stream()),
                [Ident(name), Group(args), ..]
                    if name.unraw() == "cfg_attr"
                        && args.delimiter() == Delimiter::Parenthesis
                        && contains_path_assignment(args.stream()) =>
                {
                    self.report("[conditional-module-path]", name.span(), "禁止通过 cfg_attr 设置模块的 path；请用 #[cfg(...)] 分别声明两个 mod，或把差异放到同一文件内的 cfg 块中");
                }
                [Punct(hash), Group(attribute), ..]
                    if hash.as_char() == '#' && attribute.delimiter() == Delimiter::Bracket =>
                {
                    let mut parts = attribute.stream().into_iter();
                    if let (Some(Ident(name)), Some(Punct(eq)), Some(path @ Literal(_))) =
                        (parts.next(), parts.next(), parts.next())
                        && name.unraw() == "path"
                        && eq.as_char() == '='
                        && let Ok(path) = syn::parse2::<LitStr>(path.into())
                    {
                        let value = path.value();
                        if !value.ends_with(".rs") {
                            self.report("[module-path-extension]", path.span(), &format!("#[path = {value:?}] 的目标文件必须以 .rs 结尾；请将文件改名为 .rs 并更新 #[path]"));
                        }
                    }
                }
                [Ident(name), Punct(bang), Group(args), ..]
                    if name.unraw() == "include"
                        && bang.as_char() == '!'
                        && !out_dir_include(args.stream()) =>
                {
                    self.report("[no-source-include]", name.span(), "工作区源码用 mod 声明，不要用 include! 拼接；只有构建脚本生成到 OUT_DIR 的代码可以用 include!");
                }
                [Ident(name), Punct(bang), Ident(_), Group(body), ..]
                    if name == "macro_rules"
                        && bang.as_char() == '!'
                        && contains_ident(body.stream(), "mod") =>
                {
                    self.report(
                        "[no-mod-in-macro]",
                        name.span(),
                        "模块声明必须直接写出，不能由 macro_rules! 生成",
                    );
                }
                _ => {}
            }
        }
    }
}

fn contains_path_assignment(tokens: TokenStream) -> bool {
    let mut tokens = tokens.into_iter().peekable();
    while let Some(token) = tokens.next() {
        match token {
            Ident(name)
                if name.unraw() == "path"
                    && matches!(tokens.peek(), Some(Punct(eq)) if eq.as_char() == '=') =>
            {
                return true;
            }
            Group(group) if contains_path_assignment(group.stream()) => return true,
            _ => {}
        }
    }
    false
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
            &mut Index::default(),
            &mut errors,
        )
        .unwrap();
        errors
    }

    #[test]
    fn checks_git_listed_sources_once_regardless_of_module_paths() {
        let directory = emp_test_support::fixture_dir().unwrap();
        let root = directory.path();
        let entry = root.join("lib.rs");
        fs::write(
            &entry,
            r#"#[path = "./platform.rs"] mod alternate; mod platform;"#,
        )
        .unwrap();
        let source = r#"
#[cfg_attr(unix, allow(dead_code), path = "unix.rs")]
#[cfg_attr(not(unix), path = "other.rs")]
mod backend;
mod fixtures {
    #[cfg_attr(unix,
        r#cfg_attr(test, path = "platform_tests.rs"))] mod platform;
}
#[cfg_attr(unix, path = "fixture.rs")] fn probe() {}
#[cfg_attr(true, path = "platform.rs")] mod boolean_condition;
#[cfg(unix)] #[path = "native.rs"] mod native;
#[cfg_attr(test, allow(dead_code))] mod support;
#[cfg_attr(feature = "x", doc = "path = y")] mod documented;
#[path = "helpers.inc"] mod helpers;
cfg_if! { if #[cfg(unix)] { #[path = "helpers.inc"] mod helpers; } }
include!("platform.rs");
include!(concat!(env!("OUT_DIR"), "/x.rs"));
const TEXT: &str = include_str!("a.txt");
const BYTES: &[u8] = include_bytes!("b.bin");
macro_rules! declare { ($a:meta) => { #[$a] mod backend; } }
macro_rules! helper { () => { "mod" } }"#;
        fs::write(root.join("platform.rs"), source).unwrap();
        fs::write(root.join("helpers.inc"), "").unwrap();
        fs::write(
            root.join("unreferenced.rs"),
            r#"#[path = "helpers.inc"] mod helpers;"#,
        )
        .unwrap();
        let mut errors = diagnostics(root, root, &entry);
        let paths = ["helpers.inc", "lib.rs", "platform.rs", "unreferenced.rs"].map(str::to_owned);
        super::super::check_files(root, &paths, &Index::default(), &mut errors).unwrap();
        let expected = [
            "[conditional-module-path] platform.rs:2:",
            "[conditional-module-path] platform.rs:3:",
            "[conditional-module-path] platform.rs:6:",
            "[conditional-module-path] platform.rs:7:",
            "[conditional-module-path] platform.rs:9:",
            "[conditional-module-path] platform.rs:10:",
            "[module-path-extension] platform.rs:14:",
            "[module-path-extension] platform.rs:15:",
            "[no-source-include] platform.rs:16:",
            "[no-mod-in-macro] platform.rs:20:",
            "[module-path-extension] unreferenced.rs:1:",
        ];
        assert_eq!(errors.len(), expected.len(), "{errors:?}");
        for prefix in expected {
            assert!(
                errors.iter().any(|error| error.starts_with(prefix)),
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
