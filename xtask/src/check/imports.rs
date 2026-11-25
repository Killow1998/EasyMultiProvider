use std::collections::BTreeMap;
use syn::{Item, ItemUse, Path, Stmt, UseTree, ext::IdentExt};

#[derive(Default)]
pub(super) struct Imports(Vec<BTreeMap<String, Vec<String>>>);

impl Imports {
    pub(super) fn module(items: &[Item]) -> Self {
        let mut imports = Self::default();
        imports.enter(items.iter().filter_map(item_import));
        imports
    }

    pub(super) fn enter_block(&mut self, block: &syn::Block) {
        self.enter(block.stmts.iter().filter_map(|stmt| match stmt {
            Stmt::Item(item) => item_import(item),
            _ => None,
        }));
    }

    pub(super) fn leave_block(&mut self) {
        self.0.pop();
    }

    pub(super) fn resolve(&self, path: &Path) -> Vec<String> {
        let mut segments: Vec<_> = path
            .segments
            .iter()
            .map(|segment| segment.ident.unraw().to_string())
            .collect();
        if path.leading_colon.is_none()
            && let Some(first) = segments.first()
            && let Some(import) = self.0.iter().rev().find_map(|scope| scope.get(first))
        {
            segments.splice(..1, import.iter().cloned());
        }
        segments
    }

    fn enter<'ast>(&mut self, items: impl Iterator<Item = &'ast ItemUse>) {
        let mut scope = BTreeMap::new();
        for item in items {
            add_import(&item.tree, &mut Vec::new(), &mut scope);
        }
        self.0.push(scope);
    }
}

fn item_import(item: &Item) -> Option<&ItemUse> {
    match item {
        Item::Use(item) => Some(item),
        _ => None,
    }
}

fn add_import(tree: &UseTree, prefix: &mut Vec<String>, scope: &mut BTreeMap<String, Vec<String>>) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.unraw().to_string());
            add_import(&path.tree, prefix, scope);
            prefix.pop();
        }
        UseTree::Group(group) => {
            for entry in &group.items {
                add_import(entry, prefix, scope);
            }
        }
        UseTree::Name(name) if name.ident == "self" => {
            if let Some(name) = prefix.last() {
                scope.insert(name.clone(), prefix.clone());
            }
        }
        UseTree::Name(name) => {
            let name = name.ident.unraw().to_string();
            let mut path = prefix.clone();
            path.push(name.clone());
            scope.insert(name, path);
        }
        UseTree::Rename(rename) => {
            let mut path = prefix.clone();
            if rename.ident != "self" {
                path.push(rename.ident.unraw().to_string());
            }
            scope.insert(rename.rename.unraw().to_string(), path);
        }
        UseTree::Glob(_) => {}
    }
}
