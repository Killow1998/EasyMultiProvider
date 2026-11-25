use super::{
    Path, Result, cfg_test, fs,
    test_scope::{Index, SourceKind},
};
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use std::{
    collections::BTreeSet,
    path::{Component, PathBuf},
};
use syn::{
    Expr, Lit, Meta,
    ext::IdentExt,
    spanned::Spanned,
    visit::{self, Visit},
};

pub(super) fn check_conditional(path: &str, source: &str, errors: &mut Vec<String>) -> Result {
    if !path.ends_with(".rs") || !source.contains("cfg_attr") || !source.contains("path") {
        return Ok(());
    }
    let file = syn::parse_file(source).map_err(|error| format!("{path}: {error}"))?;
    ConditionalPaths { path, errors }.visit_file(&file);
    Ok(())
}

struct ConditionalPaths<'a> {
    path: &'a str,
    errors: &'a mut Vec<String>,
}

impl ConditionalPaths<'_> {
    fn check(&mut self, meta: &Meta, line: usize) {
        if let Meta::List(list) = meta
            && list
                .path
                .get_ident()
                .is_some_and(|name| name.unraw() == "cfg_attr")
            && cfg_attr_sets_path(list.tokens.clone())
        {
            self.errors.push(format!(
                "[conditional-module-path] {}:{line}: 禁止通过 cfg_attr 设置模块的 path；请用 #[cfg(...)] 分别声明两个 mod，或把差异放到同一文件内的 cfg 块中",
                self.path
            ));
        }
    }
}

impl<'ast> Visit<'ast> for ConditionalPaths<'_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        for attr in &item.attrs {
            self.check(&attr.meta, attr.span().start().line);
        }
        visit::visit_item_mod(self, item);
    }

    fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
        cfg_test::visit_macro_attributes(invocation.tokens.clone(), &mut |meta, line| {
            self.check(meta, line);
        });
    }
}

fn cfg_attr_sets_path(tokens: TokenStream) -> bool {
    let tokens: Vec<_> = tokens.into_iter().collect();
    tokens
        .split(|token| matches!(token, TokenTree::Punct(punct) if punct.as_char() == ','))
        .skip(1)
        .any(|attr| match attr {
            [TokenTree::Ident(name), TokenTree::Punct(eq), ..] => {
                name.unraw() == "path" && eq.as_char() == '='
            }
            [TokenTree::Ident(name), TokenTree::Group(group)] => {
                name.unraw() == "cfg_attr"
                    && group.delimiter() == Delimiter::Parenthesis
                    && cfg_attr_sets_path(group.stream())
            }
            _ => false,
        })
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
                let kind = kind.in_test(&source.attrs);
                if let Ok(path) = resolved.strip_prefix(&self.resolved_root) {
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
        let explicit = item.attrs.iter().filter_map(|attr| {
            if let Meta::NameValue(attr) = &attr.meta
                && attr.path.is_ident("path")
                && let Expr::Lit(value) = &attr.value
                && let Lit::Str(path) = &value.lit
            {
                Some(path)
            } else {
                None
            }
        });
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
        let mut errors = Vec::new();
        check(
            root.path(),
            &crate_dir,
            std::iter::once((entry.as_path(), SourceKind::Production)),
            &mut Index::default(),
            &mut errors,
        )
        .unwrap();
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

        let mut errors = Vec::new();
        check(
            root.path(),
            &crate_dir,
            std::iter::once((entry.as_path(), SourceKind::Production)),
            &mut Index::default(),
            &mut errors,
        )
        .unwrap();
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

        let mut errors = Vec::new();
        check(
            root.path(),
            &crate_dir,
            std::iter::once((entry.as_path(), SourceKind::Production)),
            &mut Index::default(),
            &mut errors,
        )
        .unwrap();
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

        let mut errors = Vec::new();
        check(
            root.path(),
            &crate_dir,
            [(entry.as_path(), SourceKind::Production)].into_iter(),
            &mut Index::default(),
            &mut errors,
        )
        .unwrap();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("lib.rs:2:"), "{errors:?}");
    }
}
