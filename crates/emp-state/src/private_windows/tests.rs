use super::*;
use std::os::windows::fs::OpenOptionsExt;
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::UNPROTECTED_DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, WRITE_OWNER};

fn descriptor(file: &File) -> LocalAllocation {
    let mut descriptor = null_mut();
    let result = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(result, ERROR_SUCCESS);
    LocalAllocation(descriptor)
}

fn set_test_acl(path: &Path, acl: &LocalAllocation, protected: bool) {
    let mut path = wide_path(path);
    let protection = if protected {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    assert_eq!(
        unsafe {
            SetNamedSecurityInfoW(
                path.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | protection,
                null_mut(),
                null_mut(),
                acl.0.cast(),
                null(),
            )
        },
        ERROR_SUCCESS
    );
}

#[test]
fn applies_and_validates_a_current_user_only_file_acl() {
    let directory = tempfile::tempdir().expect("temporary directory");
    set_private_directory(directory.path()).expect("private directory ACL");
    let path = directory.path().join("private.txt");
    let file = File::create(path).expect("create file");
    set_private_file(&file).expect("private file ACL");
    validate_private_key_file(&file).expect("validate private ACL");
}

#[test]
fn legacy_inherited_key_is_protected_without_changing_key_or_credentials() {
    let directory = tempfile::tempdir().unwrap();
    set_private_directory(directory.path()).unwrap();
    let path = directory.path().join("master.key");
    let key = crate::FernetKey::generate();
    let bytes = format!("{}\n", key.encoded());
    fs::write(&path, &bytes).unwrap();
    let file = File::open(&path).unwrap();
    assert!(!descriptor_dacl_is_protected(descriptor(&file).0));
    let credentials = directory.path().join("credentials");
    let ciphertext = crate::encode_vault(&key, b"synthetic existing credential");
    fs::write(&credentials, &ciphertext).unwrap();

    let vault = crate::VaultStore::from_sources(None, &path).unwrap();

    assert!(descriptor_dacl_is_protected(descriptor(&file).0));
    assert_eq!(fs::read(&path).unwrap(), bytes.as_bytes());
    assert_eq!(fs::read(&credentials).unwrap(), ciphertext);
    assert_eq!(
        vault.read_encrypted_bytes(&credentials).unwrap().as_slice(),
        b"synthetic existing credential"
    );
    // Once migrated, loading the same installation continues to work.
    crate::VaultStore::from_sources(None, &path).unwrap();
}

#[test]
fn private_state_writes_do_not_require_write_owner_from_the_existing_owner() {
    let directory = tempfile::tempdir().unwrap();
    set_private_directory(directory.path()).unwrap();
    let user = CurrentUser::load().unwrap();
    let acl = private_acl(user.sid(), true).unwrap();
    let mut ace = null_mut();
    assert_ne!(unsafe { GetAce(acl.0.cast(), 0, &mut ace) }, 0);
    // This is the sole ACCESS_ALLOWED_ACE created by private_acl above.
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
fn legacy_key_granting_another_principal_access_is_not_silently_repaired() {
    let directory = tempfile::tempdir().unwrap();
    set_private_directory(directory.path()).unwrap();
    let path = directory.path().join("master.key");
    let bytes = crate::FernetKey::generate().encoded().to_owned();
    fs::write(&path, &bytes).unwrap();
    let mut everyone = null_mut();
    let sid_string: Vec<u16> = "S-1-1-0\0".encode_utf16().collect();
    assert_ne!(
        unsafe { ConvertStringSidToSidW(sid_string.as_ptr(), &mut everyone) },
        0
    );
    let everyone = LocalAllocation(everyone);
    let acl = private_acl(everyone.0, false).unwrap();
    // The inherited current-user ACE remains, alongside the explicit grant.
    set_test_acl(&path, &acl, false);
    let file = File::open(&path).unwrap();

    assert!(matches!(
        crate::VaultStore::from_sources(None, &path),
        Err(FilesystemError::KeyFileNotPrivate)
    ));
    assert!(!descriptor_dacl_is_protected(descriptor(&file).0));
    assert_eq!(fs::read(&path).unwrap(), bytes.as_bytes());
}

#[test]
fn detects_directory_reparse_points() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let target = directory.path().join("target");
    let link = directory.path().join("link");
    fs::create_dir(&target).expect("target directory");
    std::os::windows::fs::symlink_dir(&target, &link).expect("directory symlink");
    let metadata = fs::symlink_metadata(link).expect("link metadata");
    assert!(metadata_is_reparse(&metadata));
}
