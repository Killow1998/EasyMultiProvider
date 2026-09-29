//! Trusted discovery for the optional Claude Code execution backend.
//!
//! Discovery checks absolute PATH entries first, then only known native,
//! npm/nvm, and Homebrew locations. It does not source shell startup files,
//! invoke package managers, or search recursively through a home directory.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_NODE_INSTALLATIONS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledClaudeCli {
    pub executable: PathBuf,
    pub child_path: OsString,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetOs {
    Windows,
    Linux,
    Macos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetArch {
    X64,
    Arm64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TargetPlatform {
    os: TargetOs,
    arch: TargetArch,
}

impl TargetPlatform {
    fn current() -> Option<Self> {
        let os = match env::consts::OS {
            "windows" => TargetOs::Windows,
            "linux" => TargetOs::Linux,
            "macos" => TargetOs::Macos,
            _ => return None,
        };
        let arch = match env::consts::ARCH {
            "x86_64" => TargetArch::X64,
            "aarch64" => TargetArch::Arm64,
            _ => return None,
        };
        Some(Self { os, arch })
    }

    fn native_binary_name(self) -> &'static str {
        if self.os == TargetOs::Windows {
            "claude.exe"
        } else {
            "claude"
        }
    }

    fn npm_optional_package(self) -> &'static str {
        match (self.os, self.arch) {
            (TargetOs::Windows, TargetArch::X64) => "claude-code-win32-x64",
            (TargetOs::Windows, TargetArch::Arm64) => "claude-code-win32-arm64",
            (TargetOs::Linux, TargetArch::X64) => {
                if cfg!(target_env = "musl") {
                    "claude-code-linux-x64-musl"
                } else {
                    "claude-code-linux-x64"
                }
            }
            (TargetOs::Linux, TargetArch::Arm64) => {
                if cfg!(target_env = "musl") {
                    "claude-code-linux-arm64-musl"
                } else {
                    "claude-code-linux-arm64"
                }
            }
            (TargetOs::Macos, TargetArch::X64) => "claude-code-darwin-x64",
            (TargetOs::Macos, TargetArch::Arm64) => "claude-code-darwin-arm64",
        }
    }

    fn command_names(self) -> &'static [&'static str] {
        if self.os == TargetOs::Windows {
            &["claude.exe", "claude.com", "claude.cmd"]
        } else {
            &["claude"]
        }
    }
}

#[derive(Debug, Clone)]
struct DiscoveryEnvironment {
    platform: TargetPlatform,
    home: Option<PathBuf>,
    user_profile: Option<PathBuf>,
    path: Option<OsString>,
    nvm_dir: Option<PathBuf>,
    nvm_home: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    app_data: Option<PathBuf>,
    npm_prefix: Option<PathBuf>,
    homebrew_prefix: Option<PathBuf>,
}

impl DiscoveryEnvironment {
    fn current() -> Option<Self> {
        let platform = TargetPlatform::current()?;
        let home = env_path("HOME");
        let user_profile = env_path("USERPROFILE");
        Some(Self {
            platform,
            home,
            user_profile,
            path: env::var_os("PATH"),
            nvm_dir: env_path("NVM_DIR"),
            nvm_home: env_path("NVM_HOME"),
            xdg_config_home: env_path("XDG_CONFIG_HOME"),
            app_data: env_path("APPDATA"),
            npm_prefix: env_path("NPM_CONFIG_PREFIX").or_else(|| env_path("npm_config_prefix")),
            homebrew_prefix: env_path("HOMEBREW_PREFIX"),
        })
    }

