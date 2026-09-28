//! Windows icon/version resources: write the .rc script, compile it with the
//! SDK's rc.exe, and verify the resources inside the built PE executable.
use crate::Result;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const RT_ICON: u32 = 3;
const RT_GROUP_ICON: u32 = 14;
const RT_VERSION: u32 = 16;

fn stable_version(version: &str) -> Result<[u16; 3]> {
    let parts: Vec<&str> = version.split('.').collect();
    let valid = parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part.len() == 1 || !part.starts_with('0'))
        });
    if !valid {
        return Err("Windows file resources require a stable three-part version".to_owned());
    }
    let mut numbers = [0_u16; 3];
    for (number, part) in numbers.iter_mut().zip(parts) {
        *number = part
            .parse()
            .map_err(|_| "Windows file version components must fit 16 bits".to_owned())?;
    }
    Ok(numbers)
}

pub fn resource_script(version: &str) -> Result<String> {
    let [major, minor, patch] = stable_version(version)?;
    let text = format!("{major}.{minor}.{patch}");
    let numeric = format!("{major},{minor},{patch},0");
    Ok(format!(
        r#"#define VS_VERSION_INFO 1
1 ICON "easy-multi-provider.ico"

VS_VERSION_INFO VERSIONINFO
 FILEVERSION {numeric}
 PRODUCTVERSION {numeric}
 FILEFLAGSMASK 0x3fL
 FILEFLAGS 0x0L
 FILEOS 0x00040004L
 FILETYPE 0x00000001L
 FILESUBTYPE 0x00000000L
BEGIN
 BLOCK "StringFileInfo"
 BEGIN
  BLOCK "040904b0"
  BEGIN
   VALUE "CompanyName", "Killow1998"
   VALUE "FileDescription", "EMP local multi-provider control plane"
   VALUE "FileVersion", "{text}"
   VALUE "InternalName", "EMP"
   VALUE "OriginalFilename", "EMP.exe"
   VALUE "ProductName", "EasyMultiProvider"
   VALUE "ProductVersion", "{text}"
  END
 END
 BLOCK "VarFileInfo"
 BEGIN
  VALUE "Translation", 0x0409, 1200
 END
END
"#
    ))
}

pub fn write_resource_script(destination: &Path, icon: &Path, version: &str) -> Result<PathBuf> {
    if !icon.is_file() {
        return Err("Windows application icon is missing".to_owned());
    }
    let source = resource_script(version)?;
    fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    crate::copy(icon, &destination.join("easy-multi-provider.ico"))?;
    let script = destination.join("EMP.rc");
    fs::write(&script, source).map_err(|error| error.to_string())?;
    Ok(script)
}

fn versioned_compilers(bin: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(bin) else {
        return Vec::new();
    };
    let mut versions: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    let key = |path: &PathBuf| -> Vec<u64> {
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .split(|character: char| !character.is_ascii_digit())
            .filter_map(|number| number.parse().ok())
            .collect()
    };
    versions.sort_by_key(|path| std::cmp::Reverse(key(path)));
    versions
        .iter()
        .flat_map(|version| ["x64", "x86"].map(|arch| version.join(arch).join("rc.exe")))
        .collect()
}

fn find_resource_compiler() -> Result<PathBuf> {
    if let Some(configured) = std::env::var_os("EMP_RC") {
        let compiler = PathBuf::from(configured);
        return if compiler.is_file() {
            Ok(compiler)
        } else {
            Err("EMP_RC does not point to an existing rc.exe".to_owned())
        };
    }
    let mut candidates = Vec::new();
    if let Some(sdk_bin) = std::env::var_os("WindowsSdkBinPath") {
        candidates.extend(["x64", "x86"].map(|arch| Path::new(&sdk_bin).join(arch).join("rc.exe")));
    }
    if let Some(sdk) = std::env::var_os("WindowsSdkDir") {
        let bin = Path::new(&sdk).join("bin");
        let version = std::env::var("WindowsSDKVersion").unwrap_or_default();
        let version = version.trim_end_matches(['\\', '/']);
        if !version.is_empty() {
            candidates
                .extend(["x64", "x86"].map(|arch| bin.join(version).join(arch).join("rc.exe")));
        }
        candidates.extend(versioned_compilers(&bin));
    }
    if let Some(program_files) = std::env::var_os("ProgramFiles(x86)") {
        candidates.extend(versioned_compilers(
            &Path::new(&program_files).join("Windows Kits/10/bin"),
        ));
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|directory| directory.join("rc.exe")));
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| "Windows SDK rc.exe was not found".to_owned())
}

