use emp_state::generated_catalog_path;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// The generated catalog must live at exactly
/// `<resolved Codex home>/easy-multi-provider/catalog.json`: relative homes,
/// `..` components, `~` expansion and `CODEX_HOME` all have to collapse onto
/// the same absolute location before the path reaches the filesystem.
#[test]
fn generated_catalog_resolves_to_codex_home_catalog_json() {
    let _lock = ENV_LOCK.lock().expect("environment lock");
    let directory = tempfile::tempdir().expect("temporary directory");
    let home = directory
        .path()
        .canonicalize()
        .expect("canonical temporary directory")
        .join("codex-home");
    fs::create_dir_all(&home).expect("create home");
    let environment_names = ["HOME", "USERPROFILE", "CODEX_HOME"];
    let original = environment_names.map(|name| (name, std::env::var_os(name)));

    // SAFETY: this test serializes environment mutation under ENV_LOCK and
    // restores the original value on every exit path.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("USERPROFILE", &home);
        std::env::remove_var("CODEX_HOME");
    }
    let catalog = |home_root: &Path| home_root.join("easy-multi-provider").join("catalog.json");

    // Explicit Codex home: `~` and `..` spellings resolve onto the real home.
    assert_eq!(
        generated_catalog_path(Some(Path::new("~/fixture/../codex"))),
        catalog(&home.join("codex"))
    );
    assert_eq!(
        generated_catalog_path(Some(&home.join("missing/../codex"))),
        catalog(&home.join("codex"))
    );
    assert_eq!(
        generated_catalog_path(Some(Path::new("~/.codex"))),
        catalog(&home.join(".codex"))
    );

    // Relative homes resolve against the current directory, exactly like any
    // other relative user path (`std::path::absolute`, as production uses).
    assert_eq!(
        generated_catalog_path(Some(Path::new("fixture/../codex"))),
        std::path::absolute("codex")
            .expect("cwd-relative codex path")
            .join("easy-multi-provider")
            .join("catalog.json")
    );
    let cwd = std::path::absolute(".").expect("cwd path");
    assert_eq!(
        generated_catalog_path(Some(Path::new("."))),
        cwd.join("easy-multi-provider").join("catalog.json")
    );
    assert_eq!(
        generated_catalog_path(Some(Path::new(""))),
        cwd.join("easy-multi-provider").join("catalog.json")
    );

    // Without an argument CODEX_HOME wins; a blank value is ignored and falls
    // back to ~/.codex under the user home.
    // SAFETY: guarded and restored below.
    unsafe {
        std::env::set_var("CODEX_HOME", home.join("from-env"));
    }
    assert_eq!(
        generated_catalog_path(None),
        catalog(&home.join("from-env"))
    );
    // SAFETY: restore the caller environment.
    unsafe {
        for (name, value) in original {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}
