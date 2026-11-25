use super::*;

#[test]
fn classifies_git_rust_paths_and_counts_physical_lines() {
    let files = [
        ("src/lib.rs", "pub fn first() {}\r\n\r\n#[cfg(test)]\n#[path = \"fixtures/../harness/mod.rs\"]\npub(crate) mod probe;\n#[cfg(unix)]\npub fn last() {}"),
        ("src/probe/helper.rs", "fn helper() {}\n"),
        ("src/harness/mod.rs", "mod helper;\n"),
        ("src/harness/helper.rs", "fn helper() {}\n"),
        ("src/driver.rs", "#[cfg(test)]\nmod checks;\n"),
        ("src/driver/checks.rs", "mod worker;\n"),
        ("src/driver/checks/worker.rs", "fn worker() {}\n"),
        ("src/another_tests.rs", "fn helper() {}\n"),
        ("src/conditional.rs", "\n#![cfg(test)]\n// counted as a test\n"),
        ("tests/contract.rs", "fn check() {}\n"),
        ("benches/throughput.rs", "fn main() {}\n"),
        ("examples/probe.rs", "fn main() {}\n#[cfg(all(test, unix))]\nfn check() {}\n"),
        ("xtask/src/main.rs", "fn main() {}\n"),
        ("crates/emp-test-support/src/lib.rs", "fn helper() {}\n"),
    ].into_iter().map(|(path, content)| (path.into(), content.into())).collect();

    let counts = Counter::new().count(&files);
    assert_eq!(
        Lines::totals(&counts, ""),
        Lines {
            production: 4,
            tests: 18,
            examples: 1
        }
    );
    assert_eq!(
        Lines::totals(&counts, "src/driver"),
        Lines {
            production: 0,
            tests: 4,
            examples: 0
        }
    );
    assert!(!counts.contains_key("xtask/src/main.rs"));
    assert!(!counts.contains_key("crates/emp-test-support/src/lib.rs"));
}

#[test]
fn inline_test_spans_do_not_consume_following_items_or_recount_nested_tests() {
    let source = "\
#[cfg(test)]
#[allow(dead_code)]
mod outer {
    #[cfg(test)]
    mod inner {}
}
fn production() {}
#[cfg(all(test, unix))]
const ITEM: u32 = 1;
#[cfg(windows)]
fn more_production() {}";
    assert_eq!(Counter::new().inline_test_lines(source), 8);
}

#[test]
fn accepts_repeated_merge_and_prefix_options() {
    let args = [
        "base",
        "head",
        "--subtract-merge",
        "merge-one",
        "--prefix",
        "src/",
        "--subtract-merge",
        "merge-two",
        "--prefix",
        "tests/",
    ]
    .map(str::to_owned);
    let options = Options::parse(&args).unwrap();
    assert_eq!(options.merges, ["merge-one", "merge-two"]);
    assert_eq!(options.prefixes, ["src/", "tests/"]);
    assert!(Options::parse(&args[..3]).is_err());
    assert!(Options::parse(&args[..1]).is_err());
}