pub fn compile_resource(script: &Path) -> Result<PathBuf> {
    let directory = script.parent().ok_or("resource script has no parent")?;
    if !directory.join("easy-multi-provider.ico").is_file() {
        return Err("Windows application icon is missing beside its resource script".to_owned());
    }
    let compiled = script.with_extension("res");
    let output = Command::new(find_resource_compiler()?)
        .current_dir(directory)
        .arg("/nologo")
        .arg("/fo")
        .arg(&compiled)
        .arg(script.file_name().ok_or("resource script has no name")?)
        .output()
        .map_err(|error| format!("Windows resource compiler could not run: {error}"))?;
    if !output.status.success() {
        let details = if output.stderr.is_empty() {
            output.stdout
        } else {
            output.stderr
        };
        return Err(format!(
            "Windows resource compilation failed: {}",
            String::from_utf8_lossy(&details).trim()
        ));
    }
    if fs::metadata(&compiled).map_or(0, |metadata| metadata.len()) == 0 {
        return Err("Windows resource compiler did not create a nonempty .res file".to_owned());
    }
    Ok(compiled)
}

struct Pe<'a> {
    data: &'a [u8],
    /// (virtual address, virtual size, raw pointer, raw size) per section.
    sections: Vec<(u32, u32, u32, u32)>,
}

fn read_u16(data: &[u8], offset: usize) -> Result<u16> {
    data.get(offset..offset + 2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .ok_or_else(|| "truncated PE executable".to_owned())
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    data.get(offset..offset + 4)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .ok_or_else(|| "truncated PE executable".to_owned())
}

impl Pe<'_> {
    fn offset(&self, rva: u32) -> Result<usize> {
        self.sections
            .iter()
            .find(|(address, size, _, raw_size)| {
                rva >= *address && rva < address + size.max(raw_size)
            })
            .map(|(address, _, pointer, _)| (rva - address + pointer) as usize)
            .ok_or_else(|| "PE address is outside every section".to_owned())
    }
}

/// Entries of one resource directory as (id, is_subdirectory, offset from the resource root).
fn resource_entries(
    data: &[u8],
    root: usize,
    directory: usize,
) -> Result<Vec<(Option<u32>, bool, usize)>> {
    let base = root + directory;
    let count = read_u16(data, base + 12)? as usize + read_u16(data, base + 14)? as usize;
    (0..count)
        .map(|index| {
            let entry = base + 16 + index * 8;
            let name = read_u32(data, entry)?;
            let target = read_u32(data, entry + 4)?;
            let id = (name & 0x8000_0000 == 0).then_some(name);
            Ok((
                id,
                target & 0x8000_0000 != 0,
                (target & 0x7fff_ffff) as usize,
            ))
        })
        .collect()
}

fn utf16_value_after(data: &[u8], key: &str) -> Option<String> {
    let needle: Vec<u8> = key
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect();
    let start = data
        .windows(needle.len())
        .position(|window| window == needle.as_slice())?
        + needle.len();
    let units: Vec<u16> = data[start..]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .skip_while(|unit| *unit == 0)
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16(&units).ok()
}