    fn home_directory(&self) -> Option<&Path> {
        if self.platform.os == TargetOs::Windows {
            self.user_profile.as_deref().or(self.home.as_deref())
        } else {
            self.home.as_deref().or(self.user_profile.as_deref())
        }
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    executable: PathBuf,
    node_directory: Option<PathBuf>,
}

/// Resolve a trusted Claude Code executable from PATH and known install roots.
pub fn resolve_claude_cli() -> Option<InstalledClaudeCli> {
    let environment = DiscoveryEnvironment::current()?;
    resolve_with(&environment, |candidate| {
        let canonical = candidate.canonicalize().ok()?;
        crate::executable_trust::validate_executable(&canonical).ok()?;
        Some(canonical)
    })
}

fn resolve_with(
    environment: &DiscoveryEnvironment,
    trusted_candidate: impl FnMut(&Path) -> Option<PathBuf>,
) -> Option<InstalledClaudeCli> {
    resolve_with_path_adjustment(environment, trusted_candidate, |directory, inherited| {
        crate::runtime_inventory::launcher::path_with_node(directory, inherited)
    })
}

fn resolve_with_path_adjustment(
    environment: &DiscoveryEnvironment,
    mut trusted_candidate: impl FnMut(&Path) -> Option<PathBuf>,
    mut adjust_node_path: impl FnMut(
        &Path,
        Option<OsString>,
    ) -> crate::runtime_inventory::launcher::PathAdjustment,
) -> Option<InstalledClaudeCli> {
    let inherited_path = environment.path.clone().unwrap_or_default();
    for candidate in candidates(environment) {
        let Some(executable) = trusted_candidate(&candidate.executable) else {
            continue;
        };
        let child_path = if is_node_launcher(&executable) {
            let directory = candidate
                .node_directory
                .as_deref()
                .or_else(|| candidate.executable.parent());
            match directory {
                Some(directory) => {
                    use crate::runtime_inventory::launcher::PathAdjustment;
                    match adjust_node_path(directory, Some(inherited_path.clone())) {
                        PathAdjustment::Set(path) => path,
                        PathAdjustment::Inherit => inherited_path.clone(),
                        PathAdjustment::Refuse => continue,
                    }
                }
                None => inherited_path.clone(),
            }
        } else {
            inherited_path.clone()
        };
        return Some(InstalledClaudeCli {
            executable,
            child_path,
        });
    }
    None
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name).map(PathBuf::from)
}

fn candidates(environment: &DiscoveryEnvironment) -> Vec<Candidate> {
    let mut result = Vec::new();

    if let Some(path) = environment.path.as_deref() {
        for directory in env::split_paths(path).filter(|directory| directory.is_absolute()) {
            for name in environment.platform.command_names() {
                add_path_command(&mut result, &directory.join(name), environment.platform);
            }
        }
    }

    if let Some(home) = environment
        .home_directory()
        .filter(|home| home.is_absolute())
    {
        let native = home
            .join(".local")
            .join("bin")
            .join(environment.platform.native_binary_name());
        add_path_command(&mut result, &native, environment.platform);
    }

    add_nvm_candidates(environment, &mut result);
    add_npm_prefix_candidates(environment, &mut result);
    result
}

fn add_path_command(result: &mut Vec<Candidate>, command: &Path, platform: TargetPlatform) {
    if !command.is_file() {
        return;
    }

    if platform.os == TargetOs::Windows
        && command
            .extension()
            .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("cmd"))
    {
        if let Some(package_root) = npm_package_root_from_shim(command) {
            add_npm_package_candidates(
                result,
                &package_root,
                command.parent().map(Path::to_path_buf),
                platform,
            );
        }
        return;
    }

    if let Some(package_root) = npm_package_root_from_binary(command) {
        add_npm_package_candidates(
            result,
            &package_root,
            command.parent().map(Path::to_path_buf),
            platform,
        );
    }
    push_candidate(
        result,
        command.to_path_buf(),
        command.parent().map(Path::to_path_buf),
    );
}

