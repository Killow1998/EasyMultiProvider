use crate::FilesystemError;
use emp_files::filesystem::set_private_directory;
use emp_files::{CurrentUser, LocalAllocation, private_acl};
use std::fs::{self, File};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSidToSidW, GetSecurityInfo, SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
    UNPROTECTED_DACL_SECURITY_INFORMATION,
};

fn descriptor(file: &File) -> LocalAllocation {
    let mut descriptor = null_mut();
    // SAFETY: file owns a live handle and descriptor is writable output storage, freed with LocalFree.
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
    // SAFETY: GetSecurityInfo succeeded and transferred ownership of the descriptor.
    unsafe { LocalAllocation::from_raw(descriptor) }
}

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

fn set_test_owner(path: &Path) {
    // An elevated runner may create files owned by Administrators. Model an
    // ordinary user's legacy key explicitly, preserving its inherited DACL.
    let user = CurrentUser::load().unwrap();
    let mut path = wide_path(path);
    assert_eq!(
        // SAFETY: path is NUL-terminated and user owns the SID for the duration of the call.
        unsafe {
            SetNamedSecurityInfoW(
                path.as_mut_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                user.sid().as_ptr(),
                null_mut(),
                null(),
                null(),
            )
        },
        ERROR_SUCCESS
    );
}

fn descriptor_dacl_is_protected(descriptor: PSECURITY_DESCRIPTOR) -> bool {
    let mut control = 0_u16;
    let mut revision = 0_u32;
    // SAFETY: GetSecurityInfo returned descriptor; both outputs are initialized local variables.
    let succeeded =
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) };
    succeeded != 0 && control & SE_DACL_PROTECTED != 0
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[test]
fn legacy_inherited_key_is_protected_without_changing_key_or_credentials() {
    let directory = tempfile::tempdir().unwrap();
    set_private_directory(directory.path()).unwrap();
    let path = directory.path().join("master.key");
    let key = crate::FernetKey::generate();
    let bytes = format!("{}\n", key.encoded());
    fs::write(&path, &bytes).unwrap();
    set_test_owner(&path);
    let file = File::open(&path).unwrap();
    assert!(!descriptor_dacl_is_protected(descriptor(&file).as_ptr()));
    let credentials = directory.path().join("credentials");
    let ciphertext = crate::encode_vault(&key, b"synthetic existing credential");
    fs::write(&credentials, &ciphertext).unwrap();

    let vault = crate::VaultStore::from_sources(None, &path).unwrap();

    assert!(descriptor_dacl_is_protected(descriptor(&file).as_ptr()));
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
fn legacy_key_granting_another_principal_access_is_not_silently_repaired() {
    let directory = tempfile::tempdir().unwrap();
    set_private_directory(directory.path()).unwrap();
    let path = directory.path().join("master.key");
    let bytes = crate::FernetKey::generate().encoded().to_owned();
    fs::write(&path, &bytes).unwrap();
    set_test_owner(&path);
    let mut everyone = null_mut();
    let sid_string: Vec<u16> = "S-1-1-0\0".encode_utf16().collect();
    assert_ne!(
        // SAFETY: sid_string is NUL-terminated and everyone receives an allocation later freed by LocalFree.
        unsafe { ConvertStringSidToSidW(sid_string.as_ptr(), &mut everyone) },
        0
    );
    // SAFETY: ConvertStringSidToSidW succeeded and transferred ownership of a valid SID.
    let everyone = unsafe { LocalAllocation::from_raw(everyone) };
    // SAFETY: this allocation contains the untouched SID returned by ConvertStringSidToSidW.
    let acl = private_acl(unsafe { everyone.sid() }, false).unwrap();
    // The inherited current-user ACE remains, alongside the explicit grant.
    set_test_acl(&path, &acl, false);
    let file = File::open(&path).unwrap();

    assert!(matches!(
        crate::VaultStore::from_sources(None, &path),
        Err(FilesystemError::KeyFileNotPrivate)
    ));
    assert!(!descriptor_dacl_is_protected(descriptor(&file).as_ptr()));
    assert_eq!(fs::read(&path).unwrap(), bytes.as_bytes());
}
