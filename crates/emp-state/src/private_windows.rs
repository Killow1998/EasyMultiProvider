//! Windows private-state ACL and reparse-point helpers.

use super::FilesystemError;
use std::ffi::c_void;
use std::fs::{self, File, Metadata};
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, GetSecurityInfo, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT,
    SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, CONTAINER_INHERIT_ACE,
    DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl,
    GetTokenInformation, NO_INHERITANCE, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ALL_ACCESS, FILE_ATTRIBUTE_REPARSE_POINT, FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW,
    VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: this wrapper uniquely owns a valid Win32 handle.
            unsafe { CloseHandle(self.0) };
        }
    }
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: security APIs return LocalAlloc-owned buffers.
            unsafe { LocalFree(self.0) };
        }
    }
}

struct CurrentUser {
    _token: OwnedHandle,
    token_information: Vec<u8>,
}

impl CurrentUser {
    fn load() -> Result<Self, FilesystemError> {
        let mut token = null_mut();
        // SAFETY: output points to initialized storage and the process pseudo
        // handle remains valid for the duration of the call.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(FilesystemError::KeyFileNotOwned);
        }
        let token = OwnedHandle(token);

        let mut required = 0_u32;
        // SAFETY: a null first buffer is the documented size query.
        let first =
            unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut required) };
        if first != 0
            || required == 0
            || std::io::Error::last_os_error().raw_os_error()
                != Some(ERROR_INSUFFICIENT_BUFFER as i32)
        {
            return Err(FilesystemError::KeyFileNotOwned);
        }
        let mut token_information = vec![0_u8; required as usize];
        // SAFETY: the byte buffer has exactly the size requested by Windows.
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                token_information.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(FilesystemError::KeyFileNotOwned);
        }
        let user = Self {
            _token: token,
            token_information,
        };
        if user.sid().is_null() {
            return Err(FilesystemError::KeyFileNotOwned);
        }
        Ok(user)
    }

    fn sid(&self) -> PSID {
        // SAFETY: GetTokenInformation initialized this buffer as TOKEN_USER.
        unsafe {
            self.token_information
                .as_ptr()
                .cast::<TOKEN_USER>()
                .read_unaligned()
                .User
                .Sid
        }
    }
}

pub(super) fn metadata_is_reparse(metadata: &Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

pub(super) fn set_private_directory(path: &Path) -> Result<(), FilesystemError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| FilesystemError::KeyDirectoryUnavailable)?;
    if !metadata.is_dir() || metadata_is_reparse(&metadata) {
        return Err(FilesystemError::KeyDirectoryNotRegular);
    }
    let user = CurrentUser::load()?;
    let acl = private_acl(user.sid(), true)?;
    let mut wide = wide_path(path);
    // SAFETY: all pointers remain valid for the call and the ACL is owned by
    // `acl` until after Windows copies it into the object security descriptor.
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide.as_mut_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            user.sid(),
            null_mut(),
            acl.0.cast(),
            null(),
        )
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(FilesystemError::KeyDirectoryUnavailable)
    }
}

pub(super) fn set_private_file(file: &File) -> Result<(), FilesystemError> {
    let user = CurrentUser::load().map_err(|_| FilesystemError::ManagedFileUnavailable)?;
    let acl =
        private_acl(user.sid(), false).map_err(|_| FilesystemError::ManagedFileUnavailable)?;
    // Standard Rust write handles do not request WRITE_DAC. Resolve the name
    // of the already-open file, then let SetNamedSecurityInfoW acquire the
    // access it needs without weakening the file's ordinary data handle.
    let mut wide = path_for_file_handle(file)?;
    // SAFETY: the path and ACL buffers remain live for this synchronous call.
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide.as_mut_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            user.sid(),
            null_mut(),
            acl.0.cast(),
            null(),
        )
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(FilesystemError::ManagedFileUnavailable)
    }
}

