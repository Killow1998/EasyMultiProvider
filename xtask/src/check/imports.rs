//! 为 lint 识别常规写法中的失误。`use` 限于同一文件，别名按声明作用域展开至不动点。
//! 规则处理带初始化式的 `let` 和简单赋值 `x = …`。有意规避、局部遮蔽分析、
//! 跨函数或跨文件的数据流和宏生成的名字超出适用范围。

use std::collections::{BTreeMap, BTreeSet};
use syn::{Item, ItemUse, Path, Stmt, UseTree, ext::IdentExt};

#[derive(Default)]
pub(super) struct Imports(Vec<BTreeMap<String, Import>>);

struct Import {
    absolute: bool,
    segments: Vec<String>,
}

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
        if path.leading_colon.is_some() {
            return segments;
        }
        let mut scopes = &self.0[..];
        let mut visited = BTreeSet::new();
        while let Some((index, name, import)) =
            scopes.iter().enumerate().rev().find_map(|(index, scope)| {
                let (name, import) = scope.get_key_value(segments.first()?)?;
                Some((index, name, import))
            })
        {
            if !visited.insert((index, name)) {
                break;
            }
            let terminal = import.absolute || import.segments.first() == Some(name);
            segments.splice(..1, import.segments.iter().cloned());
            if terminal {
                break;
            }
            scopes = &scopes[..=index];
        }
        segments
    }

    fn enter<'ast>(&mut self, items: impl Iterator<Item = &'ast ItemUse>) {
        let mut scope = BTreeMap::new();
        for item in items {
            add_import(
                &item.tree,
                &mut Vec::new(),
                item.leading_colon.is_some(),
                &mut scope,
            );
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

fn add_import(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    absolute: bool,
    scope: &mut BTreeMap<String, Import>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.unraw().to_string());
            add_import(&path.tree, prefix, absolute, scope);
            prefix.pop();
        }
        UseTree::Group(group) => {
            for entry in &group.items {
                add_import(entry, prefix, absolute, scope);
            }
        }
        UseTree::Name(name) if name.ident == "self" => {
            if let Some(name) = prefix.last() {
                scope.insert(
                    name.clone(),
                    Import {
                        absolute,
                        segments: prefix.clone(),
                    },
                );
            }
        }
        UseTree::Name(name) => {
            let name = name.ident.unraw().to_string();
            let mut segments = prefix.clone();
            segments.push(name.clone());
            scope.insert(name, Import { absolute, segments });
        }
        UseTree::Rename(rename) => {
            let mut segments = prefix.clone();
            if rename.ident != "self" {
                segments.push(rename.ident.unraw().to_string());
            }
            scope.insert(
                rename.rename.unraw().to_string(),
                Import { absolute, segments },
            );
        }
        UseTree::Glob(_) => {}
    }
}
