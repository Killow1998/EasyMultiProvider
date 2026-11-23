//! Lints Windows and macOS code from a Linux host before platform CI runs.
use crate::{Result, project_root};
use std::path::Path;
use std::process::{Command, Stdio};

const ZIG_VERSION: &str = "0.15.2";

pub fn run(args: &[String]) -> Result {
    let targets = match args {
        [] => vec![Target::Windows, Target::MacOs],
        [target] if target == "windows" => vec![Target::Windows],
        [target] if target == "macos" => vec![Target::MacOs],
        _ => return Err("usage: cargo xtask cross-check [windows|macos]".into()),
    };
    let root = project_root();
    for target in &targets {
        target.preflight()?;
    }
    for target in targets {
        let mut clippy = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        clippy.current_dir(&root).args([
            "clippy",
            "--locked",
            "--workspace",
            "--all-targets",
            "--target",
            target.triple(),
            "--",
            "-D",
            "warnings",
        ]);
        target.configure(&root, &mut clippy);
        eprintln!("cross-check: {}", target.triple());
        let status = clippy
            .status()
            .map_err(|error| format!("{clippy:?}: {error}"))?;
        if !status.success() {
            return Err(format!(
                "cross-check {} failed with {status}",
                target.triple()
            ));
        }
    }
    println!("cross-check: workspace clippy passed for the requested targets");
    Ok(())
}

#[derive(Clone, Copy)]
enum Target {
    Windows,
    MacOs,
}

impl Target {
    fn triple(self) -> &'static str {
        match self {
            Self::Windows => "x86_64-pc-windows-gnu",
            Self::MacOs => "x86_64-apple-darwin",
        }
    }

    fn preflight(self) -> Result {
        let (program, hint) = match self {
            Self::Windows => (
                "x86_64-w64-mingw32-gcc".to_owned(),
                "安装 MinGW-w64 C 编译器，例如 `sudo apt install gcc-mingw-w64-x86-64 cmake`"
                    .to_owned(),
            ),
            Self::MacOs => (
                zig(),
                format!(
                    "从 ziglang.org 下载 zig {ZIG_VERSION}，解压到私有目录并设置 EMP_ZIG 指向其中的 zig"
                ),
            ),
        };
        let found = Command::new(&program)
            .arg(if matches!(self, Self::MacOs) {
                "version"
            } else {
                "--version"
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success());
        match (self, found) {
            (_, None) => Err(format!(
                "cross-check {}: 找不到 {program}；{hint}",
                self.triple()
            )),
            (Self::MacOs, Some(output))
                if String::from_utf8_lossy(&output.stdout).trim() != ZIG_VERSION =>
            {
                Err(format!(
                    "cross-check {}: {program} 版本是 {}，需要 {ZIG_VERSION}；{hint}",
                    self.triple(),
                    String::from_utf8_lossy(&output.stdout).trim()
                ))
            }
            _ => Ok(()),
        }
    }

    fn configure(self, root: &Path, command: &mut Command) {
        if let Self::MacOs = self {
            let cross = root.join("xtask/cross");
            command
                .env("EMP_ZIG", zig())
                .env("CC_x86_64_apple_darwin", cross.join("zig-cc"))
                .env("CXX_x86_64_apple_darwin", cross.join("zig-c++"))
                .env("AR_x86_64_apple_darwin", cross.join("zig-ar"));
        }
    }
}

fn zig() -> String {
    std::env::var("EMP_ZIG").unwrap_or_else(|_| "zig".into())
}
