use emp_state::generated_catalog_path;
use serde_json::json;
use std::path::Path;

/// The generated catalog must live under an absolute, fully resolved Codex
/// home regardless of how the user spelled it; `..` components and relative
/// homes have to collapse before the path reaches the filesystem.
#[test]
fn generated_catalog_resolves_relative_and_home_paths() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let cases = json!([
        null,
        "",
        ".",
        "fixture/../codex",
        "~/.codex",
        "~/fixture/../codex",
        directory.path().join("missing/../codex")
    ]);
    let actual = cases
        .as_array()
        .expect("paths")
        .iter()
        .map(|case| json!(generated_catalog_path(case.as_str().map(Path::new))))
        .collect::<Vec<_>>();
    for value in &actual {
        let path = Path::new(value.as_str().expect("path"));
        assert!(path.is_absolute(), "catalog path must be absolute: {value}");
        assert!(
            !path
                .components()
                .any(|component| component == std::path::Component::ParentDir),
            "catalog path must not contain .. components: {value}"
        );
    }
    assert!(
        actual[4]
            .as_str()
            .expect("home case")
            .ends_with(".codex/easy-multi-provider/catalog.json"),
        "default home expansion: {}",
        actual[4]
    );
    assert!(
        actual[6]
            .as_str()
            .expect("tempdir case")
            .ends_with("codex/easy-multi-provider/catalog.json"),
        "dotdot components must resolve: {}",
        actual[6]
    );
}
