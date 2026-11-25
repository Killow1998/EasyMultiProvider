use super::{Path, Result, comments, read};
use std::collections::BTreeMap;
use syn::visit::{self, Visit};

const BASELINE: &[(&str, usize)] = &[
    ("emp-app", 97),
    ("emp-codex", 15),
    ("emp-config", 3),
    ("emp-context", 1),
    ("emp-protocol", 2),
    ("emp-router", 1),
    ("emp-storage", 3),
    ("emp-updater", 4),
];

pub(super) fn check(root: &Path, paths: &[String], errors: &mut Vec<String>) -> Result {
    let mut counts: BTreeMap<_, usize> = BASELINE.iter().map(|&(group, _)| (group, 0)).collect();
    for path in paths {
        if !path.ends_with(".rs") || comments::test_file(path) || !root.join(path).is_file() {
            continue;
        }
        let mut parts = path.split('/');
        let top = parts.next().expect("source path");
        let group = if top == "crates" {
            parts.next().expect("crate name")
        } else {
            top
        };
        *counts.entry(group).or_default() +=
            count(&read(root, path)?).map_err(|error| format!("{path}: {error}"))?;
    }
    for (group, actual) in counts {
        let baseline = BASELINE
            .iter()
            .find_map(|&(name, count)| (name == group).then_some(count))
            .unwrap_or_default();
        errors.extend(mismatch(group, actual, baseline));
    }
    Ok(())
}

fn mismatch(group: &str, actual: usize, baseline: usize) -> Option<String> {
    (actual != baseline).then(|| format!(
        "[cfg-test] {group}: 非测试文件的测试专用 cfg 共 {actual} 项，基线为 {baseline}；新增探针应收拢到现有测试模块，减少后须同步调低基线"
    ))
}

fn count(source: &str) -> syn::Result<usize> {
    if !source.contains("cfg") || !source.contains("test") {
        return Ok(0);
    }
    let mut cfg = TestCfg::default();
    cfg.visit_file(&syn::parse_file(source)?);
    Ok(cfg.0)
}

#[derive(Default)]
struct TestCfg(usize);

impl<'ast> Visit<'ast> for TestCfg {
    fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
        if attribute.path().is_ident("cfg")
            && attribute
                .parse_args::<syn::Meta>()
                .is_ok_and(|meta| comments::requires_test(&meta))
        {
            self.0 += 1;
        }
    }

    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        if module.ident == "tests" {
            if let Some((_, items)) = &module.content {
                for item in items {
                    self.visit_item(item);
                }
            }
        } else {
            visit::visit_item_mod(self, module);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_test_only_fields_and_hooks_but_exempts_test_module_declarations() {
        let source = r##"
#[cfg(test)]
mod tests;
#[cfg(all(test, unix))]
mod probes {}
#[cfg(not(test))]
mod probes {}
struct State {
    #[cfg(test)]
    hook: (),
}
fn drive() {
    #[cfg(test)]
    hook();
    #[cfg(any(test, unix))]
    close();
}
const FIXTURE: &str = "#[cfg(test)]";
"##;
        assert_eq!(count(source).unwrap(), 3);
    }

    #[test]
    fn reports_increases_and_decreases_until_the_count_matches() {
        assert!(mismatch("example", 2, 1).is_some());
        assert!(mismatch("example", 0, 1).is_some());
        assert!(mismatch("example", 1, 1).is_none());
    }
}