pub fn validate_executable(executable: &Path, version: &str) -> Result {
    let [major, minor, patch] = stable_version(version)?;
    let data =
        fs::read(executable).map_err(|error| format!("read {}: {error}", executable.display()))?;
    let header = read_u32(&data, 0x3c)? as usize;
    if data.get(header..header + 4) != Some(b"PE\0\0") {
        return Err("Windows package is not a readable PE executable".to_owned());
    }
    if read_u16(&data, header + 4)? != 0x8664 {
        return Err("Windows package is not an x86_64 executable".to_owned());
    }
    let section_count = read_u16(&data, header + 6)? as usize;
    let optional = header + 24;
    if read_u16(&data, optional)? != 0x20b {
        return Err("Windows package is not a PE32+ executable".to_owned());
    }
    let resource_rva = read_u32(&data, optional + 112 + 2 * 8)?;
    let section_table = optional + read_u16(&data, header + 20)? as usize;
    let sections = (0..section_count)
        .map(|index| {
            let section = section_table + index * 40;
            Ok((
                read_u32(&data, section + 12)?,
                read_u32(&data, section + 8)?,
                read_u32(&data, section + 20)?,
                read_u32(&data, section + 16)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let pe = Pe {
        data: &data,
        sections,
    };
    if resource_rva == 0 {
        return Err("Windows package is missing icon or version resources".to_owned());
    }
    let root = pe.offset(resource_rva)?;
    let types = resource_entries(pe.data, root, 0)?;
    let ids: Vec<u32> = types.iter().filter_map(|(id, _, _)| *id).collect();
    if ![RT_ICON, RT_GROUP_ICON, RT_VERSION]
        .iter()
        .all(|required| ids.contains(required))
    {
        return Err("Windows package is missing icon or version resources".to_owned());
    }
    // Type -> name -> language -> data entry.
    let mut node = types
        .iter()
        .find(|(id, _, _)| *id == Some(RT_VERSION))
        .map(|(_, directory, offset)| (*directory, *offset))
        .ok_or("Windows package has no version resource")?;
    for _ in 0..2 {
        let (directory, offset) = node;
        if !directory {
            return Err("Windows version resource has an unexpected layout".to_owned());
        }
        let (_, directory, offset) = *resource_entries(pe.data, root, offset)?
            .first()
            .ok_or("Windows version resource is empty")?;
        node = (directory, offset);
    }
    if node.0 {
        return Err("Windows version resource has an unexpected layout".to_owned());
    }
    let entry = root + node.1;
    let start = pe.offset(read_u32(pe.data, entry)?)?;
    let size = read_u32(pe.data, entry + 4)? as usize;
    let resource = pe
        .data
        .get(start..start + size)
        .ok_or("Windows version resource is truncated")?;

    let signature = 0xfeef_04bd_u32.to_le_bytes();
    let fixed = resource
        .windows(4)
        .position(|window| window == signature)
        .ok_or("Windows package has no fixed version resource")?;
    let expected_ms = (u32::from(major) << 16) | u32::from(minor);
    let expected_ls = u32::from(patch) << 16;
    let actual = [8, 12, 16, 20].map(|offset| read_u32(resource, fixed + offset));
    let actual = actual.into_iter().collect::<Result<Vec<_>>>()?;
    if actual != [expected_ms, expected_ls, expected_ms, expected_ls] {
        return Err("Windows package fixed version does not match source".to_owned());
    }
    let text = format!("{major}.{minor}.{patch}");
    for key in ["FileVersion", "ProductVersion"] {
        if utf16_value_after(resource, key).as_deref() != Some(text.as_str()) {
            return Err(format!(
                "Windows package {key} string does not match source"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_script_embeds_icon_and_exact_source_version() {
        let script = resource_script("0.12.2").unwrap();
        assert!(script.contains("1 ICON \"easy-multi-provider.ico\""));
        assert!(script.contains(" FILEVERSION 0,12,2,0\n PRODUCTVERSION 0,12,2,0"));
        assert!(script.contains("VALUE \"FileVersion\", \"0.12.2\""));
        assert!(script.contains("VALUE \"ProductVersion\", \"0.12.2\""));
    }

    #[test]
    fn resource_script_rejects_nonstable_or_oversized_versions() {
        for version in ["0.12", "0.12.2-rc1", "01.2.3", "1.2.65536", "1..2"] {
            assert!(resource_script(version).is_err(), "{version}");
        }
    }

    #[test]
    fn missing_icon_fails_before_writing_a_script() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("resources");
        assert!(
            write_resource_script(
                &destination,
                &directory.path().join("missing.ico"),
                "0.12.2"
            )
            .is_err()
        );
        assert!(!destination.exists());
    }

    #[test]
    fn utf16_values_skip_alignment_padding() {
        let mut data: Vec<u8> = "FileVersion\0"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        data.extend([0, 0]);
        data.extend("0.12.2\0".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(
            utf16_value_after(&data, "FileVersion").as_deref(),
            Some("0.12.2")
        );
    }
}
