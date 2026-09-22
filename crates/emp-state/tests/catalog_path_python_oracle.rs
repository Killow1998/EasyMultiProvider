use emp_state::generated_catalog_path;
use serde_json::{Value, json};
use std::path::Path;
use std::process::{Command, Stdio};

#[test]
fn catalog_paths_match_python_for_relative_and_home_paths() {
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
    assert!(
        actual
            .iter()
            .all(|value| Path::new(value.as_str().expect("path")).is_absolute())
    );
    if let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut child = Command::new(python)
            .args(["-c", r#"
import json, os, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from easy_multi_provider.catalog import generated_catalog_path
paths = json.load(sys.stdin)
json.dump([str(generated_catalog_path(Path(path) if path is not None else None)) for path in paths], sys.stdout)
"#])
            .arg(root)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().expect("Python catalog path oracle");
        serde_json::to_writer(child.stdin.take().expect("stdin"), &cases).expect("fixture");
        let output = child.wait_with_output().expect("oracle output");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected: Value = serde_json::from_slice(&output.stdout).expect("oracle JSON");
        assert_eq!(json!(actual), expected);
    }
}
