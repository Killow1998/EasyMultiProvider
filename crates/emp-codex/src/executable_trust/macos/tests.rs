use super::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;

fn fixture(root: &Path) -> PathBuf {
    let bundle = root.join("source/CodexCLI.app");
    fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
    fs::write(bundle.join("Contents/Info.plist"), b"sealed fixture").unwrap();
    let engine = bundle.join(ENGINE_RELATIVE);
    fs::write(&engine, b"#!/bin/sh\necho safe\n").unwrap();
    fs::set_permissions(&engine, fs::Permissions::from_mode(0o700)).unwrap();
    bundle
}

#[test]
fn only_nested_known_desktop_layouts_are_candidates() {
    for app in ["ChatGPT.app", "Codex.app"] {
        let path = Path::new("/Applications")
            .join(app)
            .join("Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex");
        assert!(known_bundle(&path).is_some());
    }
    for path in [
        "/Applications/Other.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
        "/Users/example/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
        "/Applications/ChatGPT.app/Contents/Resources/codex",
    ] {
        assert!(known_bundle(Path::new(path)).is_none());
    }
}

#[test]
fn signature_verifier_fails_closed_on_missing_tool_failure_and_timeout() {
    assert!(!verify_bundle(
        Path::new("/nonexistent/app"),
        Path::new("/nonexistent/codex")
    ));
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "exit 3"]);
    assert!(!run_verifier(&mut command, Duration::from_secs(1)));
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "exec sleep 10"]);
    let started = Instant::now();
    assert!(!run_verifier(&mut command, Duration::from_millis(30)));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn authenticated_info_must_bind_the_exact_codex_engine() {
    let root = tempfile::tempdir().unwrap();
    let bundle = fixture(root.path());
    let executable = bundle.join(ENGINE_RELATIVE).canonicalize().unwrap();
    let info = bundle.join("Contents/Info.plist");
    assert!(
        !bundle_binds_engine(&bundle, &executable),
        "malformed plist"
    );
    for value in [
        plist::Value::String("other".into()),
        plist::Value::String("../codex".into()),
        plist::Value::Boolean(true),
    ] {
        let mut dictionary = plist::Dictionary::new();
        dictionary.insert("CFBundleExecutable".into(), value);
        plist::Value::Dictionary(dictionary)
            .to_file_xml(&info)
            .unwrap();
        assert!(!bundle_binds_engine(&bundle, &executable));
    }
    let mut dictionary = plist::Dictionary::new();
    dictionary.insert(
        "CFBundleExecutable".into(),
        plist::Value::String("codex".into()),
    );
    let value = plist::Value::Dictionary(dictionary);
    for binary in [false, true] {
        if binary {
            value.to_file_binary(&info).unwrap();
        } else {
            value.to_file_xml(&info).unwrap();
        }
        assert!(bundle_binds_engine(&bundle, &executable));
        assert!(!bundle_binds_engine(
            &bundle,
            &root.path().join("another-codex")
        ));
    }
    fs::write(&info, vec![b' '; 256 * 1024 + 1]).unwrap();
    assert!(
        !bundle_binds_engine(&bundle, &executable),
        "oversized plist"
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires a verified official macOS App Bundle"]
fn installed_macos_signed_bundle_launch() {
    let app = PathBuf::from(std::env::var_os("EMP_MACOS_DIAGNOSTIC_APP").unwrap());
    let bundle = app.join("Contents/Resources/codex-cli/CodexCLI.app");
    if !bundle.exists() {
        assert!(app.join("Contents/Resources/codex").is_file());
        return;
    }

    let executable = bundle.join(ENGINE_RELATIVE).canonicalize().unwrap();
    if known_bundle(&executable).is_some() {
        let prepared = crate::PreparedExecutable::prepare(&executable).unwrap();
        assert_eq!(prepared.path(), executable);
        assert_eq!(prepared.quota_selector(), executable);
    } else {
        assert!(verify_bundle(&bundle, &executable));
    }
    let mut command = Command::new(executable);
    command.arg("--version");
    assert!(run_verifier(&mut command, Duration::from_secs(5)));

    let root = tempfile::tempdir().unwrap();
    let unsigned = fixture(root.path());
    assert!(!verify_bundle(&unsigned, &unsigned.join(ENGINE_RELATIVE)));
}
