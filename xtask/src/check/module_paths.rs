use super::{
    Path, Result, attribute_group, contains_ident, fs,
    test_scope::{Index, SourceKind},
};
use proc_macro2::{
    Delimiter, Spacing, Span, TokenStream,
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
    tracked_paths: &BTreeSet<&Path>,
    test_scopes: &mut Index,
    errors: &mut Vec<String>,
) -> Result {
    let mut sources = CrateSources {
        root,
        resolved_root: resolve(root),
        crate_dir: crate_dir
            .canonicalize()
            .map_err(|error| format!("{}: {error}", crate_dir.display()))?,
        tracked_paths,
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
    tracked_paths: &'a BTreeSet<&'a Path>,
    visited: BTreeSet<(PathBuf, Option<String>, SourceKind)>,
    test_scopes: &'a mut Index,
    errors: &'a mut Vec<String>,
}

impl CrateSources<'_> {
    fn is_tracked_source(&self, path: &Path) -> bool {
        let mut relative = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(name) => relative.push(name),
                Component::CurDir => {}
                Component::ParentDir if relative.pop() => {}
                _ => return false,
            }
        }
        relative
            .extension()
            .is_some_and(|extension| extension == "rs")
            && self.tracked_paths.contains(relative.as_path())
    }

    fn visit_module(&mut self, file: &Path, module: Option<String>, kind: SourceKind) {
        if file.is_file() {
            self.visit_file(file, module, kind);
        }
    }

    fn visit_file(&mut self, file: &Path, module: Option<String>, kind: SourceKind) {
        let relative = file
            .strip_prefix(self.root)
            .or_else(|_| file.strip_prefix(&self.resolved_root))
            .unwrap_or(file);
        let resolved = resolve(file);
        let tracked = resolved
            .strip_prefix(&self.resolved_root)
            .ok()
            .filter(|path| {
                self.is_tracked_source(path)
                    && (relative == *path || self.is_tracked_source(relative))
            });
        if tracked.is_none() {
            self.errors.push(format!(
                "[tracked-sources] {}: 编译源码必须是未被 .gitignore 排除的 .rs 文件；生成代码请写到 OUT_DIR，再用 include!(concat!(env!(\"OUT_DIR\"), ...)) 引入",
                relative.display()
            ));
            if !resolved.starts_with(&self.crate_dir) {
                return;
            }
        }
        if !file.is_file() {
            return;
        }
        if !self
            .visited
            .insert((resolved.clone(), module.clone(), kind))
        {
            return;
        }
        let source = match fs::read_to_string(file) {
            Ok(source) => source,
            Err(error) => {
                self.errors
                    .push(format!("[module-paths] {}: {error}", relative.display()));
                return;
            }
        };
        let Ok(source) = syn::parse_file(&source) else {
            return;
        };
        let kind = kind.in_test(&source.attrs);
        if let Some(path) = tracked {
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
}

pub(super) fn check_source(path: &str, source: &str, errors: &mut Vec<String>) -> bool {
    match syn::parse_file(source) {
        Ok(file) => {
            SourceRules { path, errors }.check_tokens(file.into_token_stream());
            true
        }
        Err(error) => {
            errors.push(format!("[module-paths] {path}: {error}"));
            false
        }
    }
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
                [Punct(hash), rest @ ..] if hash.as_char() == '#' => {
                    let Some(attribute) = attribute_group(rest) else {
                        continue;
                    };
                    let mut parts = attribute.stream().into_iter();
                    if let (Some(Ident(name)), Some(Punct(eq)), Some(path @ Literal(_))) =
                        (parts.next(), parts.next(), parts.next())
                        && is_path_assignment(&name, &eq)
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

fn is_path_assignment(name: &proc_macro2::Ident, eq: &proc_macro2::Punct) -> bool {
    name.unraw() == "path" && eq.as_char() == '=' && eq.spacing() == Spacing::Alone
}

fn contains_path_assignment(tokens: TokenStream) -> bool {
    let mut tokens = tokens.into_iter().peekable();
    while let Some(token) = tokens.next() {
        match token {
            Ident(name) if matches!(tokens.peek(), Some(Punct(eq)) if is_path_assignment(&name, eq)) =>
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
                        self.sources.visit_module(&target, None, kind);
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
                .visit_module(&child.with_extension("rs"), Some(name), kind);
            self.sources.visit_module(&child.join("mod.rs"), None, kind);
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

    fn diagnostics(root: &Path, crate_dir: &Path, entry: &Path, tracked: &[&str]) -> Vec<String> {
        let mut errors = Vec::new();
        check(
            root,
            crate_dir,
            [(entry, SourceKind::Production)].into_iter(),
            &tracked.iter().map(Path::new).collect(),
            &mut Index::default(),
            &mut errors,
        )
        .unwrap();
        errors
    }

    #[test]
    fn compiled_sources_must_be_tracked_rust_files() {
        let directory = emp_test_support::fixture_dir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("src/generated")).unwrap();
        fs::write(root.join("src/entry.inc"), "").unwrap();
        fs::write(
            root.join("src/lib.rs"),
            r#"#[path = "generated/helper.rs"] mod helper;"#,
        )
        .unwrap();
        fs::write(root.join("src/generated/helper.rs"), "").unwrap();

        for (entry, tracked, rejected) in [
            (
                "src/entry.inc",
                ["src/entry.inc"].as_slice(),
                Some("src/entry.inc"),
            ),
            (
                "src/lib.rs",
                ["src/lib.rs"].as_slice(),
                Some("src/generated/helper.rs"),
            ),
            (
                "src/lib.rs",
                ["src/lib.rs", "src/generated/helper.rs"].as_slice(),
                None,
            ),
        ] {
            let errors = diagnostics(root, root, &root.join(entry), tracked);
            assert_eq!(errors.len(), usize::from(rejected.is_some()), "{errors:?}");
            if let Some(path) = rejected {
                assert!(
                    errors[0]
                        .replace('\\', "/")
                        .starts_with(&format!("[tracked-sources] {path}:")),
                    "{errors:?}"
                );
            }
        }
    }

    #[test]
    fn checks_module_boundaries_in_ignored_sources() {
        let directory = emp_test_support::fixture_dir().unwrap();
        let root = directory.path();
        let crate_dir = root.join("app");
        let entry = crate_dir.join("src/lib.rs");
        fs::create_dir_all(crate_dir.join("src")).unwrap();
        fs::create_dir_all(root.join("other/src")).unwrap();
        fs::write(&entry, "mod helper;").unwrap();
        fs::write(
            crate_dir.join("src/helper.rs"),
            r#"#[path = "../../other/src/lib.rs"] mod outside;"#,
        )
        .unwrap();
        fs::write(root.join("other/src/lib.rs"), "").unwrap();

        let errors = diagnostics(
            root,
            &crate_dir,
            &entry,
            &["app/src/lib.rs", "other/src/lib.rs"],
        );
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(
            errors[0]
                .replace('\\', "/")
                .starts_with("[tracked-sources] app/src/helper.rs:")
        );
        assert!(
            errors[1]
                .replace('\\', "/")
                .starts_with("[module-paths] app/src/helper.rs:1:")
        );
        assert!(errors[1].contains("指向本 crate 目录之外"));
    }

    #[test]
    fn reports_syntax_errors_once_and_checks_other_files() {
        let directory = emp_test_support::fixture_dir().unwrap();
        let root = directory.path();
        let entry = root.join("lib.rs");
        fs::write(
            &entry,
            r#"mod broken; #[path = "./broken.rs"] mod alias; mod other;"#,
        )
        .unwrap();
        fs::write(root.join("broken.rs"), "impl Drop for {").unwrap();
        fs::write(root.join("other.rs"), r#"include!("x.rs");"#).unwrap();

        let tracked = ["broken.rs", "lib.rs", "other.rs"];
        let mut errors = diagnostics(root, root, &entry, &tracked);
        let (_, invalid_sources) = super::super::check_files(
            root,
            &tracked.map(str::to_owned),
            &Index::default(),
            &mut errors,
        )
        .unwrap();
        assert_eq!(invalid_sources, BTreeSet::from(["broken.rs".to_owned()]));
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(
            errors[0].starts_with("[module-paths] broken.rs:"),
            "{errors:?}"
        );
        assert!(
            errors[1].starts_with("[no-source-include] other.rs:1:"),
            "{errors:?}"
        );
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
        fs::write(
            root.join("unreferenced.rs"),
            r#"#[path = "helpers.inc"] mod helpers;"#,
        )
        .unwrap();
        let paths = ["lib.rs", "platform.rs", "unreferenced.rs"];
        let mut errors = diagnostics(root, root, &entry, &paths);
        super::super::check_files(
            root,
            &paths.map(str::to_owned),
            &Index::default(),
            &mut errors,
        )
        .unwrap();
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
    fn checks_inner_module_path_attributes() {
        let source = r#"mod fixtures { #![path = "helpers.inc"] }
mod conditional { #![cfg_attr(all(), path = "helpers.rs")] }"#;
        let mut errors = Vec::new();
        assert!(check_source("lib.rs", source, &mut errors), "{errors:?}");
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].starts_with("[module-path-extension] lib.rs:1:"));
        assert!(errors[1].starts_with("[conditional-module-path] lib.rs:2:"));
    }

    #[test]
    fn allows_path_tokens_in_documentation_attributes() {
        let source = r#"#[cfg_attr(all(), doc = stringify!(path == expected))]
mod equality {}
#[cfg_attr(all(), doc = stringify!(path => expected))]
mod mapping {}"#;
        let mut errors = Vec::new();
        assert!(check_source("lib.rs", source, &mut errors), "{errors:?}");
        assert!(errors.is_empty(), "{errors:?}");
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
        let errors = diagnostics(
            root.path(),
            &crate_dir,
            &entry,
            &[
                "app/src/lib.rs",
                "app/shared.rs",
                "app/src/local.rs",
                "app/src/visible.rs",
                "other.rs",
            ],
        );
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

        let errors = diagnostics(
            root.path(),
            &crate_dir,
            &entry,
            &["app/src/lib.rs", "app/src/inside.rs", "outside.rs"],
        );
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

        let errors = diagnostics(
            root.path(),
            &crate_dir,
            &entry,
            &[
                "emp-updater/src/lib.rs",
                "emp-updater/src/nested/inside.rs",
                "emp-test-support/src/lib.rs",
            ],
        );
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

        let errors = diagnostics(
            root.path(),
            &crate_dir,
            &entry,
            &["app/src/lib.rs", "app/src/inside.rs", "other/src/lib.rs"],
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("lib.rs:2:"), "{errors:?}");
    }
}