fn add_nvm_candidates(environment: &DiscoveryEnvironment, result: &mut Vec<Candidate>) {
    let home = environment.home_directory();
    let mut roots = Vec::new();
    let mut add_root = |root: PathBuf| {
        if root.is_absolute() && !roots.contains(&root) {
            roots.push(root);
        }
    };
    if let Some(root) = &environment.nvm_dir {
        add_root(root.clone());
    }
    if let Some(root) = &environment.xdg_config_home {
        add_root(root.join("nvm"));
    }
    if let Some(home) = home.filter(|home| home.is_absolute()) {
        add_root(home.join(".config/nvm"));
        add_root(home.join(".nvm"));
    }

    for root in roots {
        add_versioned_nvm_candidates(&root.join("versions/node"), environment.platform, result);
    }

    if environment.platform.os == TargetOs::Windows
        && let Some(root) = &environment.nvm_home
    {
        add_versioned_nvm_candidates(root, environment.platform, result);
    }
}

fn add_versioned_nvm_candidates(
    installations_root: &Path,
    platform: TargetPlatform,
    result: &mut Vec<Candidate>,
) {
    let Ok(entries) = fs::read_dir(installations_root) else {
        return;
    };
    let mut versions = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    versions.sort_by(|left, right| compare_node_versions(left, right));
    versions.truncate(MAX_NODE_INSTALLATIONS);

    for version in versions {
        let bin = version.join("bin");
        for name in platform.command_names() {
            add_path_command(result, &bin.join(name), platform);
        }
        if platform.os == TargetOs::Windows {
            for name in platform.command_names() {
                add_path_command(result, &version.join(name), platform);
            }
            add_npm_package_candidates(
                result,
                &version.join("node_modules/@anthropic-ai/claude-code"),
                Some(bin),
                platform,
            );
        } else {
            add_npm_package_candidates(
                result,
                &version.join("lib/node_modules/@anthropic-ai/claude-code"),
                Some(bin),
                platform,
            );
        }
    }
}

fn add_npm_prefix_candidates(environment: &DiscoveryEnvironment, result: &mut Vec<Candidate>) {
    let platform = environment.platform;
    let mut prefixes = Vec::new();
    let mut add_prefix = |prefix: PathBuf| {
        if prefix.is_absolute() && !prefixes.contains(&prefix) {
            prefixes.push(prefix);
        }
    };
    if let Some(prefix) = &environment.npm_prefix {
        add_prefix(prefix.clone());
    }
    if let Some(prefix) = &environment.homebrew_prefix {
        add_prefix(prefix.clone());
    }

    match platform.os {
        TargetOs::Windows => {
            if let Some(app_data) = &environment.app_data {
                add_prefix(app_data.join("npm"));
            }
            if let Some(profile) = environment
                .user_profile
                .as_deref()
                .filter(|profile| profile.is_absolute())
            {
                add_prefix(profile.join("AppData/Roaming/npm"));
            }
        }
        TargetOs::Linux => {
            add_prefix(PathBuf::from("/usr"));
            add_prefix(PathBuf::from("/usr/local"));
            add_prefix(PathBuf::from("/home/linuxbrew/.linuxbrew"));
            if let Some(home) = environment
                .home_directory()
                .filter(|home| home.is_absolute())
            {
                add_prefix(home.join(".linuxbrew"));
            }
        }
        TargetOs::Macos => {
            add_prefix(PathBuf::from("/opt/homebrew"));
            add_prefix(PathBuf::from("/usr/local"));
        }
    }

    for prefix in prefixes {
        match platform.os {
            TargetOs::Windows => {
                for name in platform.command_names() {
                    add_path_command(result, &prefix.join(name), platform);
                }
                add_npm_package_candidates(
                    result,
                    &prefix.join("node_modules/@anthropic-ai/claude-code"),
                    None,
                    platform,
                );
            }
            TargetOs::Linux | TargetOs::Macos => {
                let bin = prefix.join("bin");
                add_path_command(result, &bin.join("claude"), platform);
                add_npm_package_candidates(
                    result,
                    &prefix.join("lib/node_modules/@anthropic-ai/claude-code"),
                    Some(bin),
                    platform,
                );
            }
        }
    }
}

