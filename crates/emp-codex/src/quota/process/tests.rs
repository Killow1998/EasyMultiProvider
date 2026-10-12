use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn stale_credential_directories_are_swept_but_recent_and_foreign_entries_stay() {
    let root = tempfile::tempdir().expect("temporary root");
    let stale = root
        .path()
        .join(format!("{CREDENTIAL_DIRECTORY_PREFIX}stale"));
    let recent = root
        .path()
        .join(format!("{CREDENTIAL_DIRECTORY_PREFIX}recent"));
    let unrelated = root.path().join("unrelated-stale");
    let target = root.path().join("link-target");
    for directory in [&stale, &recent, &unrelated, &target] {
        fs::create_dir(directory).expect("create directory");
    }
    fs::write(stale.join("auth.json"), b"{}").expect("stale credential");
    let link = root
        .path()
        .join(format!("{CREDENTIAL_DIRECTORY_PREFIX}link"));
    std::os::unix::fs::symlink(&target, &link).expect("symlink");

    // Everything created just now is two hours old from `now`'s view,
    // except `recent`, which is judged against its own modification time.
    let later = SystemTime::now() + Duration::from_secs(2 * 60 * 60);
    sweep_stale_credential_directories(root.path(), SystemTime::now());
    assert!(stale.exists() && recent.exists(), "fresh directories stay");
    sweep_stale_credential_directories(root.path(), later);
    assert!(!stale.exists(), "stale credential directory removed");
    assert!(
        !recent.exists(),
        "an old enough prefixed directory is stale too"
    );
    assert!(unrelated.exists(), "unrelated directories stay");
    assert!(
        link.symlink_metadata().is_ok() && target.exists(),
        "links are not followed"
    );
}

#[test]
fn credential_files_are_private_and_never_replace_existing_paths() {
    let root = tempfile::tempdir().expect("temporary root");
    let directory = root.path().join("home");
    fs::create_dir(&directory).expect("create directory");
    set_private_directory(&directory).expect("private directory");
    assert_eq!(
        fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let auth = directory.join("auth.json");
    write_private(&auth, b"{}").expect("private file");
    assert_eq!(
        fs::metadata(&auth).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        write_private(&auth, b"{}").is_err(),
        "existing file is not replaced"
    );
    let link = directory.join("link.json");
    std::os::unix::fs::symlink(root.path().join("elsewhere"), &link).expect("symlink");
    assert!(
        write_private(&link, b"{}").is_err(),
        "links are not followed"
    );
    assert!(!root.path().join("elsewhere").exists());
}

#[test]
fn quota_process_runs_npm_codex_through_a_trusted_sibling_node() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("temporary nvm install");
    let bin = root.path().join("versions/node/fixture/bin");
    let package = root
        .path()
        .join("versions/node/fixture/lib/node_modules/@openai/codex/bin");
    fs::create_dir_all(&bin).expect("create nvm bin");
    fs::create_dir_all(&package).expect("create npm package");
    let node = bin.join("node");
    fs::write(
        &node,
        "#!/bin/sh\nscript=$1; shift; exec /bin/sh \"$script\" \"$@\"\n",
    )
    .expect("write fake node runtime");
    fs::set_permissions(&node, fs::Permissions::from_mode(0o700))
        .expect("make fake node executable");

    let cli = package.join("codex.js");
    fs::write(
        &cli,
        r#"#!/usr/bin/env node
[ "$1" = "app-server" ] && [ "$2" = "--stdio" ] || exit 2
IFS= read -r request || exit 3
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r request || exit 4
IFS= read -r request || exit 5
printf '%s\n' '{"id":2,"result":{"account":{"email":"user@example.com","planType":"pro"}}}'
IFS= read -r request || exit 6
printf '%s\n' '{"id":3,"result":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":7}}}}'
"#,
    )
    .expect("write fake npm Codex entry");
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o700))
        .expect("make fake npm Codex executable");
    let launcher = bin.join("codex");
    symlink(&cli, &launcher).expect("create npm launcher symlink");

    let trusted = TrustedBinary::resolve(launcher.to_str().expect("UTF-8 launcher"))
        .expect("trusted npm Codex launcher");
    let auth = json!({"tokens":{"access_token":"fixture-token","account_id":"a"}});
    let result = run_isolated_quota_process(
        &auth,
        &trusted,
        Duration::from_secs(5),
        false,
        None,
        None,
        None,
    )
    .expect("quota request through sibling node");
    assert_eq!(result.quota["rate_limits"]["primary"]["usedPercent"], 7);
}
