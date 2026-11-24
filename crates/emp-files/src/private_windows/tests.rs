use super::*;
use std::os::windows::fs::OpenOptionsExt;
use windows_sys::Win32::Security::UNPROTECTED_DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, WRITE_OWNER};

fn set_test_acl(path: &Path, acl: &LocalAllocation, protected: bool) {
    let mut path = wide_path(path);
    let protection = if protected {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    assert_eq!(
        // SAFETY: path is NUL-terminated and acl owns a valid ACL that remains live through the call.
        unsafe {
            SetNamedSecurityInfoW(
                path.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | protection,
                null_mut(),
                null_mut(),
                acl.as_ptr().cast(),
                null(),
            )
        },
        ERROR_SUCCESS
    );
}

#[test]
fn applies_and_validates_a_current_user_only_file_acl() {
    let directory = emp_test_support::fixture_dir().expect("temporary directory");
    set_private_directory(directory.path()).expect("private directory ACL");
    let path = directory.path().join("private.txt");
    let file = File::create(path).expect("create file");
    set_private_file(&file).expect("private file ACL");
    validate_private_key_file(&file).expect("validate private ACL");
}

#[test]
fn private_state_writes_do_not_require_write_owner_from_the_existing_owner() {
    let directory = emp_test_support::fixture_dir().unwrap();
    set_private_directory(directory.path()).unwrap();
    let user = CurrentUser::load().unwrap();
    let acl = private_acl(user.sid(), true).unwrap();
    let mut ace = null_mut();
    // SAFETY: acl owns an initialized ACL containing one ACE and ace is writable output storage.
    assert_ne!(unsafe { GetAce(acl.as_ptr().cast(), 0, &mut ace) }, 0);
    // SAFETY: GetAce succeeded and returned the sole ACCESS_ALLOWED_ACE in the live private_acl allocation.
    unsafe { (*ace.cast::<ACCESS_ALLOWED_ACE>()).Mask &= !WRITE_OWNER };
    set_test_acl(directory.path(), &acl, true);
    let ownership_access = fs::OpenOptions::new()
        .access_mode(WRITE_OWNER)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(directory.path());
    assert!(ownership_access.is_err(), "fixture must deny WRITE_OWNER");

    let marker = directory.path().join("worker-status.json");
    crate::filesystem::atomic_write_private_state(&marker, b"{\"phase\":\"complete\"}").unwrap();
    assert_eq!(fs::read(&marker).unwrap(), b"{\"phase\":\"complete\"}");

    // Also cover existing files reached through the handle-based file helper.
    let file = File::open(&marker).unwrap();
    set_test_acl(&marker, &acl, true);
    assert!(
        fs::OpenOptions::new()
            .access_mode(WRITE_OWNER)
            .open(&marker)
            .is_err()
    );
    set_private_file(&file).unwrap();
    validate_private_key_file(&file).unwrap();
}

#[test]
fn detects_directory_reparse_points() {
    let directory = emp_test_support::fixture_dir().expect("temporary directory");
    let target = directory.path().join("target");
    let link = directory.path().join("link");
    fs::create_dir(&target).expect("target directory");
    std::os::windows::fs::symlink_dir(&target, &link).expect("directory symlink");
    let metadata = fs::symlink_metadata(link).expect("link metadata");
    assert!(metadata_is_reparse(&metadata));
}