fn add_npm_package_candidates(
    result: &mut Vec<Candidate>,
    package_root: &Path,
    node_directory: Option<PathBuf>,
    platform: TargetPlatform,
) {
    if !package_root.is_absolute() {
        return;
    }

    let mut native = Vec::new();
    let mut scripts = Vec::new();
    let mut add_main = |path: PathBuf| {
        if !path.is_file() {
            return;
        }
        if platform.os != TargetOs::Windows && is_node_launcher(&path) {
            scripts.push(path);
        } else if !is_node_launcher(&path) {
            native.push(path);
        }
    };
    add_main(package_root.join("bin").join(platform.native_binary_name()));
    if platform.os != TargetOs::Windows {
        add_main(package_root.join("bin/claude"));
    }

    let optional_binary = package_root
        .join("node_modules/@anthropic-ai")
        .join(platform.npm_optional_package())
        .join(platform.native_binary_name());
    if optional_binary.is_file() {
        native.push(optional_binary);
    }

    for path in native {
        push_candidate(result, path, None);
    }
    for path in scripts {
        push_candidate(result, path, node_directory.clone());
    }
}

fn npm_package_root_from_binary(binary: &Path) -> Option<PathBuf> {
    let canonical = binary.canonicalize().ok()?;
    let bin = canonical.parent()?;
    if file_name(bin) != Some("bin") {
        return None;
    }
    let root = bin.parent()?;
    if file_name(root) != Some("claude-code")
        || root.parent().and_then(|path| file_name(path)) != Some("@anthropic-ai")
        || root
            .parent()
            .and_then(Path::parent)
            .and_then(|path| file_name(path))
            != Some("node_modules")
    {
        return None;
    }
    Some(root.to_path_buf())
}

fn npm_package_root_from_shim(shim: &Path) -> Option<PathBuf> {
    let parent = shim.parent()?;
    if file_name(parent) == Some(".bin")
        && parent.parent().and_then(|path| file_name(path)) == Some("node_modules")
    {
        Some(parent.parent()?.join("@anthropic-ai/claude-code"))
    } else {
        Some(parent.join("node_modules/@anthropic-ai/claude-code"))
    }
}

fn file_name(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()
}

fn push_candidate(
    result: &mut Vec<Candidate>,
    executable: PathBuf,
    node_directory: Option<PathBuf>,
) {
    if !executable.is_absolute() || !executable.is_file() {
        return;
    }
    if result
        .iter()
        .any(|candidate| candidate.executable == executable)
    {
        return;
    }
    result.push(Candidate {
        executable,
        node_directory,
    });
}

fn is_node_launcher(path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut bytes = [0_u8; 256];
    let Ok(length) = std::io::Read::read(&mut file, &mut bytes) else {
        return false;
    };
    let Some(first_line) = bytes[..length].split(|byte| *byte == b'\n').next() else {
        return false;
    };
    first_line.starts_with(b"#!")
        && String::from_utf8_lossy(first_line)
            .to_ascii_lowercase()
            .contains("node")
}

fn compare_node_versions(left: &Path, right: &Path) -> std::cmp::Ordering {
    fn version(path: &Path) -> Option<(u64, u64, u64, bool)> {
        let name = path.file_name()?.to_str()?.strip_prefix('v')?;
        let stable = !name.contains('-');
        let core = name.split(['-', '+']).next()?;
        let mut parts = core.split('.');
        Some((
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            stable,
        ))
    }
    match (version(left), version(right)) {
        (Some(left_version), Some(right_version)) => right_version
            .cmp(&left_version)
            .then_with(|| right.file_name().cmp(&left.file_name())),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => right.file_name().cmp(&left.file_name()),
    }
}

#[cfg(test)]
mod tests;