fn path_for_file_handle(file: &File) -> Result<Vec<u16>, FilesystemError> {
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: a null/zero buffer is the documented size query.
    let required = unsafe {
        GetFinalPathNameByHandleW(
            handle,
            null_mut(),
            0,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    };
    if required == 0 {
        return Err(FilesystemError::ManagedFileUnavailable);
    }
    let mut path = vec![0_u16; required as usize + 1];
    // SAFETY: `path` is writable for the advertised number of UTF-16 units.
    let written = unsafe {
        GetFinalPathNameByHandleW(
            handle,
            path.as_mut_ptr(),
            path.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    };
    if written == 0 || written as usize >= path.len() {
        return Err(FilesystemError::ManagedFileUnavailable);
    }
    path.truncate(written as usize + 1);
    path[written as usize] = 0;
    Ok(path)
}

pub(super) fn validate_private_key_file(file: &File) -> Result<(), FilesystemError> {
    let user = CurrentUser::load()?;
    let mut owner: PSID = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: output pointers refer to local variables; descriptor owns the
    // returned owner and DACL pointers until LocalFree below.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS || descriptor.is_null() {
        return Err(FilesystemError::KeyFileNotPrivate);
    }
    let _descriptor = LocalAllocation(descriptor);
    // SAFETY: both SIDs are valid while `user` and `_descriptor` are alive.
    if owner.is_null() || unsafe { EqualSid(owner, user.sid()) } == 0 {
        return Err(FilesystemError::KeyFileNotOwned);
    }
    if dacl.is_null() || !descriptor_dacl_is_protected(descriptor) {
        return Err(FilesystemError::KeyFileNotPrivate);
    }

    // EMP's DACL contains exactly one full-control allow ACE for this user.
    // Requiring that shape rejects inherited or group access.
    let mut information: ACL_SIZE_INFORMATION = unsafe { zeroed() };
    // SAFETY: dacl belongs to the live security descriptor and the output
    // buffer has the documented structure and size.
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut information as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
        || information.AceCount != 1
    {
        return Err(FilesystemError::KeyFileNotPrivate);
    }
    let mut raw_ace = null_mut();
    // SAFETY: the ACL reports one ACE, so index zero is valid.
    if unsafe { GetAce(dacl, 0, &mut raw_ace) } == 0 || raw_ace.is_null() {
        return Err(FilesystemError::KeyFileNotPrivate);
    }
    // SAFETY: ACCESS_ALLOWED_ACE_TYPE guarantees this layout after checking
    // the common ACE header.
    let ace = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
    if u32::from(ace.Header.AceType) != ACCESS_ALLOWED_ACE_TYPE
        || ace.Mask & FILE_ALL_ACCESS != FILE_ALL_ACCESS
    {
        return Err(FilesystemError::KeyFileNotPrivate);
    }
    let ace_sid = (&ace.SidStart as *const u32).cast_mut().cast::<c_void>();
    // SAFETY: SidStart is the variable-length SID at the end of this ACE.
    if unsafe { EqualSid(ace_sid, user.sid()) } == 0 {
        return Err(FilesystemError::KeyFileNotPrivate);
    }
    Ok(())
}

fn private_acl(sid: PSID, directory: bool) -> Result<LocalAllocation, FilesystemError> {
    let trustee = TRUSTEE_W {
        pMultipleTrustee: null_mut(),
        MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_USER,
        ptstrName: sid.cast(),
    };
    let access = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: if directory {
            CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE
        } else {
            NO_INHERITANCE
        },
        Trustee: trustee,
    };
    let mut acl = null_mut();
    // SAFETY: access and SID remain valid throughout the synchronous call.
    let status = unsafe { SetEntriesInAclW(1, &access, null(), &mut acl) };
    if status != ERROR_SUCCESS || acl.is_null() {
        return Err(FilesystemError::KeyFileNotPrivate);
    }
    Ok(LocalAllocation(acl.cast()))
}

fn descriptor_dacl_is_protected(descriptor: PSECURITY_DESCRIPTOR) -> bool {
    let mut control = 0_u16;
    let mut revision = 0_u32;
    // SAFETY: descriptor is returned by GetSecurityInfo and both outputs are
    // initialized local variables.
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn detects_directory_reparse_points() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let target = directory.path().join("target");
        let link = directory.path().join("link");
        fs::create_dir(&target).expect("target directory");
        std::os::windows::fs::symlink_dir(&target, &link).expect("directory symlink");
        let metadata = fs::symlink_metadata(link).expect("link metadata");
        assert!(metadata_is_reparse(&metadata));
    }
}
