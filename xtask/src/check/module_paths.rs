use super::{Path, Result, fs};
use std::{
    collections::BTreeSet,
    path::{Component, PathBuf},
};
use syn::{Expr, Lit, LitStr, Meta, ext::IdentExt, punctuated::Punctuated, visit::Visit};

pub(super) fn check<'a>(
    root: &Path,
    crate_dir: &Path,
    targets: impl Iterator<Item = &'a Path>,
    errors: &mut Vec<String>,
) -> Result {
    let mut sources = CrateSources {
        root,
        crate_dir: crate_dir
            .canonicalize()
            .map_err(|error| format!("{}: {error}", crate_dir.display()))?,
        visited: BTreeSet::new(),
        errors,
    };
    for target in targets {
        sources.visit_file(target, None);
    }
    Ok(())
}

struct CrateSources<'a> {
    root: &'a Path,
    crate_dir: PathBuf,
    visited: BTreeSet<(PathBuf, Option<String>)>,
    errors: &'a mut Vec<String>,
}

impl CrateSources<'_> {
    fn visit_file(&mut self, file: &Path, module: Option<String>) {
        if !file.is_file() || !self.visited.insert((resolve(file), module.clone())) {
            return;
        }
        let source = fs::read_to_string(file)
            .map_err(|error| error.to_string())
            .and_then(|source| syn::parse_file(&source).map_err(|error| error.to_string()));
        match source {
            Ok(source) => ModulePaths {
                sources: self,
                file,
                directory: file.parent().expect("absolute Rust source path"),
                module,
            }
            .visit_file(&source),
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
}

impl<'ast> Visit<'ast> for ModulePaths<'_, '_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let mut explicit = Vec::new();
        for attr in &item.attrs {
            path_attributes(&attr.meta, &mut explicit);
        }
        for path in explicit {
            let value = path.value();
            let portable = value.replace('\\', "/");
            let rooted = portable.starts_with('/')
                || matches!(portable.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic());
            let mut targets = BTreeSet::from([self.directory.join(&value)]);
            targets.insert(self.directory.join(&portable));
            if rooted
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
                        self.visit_inline(&target, items);
                    } else {
                        self.sources.visit_file(&target, None);
                    }
                }
            }
        }

        if item.attrs.iter().any(|attr| attr.path().is_ident("path")) {
            return;
        }
        let mut directory = self.directory.to_owned();
        if let Some(module) = &self.module {
            directory.push(module);
        }
        let name = item.ident.unraw().to_string();
        let child = directory.join(&name);
        if let Some((_, items)) = &item.content {
            self.visit_inline(&child, items);
        } else {
            self.sources
                .visit_file(&child.with_extension("rs"), Some(name));
            self.sources.visit_file(&child.join("mod.rs"), None);
        }
    }
}

impl ModulePaths<'_, '_> {
    fn visit_inline(&mut self, directory: &Path, items: &[syn::Item]) {
        let mut visitor = ModulePaths {
            sources: self.sources,
            file: self.file,
            directory,
            module: None,
        };
        for item in items {
            visitor.visit_item(item);
        }
    }
}

fn path_attributes(meta: &Meta, paths: &mut Vec<LitStr>) {
    match meta {
        Meta::NameValue(attr) if attr.path.is_ident("path") => {
            if let Expr::Lit(value) = &attr.value
                && let Lit::Str(path) = &value.lit
            {
                paths.push(path.clone());
            }
        }
        Meta::List(attr) if attr.path.is_ident("cfg_attr") => {
            if let Ok(parts) =
                attr.parse_args_with(Punctuated::<Meta, syn::Token![,]>::parse_terminated)
            {
                for part in parts.iter().skip(1) {
                    path_attributes(part, paths);
                }
            }
        }
        _ => {}
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
            std::iter::once(entry.as_path()),
            &mut errors,
        )
        .unwrap();
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors[0].contains("lib.rs:3"));
        assert!(errors[1].contains("lib.rs:4"));
        assert!(errors[2].contains("local.rs:4"));
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
            std::iter::once(entry.as_path()),
            &mut errors,
        )
        .unwrap();
        assert_eq!(errors.len(), 7, "{errors:?}");
        for (error, line) in errors.iter().zip([2, 3, 5, 6, 7, 8, 9]) {
            assert!(error.contains(&format!("lib.rs:{line}:")), "{error}");
        }
    }
}
