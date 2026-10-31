use super::Image;
use std::path::Path;

pub(super) fn apply(root: &Path, image: &Image) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(root)?;
    let png = root.join(format!("{}.png", image.version));
    emp_state::atomic_write_private_state(&png, &image.png)?;
    #[cfg(windows)]
    {
        use std::{
            os::windows::process::CommandExt,
            process::{Command, Stdio},
        };
        let ico = root.join(format!("{}.ico", image.version));
        emp_state::atomic_write_private_state(&ico, &super::pixels::ico(image)?)?;
        let script = r#"$ErrorActionPreference='Stop'
$shell=New-Object -ComObject WScript.Shell
$target=[IO.Path]::GetFullPath($env:EMP_ICON_EXECUTABLE)
foreach($folder in @('Desktop','Programs')) {
  $directory=$shell.SpecialFolders.Item($folder)
  if([IO.Directory]::Exists($directory)) {
    Get-ChildItem -LiteralPath $directory -Filter '*.lnk' -File -Recurse | ForEach-Object {
      $shortcut=$shell.CreateShortcut($_.FullName)
      if($shortcut.TargetPath -and [IO.Path]::GetFullPath($shortcut.TargetPath) -ieq $target) {
        $shortcut.IconLocation=$env:EMP_ICON_FILE+',0'
        $shortcut.Save()
      }
    }
  }
}"#;
        let powershell = crate::cli::desktop::windows_system_directory()
            .join("WindowsPowerShell/v1.0/powershell.exe");
        if !Command::new(powershell)
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("EMP_ICON_EXECUTABLE", std::env::current_exe()?)
            .env("EMP_ICON_FILE", ico)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x08000000)
            .status()?
            .success()
        {
            return Err("shortcut icon update failed".into());
        }
    }
    #[cfg(target_os = "linux")]
    linux(&png)?;
    #[cfg(target_os = "macos")]
    {
        use objc2::AnyThread;
        use objc2_app_kit::{NSImage, NSWorkspace, NSWorkspaceIconCreationOptions};
        use objc2_foundation::{NSData, NSString};
        let executable = std::env::current_exe()?;
        if let Some(bundle) = executable
            .ancestors()
            .find(|path| path.extension().is_some_and(|extension| extension == "app"))
        {
            let icon = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(&image.png))
                .ok_or("application icon could not be decoded")?;
            if !NSWorkspace::sharedWorkspace().setIcon_forFile_options(
                Some(&icon),
                &NSString::from_str(&bundle.to_string_lossy()),
                NSWorkspaceIconCreationOptions::empty(),
            ) {
                return Err("application bundle icon could not be updated".into());
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux(png: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is unset")?;
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let desktop = std::fs::read_to_string(config.join("user-dirs.dirs"))
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix("XDG_DESKTOP_DIR=\"")?
                    .strip_suffix('"')
                    .map(str::to_owned)
            })
        })
        .map(|directory| PathBuf::from(directory.replace("$HOME", &home.to_string_lossy())))
        .unwrap_or_else(|| home.join("Desktop"));
    for name in [
        "easy-multi-provider.desktop",
        "easy-multi-provider-user.desktop",
    ] {
        let user = data.join("applications").join(name);
        let source = if user.is_file() {
            user.clone()
        } else {
            Path::new("/usr/share/applications").join(name)
        };
        for (source, destination) in [(&source, &user), (&desktop.join(name), &desktop.join(name))]
        {
            if !source.is_file() {
                continue;
            }
            let contents = std::fs::read_to_string(source)?;
            let text = contents
                .lines()
                .map(|line| {
                    if line.starts_with("Icon=") {
                        format!("Icon={}", png.display())
                    } else {
                        line.into()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            std::fs::create_dir_all(destination.parent().expect("launcher directory"))?;
            std::fs::write(destination, text)?;
        }
    }
    Ok(())
}
