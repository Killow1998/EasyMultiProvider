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

#[test]
fn validates_merge_revisions_and_deduplicates_short_and_full_ids() {
    let repo = fixture_repository();
    let root = repo.path();
    let tree = fixture_git(root, &["write-tree"]);
    let base = fixture_git(root, &["commit-tree", &tree, "-m", "base"]);
    let first = fixture_git(root, &["commit-tree", &tree, "-p", &base, "-m", "first"]);
    let second = fixture_git(root, &["commit-tree", &tree, "-p", &base, "-m", "second"]);
    let merge = fixture_git(
        root,
        &[
            "commit-tree",
            &tree,
            "-p",
            &first,
            "-p",
            &second,
            "-m",
            "merge",
        ],
    );
    let short = fixture_git(root, &["rev-parse", "--short", &merge]);
    assert_eq!(
        merge_commits(root, &[&short, &merge]).unwrap(),
        [(short.as_str(), merge.clone())]
    );

    let non_merge = fixture_git(root, &["rev-parse", "--short", &first]);
    let error = merge_commits(root, &[&non_merge]).unwrap_err();
    assert!(
        error.contains(&non_merge) && error.contains("not a merge commit"),
        "{error}"
    );

    let missing = "missing-merge";
    let error = merge_commits(root, &[missing]).unwrap_err();
    assert!(error.contains(missing), "{error}");
}

#[test]
fn counts_rust_when_the_git_tree_has_a_non_utf8_non_rust_path() {
    // Built with mktree so every platform sees the raw path bytes; some file
    // systems (APFS) refuse to create such a name on disk.
    let repo = fixture_repository();
    let root = repo.path();
    let source = fixture_blob(root, b"fn example() {}\n");
    let image = fixture_blob(root, b"data");
    let mut entries = Vec::new();
    entries.extend_from_slice(format!("100644 blob {source}\tlib.rs\0").as_bytes());
    entries.extend_from_slice(format!("100644 blob {image}\timage-").as_bytes());
    entries.extend_from_slice(b"\xff.dat\0");
    let tree = git_with_input(git_in(root).args(["mktree", "-z"]), entries).unwrap();
    let tree = String::from_utf8(tree).unwrap().trim().to_owned();

    let counts = Counter::new().count(&read_tree(root, &tree).unwrap());
    assert_eq!(counts.len(), 1);
    assert_eq!(counts["lib.rs"].production, 1);
}

#[test]
fn git_children_drop_the_callers_repository_location() {
    // Hooks export GIT_DIR / GIT_INDEX_FILE; a child inheriting them would
    // read or stage into the caller's repository instead of `root`.
    let command = git_in(Path::new("."));
    let removed = command
        .get_envs()
        .filter(|(_, value)| value.is_none())
        .map(|(name, _)| name.to_str().unwrap())
        .collect::<BTreeSet<_>>();
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
    ] {
        assert!(removed.contains(name), "{name} reaches git children");
    }
}

fn fixture_blob(root: &Path, contents: &[u8]) -> String {
    let id = git_with_input(
        git_in(root).args(["hash-object", "-w", "--stdin"]),
        contents.to_vec(),
    )
    .unwrap();
    String::from_utf8(id).unwrap().trim().to_owned()
}

fn fixture_repository() -> tempfile::TempDir {
    let root = emp_test_support::fixture_dir().unwrap();
    fixture_git(root.path(), &["init", "--quiet"]);
    root
}

fn fixture_git(root: &Path, args: &[&str]) -> String {
    let output = git(git_in(root)
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .args(args))
    .unwrap();
    String::from_utf8(output).unwrap().trim().to_owned()
}
