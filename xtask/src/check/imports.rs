use std::collections::{BTreeMap, BTreeSet};
use syn::{Item, ItemMod, Path, Stmt, UseTree, ext::IdentExt};

pub(super) struct Imports<'source> {
    scopes: Vec<Scope>,
    modules: BTreeMap<(usize, usize), usize>,
    file_module: Option<Vec<&'source str>>,
    current: usize,
}

struct Scope {
    bindings: BTreeMap<String, Binding>,
    parent: Option<usize>,
    module: usize,
}

enum Binding {
    Import {
        absolute: bool,
        segments: Vec<String>,
    },
    Constant(usize),
    Module(usize),
}

impl<'source> Imports<'source> {
    pub(super) fn file(path: &'source str, items: &[Item]) -> Self {
        let mut imports = Self {
            scopes: Vec::new(),
            modules: BTreeMap::new(),
            file_module: file_module(path),
            current: 0,
        };
        imports.add_scope(None, true, items.iter());
        imports
    }

    pub(super) fn enter_module(&mut self, module: &ItemMod) {
        let start = module.ident.span().start();
        self.current = self.modules[&(start.line, start.column)];
    }

    pub(super) fn enter_block(&mut self, block: &syn::Block) {
        self.current = self.add_scope(
            Some(self.current),
            false,
            block.stmts.iter().filter_map(|stmt| match stmt {
                Stmt::Item(item) => Some(item),
                _ => None,
            }),
        );
    }

    pub(super) fn leave_scope(&mut self) {
        self.current = self.scopes[self.current].parent.expect("nested scope");
    }

    pub(super) fn resolve(&self, path: &Path) -> Vec<String> {
        self.resolve_definition(path).0
    }

    pub(super) fn resolve_definition(&self, path: &Path) -> (Vec<String>, Option<usize>) {
        let mut segments: Vec<_> = path
            .segments
            .iter()
            .map(|segment| segment.ident.unraw().to_string())
            .collect();
        if path.leading_colon.is_some() {
            return (segments, None);
        }
        let mut scope = self.current;
        let mut visited = BTreeSet::new();
        while let Some((index, declared_in, binding)) = self.binding(&segments, scope) {
            match binding {
                Binding::Constant(line) if index + 1 == segments.len() => {
                    return (segments, Some(*line));
                }
                Binding::Import {
                    absolute,
                    segments: target,
                } => {
                    let name = &segments[index];
                    if !visited.insert((declared_in, name.clone())) {
                        break;
                    }
                    let terminal = *absolute || (index == 0 && target.first() == Some(name));
                    segments.splice(..=index, target.iter().cloned());
                    if terminal {
                        break;
                    }
                    scope = declared_in;
                }
                _ => break,
            }
        }
        (segments, None)
    }

    fn binding(&self, segments: &[String], scope: usize) -> Option<(usize, usize, &Binding)> {
        let mut module = self.scopes[scope].module;
        let (mut index, mut declared_in) = match segments.first()?.as_str() {
            "self" => (1, module),
            "super" => {
                let parents = segments.iter().take_while(|name| *name == "super").count();
                for _ in 0..parents {
                    module = self.scopes[self.scopes[module].parent?].module;
                }
                (parents, module)
            }
            "crate" => {
                let file_module = self.file_module.as_ref()?;
                let offset = 1 + file_module.len();
                if segments.get(1..offset)? != file_module.as_slice() {
                    return None;
                }
                (offset, 0)
            }
            _ => (0, scope),
        };
        let mut binding = loop {
            let scope = &self.scopes[declared_in];
            if let Some(binding) = scope.bindings.get(segments.get(index)?) {
                break binding;
            }
            if declared_in == scope.module {
                return None;
            }
            declared_in = scope.parent?;
        };
        while let Binding::Module(module) = binding {
            declared_in = *module;
            index += 1;
            binding = self.scopes[declared_in]
                .bindings
                .get(segments.get(index)?)?;
        }
        Some((index, declared_in, binding))
    }

    fn add_scope<'ast>(
        &mut self,
        parent: Option<usize>,
        module_scope: bool,
        items: impl Iterator<Item = &'ast Item>,
    ) -> usize {
        let index = self.scopes.len();
        let module = if module_scope {
            index
        } else {
            self.scopes[parent.expect("block scope parent")].module
        };
        self.scopes.push(Scope {
            bindings: BTreeMap::new(),
            parent,
            module,
        });
        let mut bindings = BTreeMap::new();
        for item in items {
            let (ident, binding) = match item {
                Item::Use(item) => {
                    add_import(
                        &item.tree,
                        &mut Vec::new(),
                        item.leading_colon.is_some(),
                        &mut bindings,
                    );
                    continue;
                }
                Item::Const(item) => (
                    &item.ident,
                    Binding::Constant(item.ident.span().start().line),
                ),
                Item::Static(item) => (
                    &item.ident,
                    Binding::Constant(item.ident.span().start().line),
                ),
                Item::Mod(item) => {
                    let Some((_, items)) = &item.content else {
                        continue;
                    };
                    let child = self.add_scope(Some(index), true, items.iter());
                    let start = item.ident.span().start();
                    self.modules.insert((start.line, start.column), child);
                    (&item.ident, Binding::Module(child))
                }
                _ => continue,
            };
            bindings.insert(ident.unraw().to_string(), binding);
        }
        self.scopes[index].bindings = bindings;
        index
    }
}

fn file_module(path: &str) -> Option<Vec<&str>> {
    let path = path.strip_suffix(".rs")?;
    if let Some((_, relative)) = path.split_once("/src/") {
        let mut parts: Vec<_> = relative.split('/').collect();
        match parts.as_slice() {
            ["lib" | "main"] | ["bin", _] | ["bin", _, "main"] => parts.clear(),
            [.., "mod"] => {
                parts.pop();
            }
            _ => {}
        }
        Some(parts)
    } else if matches!(
        path.split('/').collect::<Vec<_>>().as_slice(),
        ["crates", _, "tests", _] | ["xtask", "tests", _] | ["tests", _]
    ) {
        Some(Vec::new())
    } else {
        None
    }
}

fn add_import(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    absolute: bool,
    scope: &mut BTreeMap<String, Binding>,
) {
    let (name, target) = match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.unraw().to_string());
            add_import(&path.tree, prefix, absolute, scope);
            prefix.pop();
            return;
        }
        UseTree::Group(group) => {
            for entry in &group.items {
                add_import(entry, prefix, absolute, scope);
            }
            return;
        }
        UseTree::Name(name) if name.ident == "self" => {
            let Some(name) = prefix.last() else { return };
            (name.clone(), None)
        }
        UseTree::Name(name) => {
            let name = name.ident.unraw().to_string();
            (name.clone(), Some(name))
        }
        UseTree::Rename(rename) => (
            rename.rename.unraw().to_string(),
            (rename.ident != "self").then(|| rename.ident.unraw().to_string()),
        ),
        UseTree::Glob(_) => return,
    };
    let mut segments = prefix.clone();
    segments.extend(target);
    scope.insert(name, Binding::Import { absolute, segments });
}
