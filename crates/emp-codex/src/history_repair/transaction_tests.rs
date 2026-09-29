use super::*;
use std::fs;
use std::path::PathBuf;

const THREAD_ID: &str = "11111111-1111-4111-8111-111111111111";

struct Fixture {
    _temporary: tempfile::TempDir,
    home: PathBuf,
    repair_root: PathBuf,
    manifest_path: PathBuf,
    target_path: PathBuf,
    backup_path: PathBuf,
    stage_path: PathBuf,
    before: Vec<u8>,
    after: Vec<u8>,
}

fn fixture() -> Fixture {
    let fixture_root = std::env::var_os("EMP_TRANSACTION_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("emp-transaction-recovery-tests"));
    fs::create_dir_all(&fixture_root).unwrap();
    let temporary = tempfile::Builder::new()
        .prefix("emp-transaction-recovery-")
        .tempdir_in(&fixture_root)
        .unwrap();
    let home = temporary.path().join("codex-home");
    fs::create_dir_all(&home).unwrap();
    let home = fs::canonicalize(home).unwrap();
    let session_directory = home.join("sessions/2026/09/29");
    let repair_root = home.join(super::super::REPAIR_DIRECTORY);
    let transaction_directory = repair_root.join("repair-interrupted");
    fs::create_dir_all(&session_directory).unwrap();
    fs::create_dir_all(&transaction_directory).unwrap();

    let target_path = session_directory.join(format!("{THREAD_ID}.jsonl"));
    let before = b"original rollout bytes\n".to_vec();
    let after = b"replacement rollout bytes\n".to_vec();
    fs::write(&target_path, &before).unwrap();

    let transaction_relative = transaction_directory.strip_prefix(&home).unwrap();
    let backup_path = transaction_directory.join(format!("{THREAD_ID}.original"));
    let stage_path = session_directory.join(format!(
        ".emp-history-repair-repair-interrupted-{THREAD_ID}.stage"
    ));
    let manifest_path = transaction_directory.join("manifest.json");
    let manifest = RepairManifest {
        format_version: 1,
        status: "preparing".to_owned(),
        entries: vec![RepairManifestEntry {
            thread_id: THREAD_ID.to_owned(),
            target: target_path.strip_prefix(&home).unwrap().to_path_buf(),
            backup: transaction_relative.join(format!("{THREAD_ID}.original")),
            stage: stage_path.strip_prefix(&home).unwrap().to_path_buf(),
            before_sha256: sha256(&before),
            after_sha256: sha256(&after),
            before_bytes: before.len() as u64,
            after_bytes: after.len() as u64,
        }],
    };
    write_manifest(&manifest_path, &manifest).unwrap();

    Fixture {
        _temporary: temporary,
        home,
        repair_root,
        manifest_path,
        target_path,
        backup_path,
        stage_path,
        before,
        after,
    }
}

fn preparing_manifest(fixture: &Fixture) -> RepairManifest {
    serde_json::from_slice(&fs::read(&fixture.manifest_path).unwrap()).unwrap()
}

fn assert_repair_succeeds_after_recovery(fixture: &Fixture) {
    let pending = pending_manifests(&fixture.home, &fixture.repair_root).unwrap();
    recover_pending(&fixture.home, &fixture.repair_root, pending).unwrap();
    let replacement_path = fixture.home.join("replacement.spool");
    fs::write(&replacement_path, &fixture.after).unwrap();
    let report = apply_rewrites(
        &fixture.home,
        &fixture.repair_root,
        vec![PlannedRewrite {
            thread_id: THREAD_ID.to_owned(),
            path: fixture.target_path.clone(),
            replacement_path,
            before_sha256: sha256(&fixture.before),
            after_sha256: sha256(&fixture.after),
            before_bytes: fixture.before.len() as u64,
            after_bytes: fixture.after.len() as u64,
        }],
        1,
    )
    .unwrap();
    assert_eq!(report.threads_repaired, 1);
    assert_eq!(fs::read(&fixture.target_path).unwrap(), fixture.after);
}

#[test]
fn interrupted_partial_backup_is_removed_and_a_later_repair_succeeds() {
    let fixture = fixture();
    fs::write(&fixture.backup_path, b"partial backup").unwrap();
    recover_preparing(
        &fixture.home,
        &fixture.manifest_path,
        preparing_manifest(&fixture),
    )
    .unwrap();

    assert!(!fixture.backup_path.exists());
    assert_eq!(fs::read(&fixture.target_path).unwrap(), fixture.before);
    assert_eq!(
        preparing_manifest(&fixture).status,
        "aborted",
        "recovery records the interrupted transaction as aborted"
    );
    assert_repair_succeeds_after_recovery(&fixture);
}

#[test]
fn interrupted_partial_stage_is_removed_and_a_later_repair_succeeds() {
    let fixture = fixture();
    fs::write(&fixture.backup_path, &fixture.before).unwrap();
    fs::write(&fixture.stage_path, b"partial stage").unwrap();
    recover_preparing(
        &fixture.home,
        &fixture.manifest_path,
        preparing_manifest(&fixture),
    )
    .unwrap();

    assert!(!fixture.stage_path.exists());
    assert_eq!(fs::read(&fixture.backup_path).unwrap(), fixture.before);
    assert_eq!(fs::read(&fixture.target_path).unwrap(), fixture.before);
    assert_eq!(preparing_manifest(&fixture).status, "aborted");
    assert_repair_succeeds_after_recovery(&fixture);
}

#[test]
fn preparing_recovery_preserves_partial_artifacts_when_target_changed() {
    let fixture = fixture();
    let partial_backup = b"partial backup";
    let partial_stage = b"partial stage";
    fs::write(&fixture.backup_path, partial_backup).unwrap();
    fs::write(&fixture.stage_path, partial_stage).unwrap();
    let changed_target = b"changed by another writer\n";
    fs::write(&fixture.target_path, changed_target).unwrap();

    let error = recover_preparing(
        &fixture.home,
        &fixture.manifest_path,
        preparing_manifest(&fixture),
    )
    .unwrap_err();

    assert_eq!(error.reason(), "repair_stale_snapshot");
    assert_eq!(fs::read(&fixture.backup_path).unwrap(), partial_backup);
    assert_eq!(fs::read(&fixture.stage_path).unwrap(), partial_stage);
    assert_eq!(fs::read(&fixture.target_path).unwrap(), changed_target);
}

#[test]
fn prepared_recovery_preserves_staged_files_when_target_is_stale() {
    let fixture = fixture();
    let staged = fixture.after.clone();
    let changed_target = b"changed after preparation\n";
    fs::write(&fixture.backup_path, &fixture.before).unwrap();
    fs::write(&fixture.stage_path, &staged).unwrap();
    fs::write(&fixture.target_path, changed_target).unwrap();
    let mut manifest = preparing_manifest(&fixture);
    manifest.status = "prepared".to_owned();
    write_manifest(&fixture.manifest_path, &manifest).unwrap();

    let pending = pending_manifests(&fixture.home, &fixture.repair_root).unwrap();
    let error = recover_pending(&fixture.home, &fixture.repair_root, pending).unwrap_err();

    assert_eq!(error.reason(), "repair_stale_snapshot");
    assert_eq!(fs::read(&fixture.target_path).unwrap(), changed_target);
    assert_eq!(fs::read(&fixture.backup_path).unwrap(), fixture.before);
    assert_eq!(fs::read(&fixture.stage_path).unwrap(), staged);
    assert_eq!(preparing_manifest(&fixture).status, "prepared");
}

#[test]
fn preparing_recovery_rejects_unsafe_missing_artifact_paths() {
    let fixture = fixture();
    let mut manifest = preparing_manifest(&fixture);
    manifest.entries[0].backup = PathBuf::from("../outside/original");

    let error = recover_preparing(&fixture.home, &fixture.manifest_path, manifest).unwrap_err();

    assert_eq!(error.reason(), "repair_manifest_unsafe");
    assert_eq!(fs::read(&fixture.target_path).unwrap(), fixture.before);
}

#[test]
fn publishing_a_new_artifact_does_not_replace_an_existing_file() {
    let fixture = fixture();
    let existing = b"keep existing artifact";
    fs::write(&fixture.backup_path, existing).unwrap();

    let error = write_new_synced(&fixture.backup_path, b"replacement", None).unwrap_err();

    assert_eq!(error.reason(), "repair_file_unavailable");
    assert_eq!(fs::read(&fixture.backup_path).unwrap(), existing);
}

#[cfg(unix)]
#[test]
fn preparing_recovery_rejects_and_preserves_symlink_artifacts() {
    use std::os::unix::fs::symlink;

    let fixture = fixture();
    symlink(&fixture.target_path, &fixture.stage_path).unwrap();

    let error = recover_preparing(
        &fixture.home,
        &fixture.manifest_path,
        preparing_manifest(&fixture),
    )
    .unwrap_err();

    assert_eq!(error.reason(), "repair_stage_unsafe");
    assert!(
        fs::symlink_metadata(&fixture.stage_path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&fixture.target_path).unwrap(), fixture.before);
}
