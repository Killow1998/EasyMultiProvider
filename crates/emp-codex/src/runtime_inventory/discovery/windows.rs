//! Windows package registration and npm launcher resolution.
use super::paths::executable;
use super::platform::{RuntimeOs, RuntimePlatform};
use std::path::{Path, PathBuf};

pub(super) fn cli_runtime_candidate_for_launcher(
    launcher: &Path,
    platform: RuntimePlatform,
) -> Option<PathBuf> {
    if platform.os != RuntimeOs::Windows {
        return Some(launcher.to_owned());
    }
    let extension = launcher.extension()?.to_string_lossy();
    if extension.eq_ignore_ascii_case("cmd") {
        return windows_npm_native_candidate(launcher, platform);
    }
    if extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("ps1") {
        return None;
    }
    Some(launcher.to_owned())
}

pub(super) fn cli_runtime_for_launcher(
    launcher: &Path,
    platform: RuntimePlatform,
) -> Option<PathBuf> {
    let candidate = cli_runtime_candidate_for_launcher(launcher, platform)?;
    let is_windows_command = platform.os == RuntimeOs::Windows
        && launcher
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("cmd"));
    if is_windows_command && super::super::trust::trusted_binary(&candidate).is_none() {
        return None;
    }
    Some(candidate)
}

pub(super) fn windows_npm_native_candidate(
    launcher: &Path,
    platform: RuntimePlatform,
) -> Option<PathBuf> {
    if platform.os != RuntimeOs::Windows {
        return None;
    }
    let prefix = launcher.parent()?;
    let package = platform.cli_optional_package();
    // Global npm places codex.cmd beside node_modules. Local npm places it
    // under node_modules/.bin, with @openai/codex beside .bin.
    let package_root = if prefix
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case(".bin"))
        && prefix
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name.eq_ignore_ascii_case("node_modules"))
    {
        prefix.parent()?.join("@openai/codex")
    } else {
        prefix.join("node_modules/@openai/codex")
    };
    // Match the official launcher: require.resolve checks package-local
    // node_modules before hoisted optional dependencies. If resolution fails,
    // the launcher looks for the vendored binary inside @openai/codex.
    let roots = [
        package_root.join("node_modules/@openai").join(package),
        package_root.parent()?.join(package),
        package_root.join("vendor"),
    ];
    roots.into_iter().find_map(|root| {
        let vendor_root = if root.ends_with("vendor") {
            root
        } else {
            root.join("vendor")
        };
        let binary = vendor_root
            .join(platform.cli_target_triple())
            .join("bin")
            .join(platform.executable_name());
        executable(&binary).then_some(binary)
    })
}
#[cfg(windows)]
pub(super) fn registered_windows_codex_package_root(platform: RuntimePlatform) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
    use windows_sys::Win32::Storage::Packaging::Appx::{
        FindPackagesByPackageFamily, GetPackagePathByFullName, PACKAGE_FILTER_HEAD,
    };

    const PACKAGE_FAMILY: &str = "OpenAI.Codex_2p2nqsd0c76g0";
    const MAX_PACKAGES: u32 = 16;
    const MAX_BUFFER_UNITS: u32 = 64 * 1024;
    const MAX_PATH_UNITS: u32 = 32 * 1024;

    let expected_arch = platform.windows_package_arch()?;
    let family = PACKAGE_FAMILY
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut count = 0;
    let mut buffer_length = 0;
    let status = unsafe {
        FindPackagesByPackageFamily(
            family.as_ptr(),
            PACKAGE_FILTER_HEAD,
            &mut count,
            std::ptr::null_mut(),
            &mut buffer_length,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if status == ERROR_SUCCESS && count == 0 {
        return None;
    }
    if status != ERROR_INSUFFICIENT_BUFFER
        || count == 0
        || count > MAX_PACKAGES
        || buffer_length == 0
        || buffer_length > MAX_BUFFER_UNITS
    {
        return None;
    }

    let expected_count = count;
    let expected_buffer_length = buffer_length;
    let mut package_names = vec![std::ptr::null_mut(); count as usize];
    let mut buffer = vec![0_u16; buffer_length as usize];
    let status = unsafe {
        FindPackagesByPackageFamily(
            family.as_ptr(),
            PACKAGE_FILTER_HEAD,
            &mut count,
            package_names.as_mut_ptr(),
            &mut buffer_length,
            buffer.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    };
    if status != ERROR_SUCCESS
        || count as usize > package_names.len()
        || !windows_package_list_response_is_stable(
            expected_count,
            expected_buffer_length,
            count,
            buffer_length,
        )
    {
        return None;
    }
    if buffer_length as usize > buffer.len() {
        return None;
    }
    let full_name = wide_string_in_buffer(*package_names.first()?, &buffer)?;
    if !windows_package_name_matches_arch(&full_name, expected_arch) {
        return None;
    }

    let full_name = full_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut path_length = 0;
    let status = unsafe {
        GetPackagePathByFullName(full_name.as_ptr(), &mut path_length, std::ptr::null_mut())
    };
    if status != ERROR_INSUFFICIENT_BUFFER || path_length == 0 || path_length > MAX_PATH_UNITS {
        return None;
    }
    let mut path = vec![0_u16; path_length as usize];
    let status = unsafe {
        GetPackagePathByFullName(full_name.as_ptr(), &mut path_length, path.as_mut_ptr())
    };
    if status != ERROR_SUCCESS || path_length as usize > path.len() {
        return None;
    }
    let end = path.iter().position(|unit| *unit == 0)?;
    Some(PathBuf::from(std::ffi::OsString::from_wide(&path[..end])))
}

#[cfg(any(windows, test))]
pub(super) fn windows_package_name_matches_arch(full_name: &str, expected_arch: &str) -> bool {
    let mut fields = full_name.split('_');
    fields.next() == Some("OpenAI.Codex")
        && fields.next().is_some_and(|version| !version.is_empty())
        && fields.next() == Some(expected_arch)
        && fields.next().is_some()
        && fields.next() == Some("2p2nqsd0c76g0")
        && fields.next().is_none()
}

#[cfg(any(windows, test))]
pub(super) fn windows_package_list_response_is_stable(
    expected_count: u32,
    expected_buffer_length: u32,
    count: u32,
    buffer_length: u32,
) -> bool {
    count == expected_count && buffer_length == expected_buffer_length
}

#[cfg(windows)]
pub(super) fn wide_string_in_buffer(pointer: *mut u16, buffer: &[u16]) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    let start = buffer.as_ptr() as usize;
    let address = pointer as usize;
    let byte_length = buffer.len().checked_mul(std::mem::size_of::<u16>())?;
    let end = start.checked_add(byte_length)?;
    if address < start || address >= end || (address - start) % std::mem::size_of::<u16>() != 0 {
        return None;
    }
    let offset = (address - start) / std::mem::size_of::<u16>();
    let tail = buffer.get(offset..)?;
    let length = tail.iter().position(|unit| *unit == 0)?;
    String::from_utf16(&tail[..length]).ok()
}
