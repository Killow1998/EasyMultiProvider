use super::*;
use crate::executable_trust::PreparedExecutable;

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
fn snapshot_survives_source_swap_and_is_owned_until_child_finishes() {
    let root = tempfile::tempdir().unwrap();
    let source = fixture(root.path());
    let (directory, executable) =
        snapshot_with_verifier(&source, Some(root.path()), |bundle, engine| {
            assert_ne!(engine, source.join(ENGINE_RELATIVE));
            assert_eq!(
                fs::read(bundle.join("Contents/Info.plist")).unwrap(),
                b"sealed fixture"
            );
            assert_eq!(fs::read(engine).unwrap(), b"#!/bin/sh\necho safe\n");
            fs::rename(&source, root.path().join("old.app")).unwrap();
            fs::create_dir_all(source.join("Contents/MacOS")).unwrap();
            fs::write(source.join(ENGINE_RELATIVE), b"#!/bin/sh\necho replaced\n").unwrap();
            true
        })
        .unwrap();
    let root_path = directory.path().to_owned();
    assert_eq!(fs::metadata(&root_path).unwrap().mode() & 0o777, 0o700);
    assert_ne!(
        fs::metadata(&executable).unwrap().ino(),
        fs::metadata(root.path().join("old.app").join(ENGINE_RELATIVE))
            .unwrap()
            .ino()
    );
    let prepared = PreparedExecutable::snapshot(directory, executable);
    assert_eq!(prepared.path(), prepared.quota_selector());
    let mut child = Command::new(prepared.path())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    assert!(prepared.path().exists());
    let output = child.stdout.take().unwrap();
    let mut text = String::new();
    std::io::BufReader::new(output)
        .read_to_string(&mut text)
        .unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(text, "safe\n");
    drop(prepared);
    assert!(!root_path.exists());
}

#[test]
fn rejected_signature_or_link_leaves_no_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let source = fixture(root.path());
    let mut snapshot_root = None;
    assert!(
        snapshot_with_verifier(&source, Some(root.path()), |bundle, _| {
            snapshot_root = Some(bundle.parent().unwrap().to_owned());
            false
        })
        .is_err()
    );
    assert!(!snapshot_root.unwrap().exists());
    std::os::unix::fs::symlink("/nonexistent", source.join("Contents/escape")).unwrap();
    assert!(
        snapshot_with_verifier(&source, Some(root.path()), |_, _| panic!(
            "link reached verifier"
        ))
        .is_err()
    );
    assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("emp-codex-engine-")
    }));
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
    assert!(!verify_snapshot(
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
