//! Operating system and architecture policies used by discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RuntimeOs {
    Windows,
    Linux,
    Macos,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RuntimeArch {
    X64,
    Arm64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RuntimePlatform {
    pub(super) os: RuntimeOs,
    pub(super) arch: RuntimeArch,
}

impl RuntimePlatform {
    pub(super) fn current() -> Option<Self> {
        let os = match std::env::consts::OS {
            "windows" => RuntimeOs::Windows,
            "linux" => RuntimeOs::Linux,
            "macos" => RuntimeOs::Macos,
            _ => return None,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => RuntimeArch::X64,
            "aarch64" => RuntimeArch::Arm64,
            _ => return None,
        };
        Some(Self { os, arch })
    }

    pub(super) fn executable_name(self) -> &'static str {
        if self.os == RuntimeOs::Windows {
            "codex.exe"
        } else {
            "codex"
        }
    }

    pub(super) fn editor_bin_directory(self) -> &'static str {
        match (self.os, self.arch) {
            (RuntimeOs::Windows, RuntimeArch::X64) => "windows-x86_64",
            (RuntimeOs::Windows, RuntimeArch::Arm64) => "windows-aarch64",
            (RuntimeOs::Linux, RuntimeArch::X64) => "linux-x86_64",
            (RuntimeOs::Linux, RuntimeArch::Arm64) => "linux-aarch64",
            (RuntimeOs::Macos, RuntimeArch::X64) => "macos-x86_64",
            (RuntimeOs::Macos, RuntimeArch::Arm64) => "macos-aarch64",
        }
    }

    pub(super) fn cli_target_triple(self) -> &'static str {
        match (self.os, self.arch) {
            (RuntimeOs::Windows, RuntimeArch::X64) => "x86_64-pc-windows-msvc",
            (RuntimeOs::Windows, RuntimeArch::Arm64) => "aarch64-pc-windows-msvc",
            (RuntimeOs::Linux, RuntimeArch::X64) => "x86_64-unknown-linux-musl",
            (RuntimeOs::Linux, RuntimeArch::Arm64) => "aarch64-unknown-linux-musl",
            (RuntimeOs::Macos, RuntimeArch::X64) => "x86_64-apple-darwin",
            (RuntimeOs::Macos, RuntimeArch::Arm64) => "aarch64-apple-darwin",
        }
    }

    pub(super) fn cli_optional_package(self) -> &'static str {
        match (self.os, self.arch) {
            (RuntimeOs::Windows, RuntimeArch::X64) => "codex-win32-x64",
            (RuntimeOs::Windows, RuntimeArch::Arm64) => "codex-win32-arm64",
            (RuntimeOs::Linux, RuntimeArch::X64) => "codex-linux-x64",
            (RuntimeOs::Linux, RuntimeArch::Arm64) => "codex-linux-arm64",
            (RuntimeOs::Macos, RuntimeArch::X64) => "codex-darwin-x64",
            (RuntimeOs::Macos, RuntimeArch::Arm64) => "codex-darwin-arm64",
        }
    }

    #[cfg(windows)]
    pub(super) fn windows_package_arch(self) -> Option<&'static str> {
        match (self.os, self.arch) {
            (RuntimeOs::Windows, RuntimeArch::X64) => Some("x64"),
            (RuntimeOs::Windows, RuntimeArch::Arm64) => Some("arm64"),
            _ => None,
        }
    }
}
