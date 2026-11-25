use super::{
    Path, Result, fs,
    test_scope::{Index, SourceKind},
};
use std::{
    collections::BTreeSet,
    path::{Component, PathBuf},
};
use syn::{Expr, Lit, LitStr, Meta, ext::IdentExt, punctuated::Punctuated, visit::Visit};

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
        let mut explicit = Vec::new();
        for attr in &item.attrs {
            path_attributes(&attr.meta, [Some(true); 2], &mut explicit);
        }
        let mut default_without_test = true;
        let mut default_with_test = true;
        for PathAttribute {
            path,
            active: [without_test, with_test],
        } in explicit
        {
            default_without_test &= without_test != Some(true);
            default_with_test &= with_test != Some(true);
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
            } else if let Some(kind) =
                path_scope(kind, without_test != Some(false), with_test != Some(false))
            {
                for target in targets {
                    if let Some((_, items)) = &item.content {
                        self.visit_inline(&target, items, kind);
                    } else {
                        self.sources.visit_file(&target, None, kind);
                    }
                }
            }
        }

        let Some(kind) = path_scope(kind, default_without_test, default_with_test) else {
            return;
        };
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

fn path_scope(kind: SourceKind, without_test: bool, with_test: bool) -> Option<SourceKind> {
    if without_test {
        Some(kind)
    } else {
        with_test.then(|| kind.max(SourceKind::Fixture))
    }
}

struct PathAttribute {
    path: LitStr,
    active: [Option<bool>; 2],
}

fn path_attributes(meta: &Meta, active: [Option<bool>; 2], paths: &mut Vec<PathAttribute>) {
    match meta {
        Meta::NameValue(attr) if attr.path.is_ident("path") => {
            if let Expr::Lit(value) = &attr.value
                && let Lit::Str(path) = &value.lit
            {
                paths.push(PathAttribute {
                    path: path.clone(),
                    active,
                });
            }
        }
        Meta::List(attr) if attr.path.is_ident("cfg_attr") => {
            if let Ok(parts) =
                attr.parse_args_with(Punctuated::<Meta, syn::Token![,]>::parse_terminated)
                && let Some(condition) = parts.first()
            {
                let [without_test, with_test] = active;
                let active = [
                    cfg_and(without_test, cfg_value(condition, false)),
                    cfg_and(with_test, cfg_value(condition, true)),
                ];
                for part in parts.iter().skip(1) {
                    path_attributes(part, active, paths);
                }
            }
        }
        _ => {}
    }
}

fn cfg_and(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

fn cfg_value(meta: &Meta, test: bool) -> Option<bool> {
    match meta {
        Meta::Path(path) if path.is_ident("test") => Some(test),
        Meta::List(list) => {
            let parts = list
                .parse_args_with(Punctuated::<Meta, syn::Token![,]>::parse_terminated)
                .ok()?;
            if list.path.is_ident("not") && parts.len() == 1 {
                return cfg_value(parts.first()?, test).map(|value| !value);
            }
            let identity = if list.path.is_ident("all") {
                true
            } else if list.path.is_ident("any") {
                false
            } else {
                return None;
            };
            let mut known = true;
            for part in parts {
                match cfg_value(&part, test) {
                    Some(value) if value != identity => return Some(value),
                    None => known = false,
                    _ => {}
                }
            }
            known.then_some(identity)
        }
        _ => None,
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
#[cfg_attr(windows, path = "../../missing.rs")] mod windows_only;
#[cfg_attr(windows, path = "missing.rs")] mod local_windows_only;
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
