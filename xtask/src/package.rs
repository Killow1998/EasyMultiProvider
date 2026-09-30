//! Build and smoke-test one native EMP distribution into `artifacts/`.
use crate::{
    PRODUCT_NAME, Result, VERSION, copy, make_executable, project_root, run_command, windows,
};
use flate2::Compression;
use flate2::write::GzEncoder;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const RUST_TOOLCHAIN: &str = "1.93.1";
const RELEASE_DOCUMENTS: [&str; 4] = [
    "README.md",
    "README.zh-CN.md",
    "LICENSE",
    "THIRD_PARTY_NOTICES.md",
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    Linux,
    Macos,
}

#[derive(Clone, Copy)]
pub struct Target {
    pub os: Os,
    pub arch: &'static str,
}

impl Target {
    pub fn current() -> Result<Self> {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x86_64",
            "aarch64" => "arm64",
            other => return Err(format!("unsupported packaging architecture: {other}")),
        };
        let os = match (std::env::consts::OS, arch) {
            ("windows", "x86_64") => Os::Windows,
            ("linux", "x86_64") => Os::Linux,
            ("macos", _) => Os::Macos,
            (os, arch) => return Err(format!("unsupported packaging target: {os}/{arch}")),
        };
        Ok(Self { os, arch })
    }

    pub fn identity(self) -> String {
        let os = match self.os {
            Os::Windows => "windows",
            Os::Linux => "linux",
            Os::Macos => "macos",
        };
        format!("{os}-{}", self.arch)
    }

    pub fn executable_suffix(self) -> &'static str {
        if self.os == Os::Windows { ".exe" } else { "" }
    }

    /// File name shared by the raw executable and its archives.
    pub fn artifact_base(self) -> String {
        if self.os == Os::Windows {
            PRODUCT_NAME.to_owned()
        } else {
            format!("{PRODUCT_NAME}-{}", self.identity())
        }
    }

    /// The package the in-app updater downloads for this platform.
    pub fn update_package(self) -> String {
        let base = self.artifact_base();
        match self.os {
            Os::Windows => format!("{base}.exe"),
            Os::Linux => format!("{base}.tar.gz"),
            Os::Macos => format!("{base}.dmg"),
        }
    }
}

pub fn run(args: &[String]) -> Result {
    let skip_service_smoke = match args {
        [] => false,
        [flag] if flag == "--skip-service-smoke" => true,
        _ => return Err("usage: cargo xtask package [--skip-service-smoke]".to_owned()),
    };
    let root = project_root();
    for artifact in build(&root, skip_service_smoke)? {
        println!(
            "{}",
            artifact.strip_prefix(&root).unwrap_or(&artifact).display()
        );
    }
    Ok(())
}

fn build(root: &Path, skip_service_smoke: bool) -> Result<Vec<PathBuf>> {
    let target = Target::current()?;
    let build_root = root.join("build/standalone").join(target.identity());
    let artifacts = root.join("artifacts");
    if build_root.exists() {
        fs::remove_dir_all(&build_root).map_err(|error| format!("clean build root: {error}"))?;
    }
    fs::create_dir_all(&build_root).map_err(|error| format!("create build root: {error}"))?;
    fs::create_dir_all(&artifacts).map_err(|error| format!("create artifacts: {error}"))?;

    let base = target.artifact_base();
    let mut suffixes = vec![target.executable_suffix()];
    suffixes.push(if target.os == Os::Windows {
        ".zip"
    } else {
        ".tar.gz"
    });
    match target.os {
        Os::Linux => suffixes.extend(["-install.sh", ".deb"]),
        Os::Macos => suffixes.push(".dmg"),
        Os::Windows => {}
    }
    for suffix in suffixes {
        for stale in [format!("{base}{suffix}"), format!("{base}{suffix}.sha256")] {
            let _ = fs::remove_file(artifacts.join(stale));
        }
    }

    let icons = root.join("assets/branding/native");
    let resource = if target.os == Os::Windows {
        let script = windows::write_resource_script(
            &build_root.join("windows-resources"),
            &icons.join("easy-multi-provider.ico"),
            VERSION,
        )?;
        Some(windows::compile_resource(&script)?)
    } else {
        None
    };
    let executable = build_binary(root, target, resource.as_deref())?;
    if target.os == Os::Windows {
        windows::validate_executable(&executable, VERSION)?;
    }
    probe_version(&executable)?;
    if !skip_service_smoke {
        smoke_service(root, &executable, target)?;
    }

    let raw = artifacts.join(format!("{base}{}", target.executable_suffix()));
    copy(&executable, &raw)?;
    make_executable(&raw)?;
    let mut outputs = vec![raw];
    if target.os == Os::Windows {
        let archive = artifacts.join(format!("{base}.zip"));
        write_zip(&build_root, &archive, &executable, target)?;
        outputs.push(archive);
    } else {
        let archive = artifacts.join(format!("{base}.tar.gz"));
        write_tar(&build_root, &archive, &executable, target)?;
        if target.os == Os::Linux {
            validate_linux_archive(&archive, &executable)?;
        }
        outputs.push(archive);
    }
    if target.os == Os::Linux {
        let installer = artifacts.join(format!("{base}-install.sh"));
        copy(&root.join("packaging/install-linux.sh"), &installer)?;
        make_executable(&installer)?;
        outputs.push(installer);
    }
    if target.os == Os::Macos {
        let dmg = artifacts.join(format!("{base}.dmg"));
        write_dmg(
            root,
            &build_root,
            &dmg,
            &executable,
            &icons.join("easy-multi-provider.icns"),
        )?;
        outputs.push(dmg);
    }
    let mut checksums = Vec::new();
    for artifact in &outputs {
        checksums.push(write_checksum(artifact)?);
    }
    outputs.extend(checksums);
    Ok(outputs)
}

fn build_binary(root: &Path, target: Target, resource: Option<&Path>) -> Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let version = Command::new(&cargo)
        .arg("--version")
        .output()
        .map_err(|error| format!("Rust Cargo is required to build EMP: {error}"))?;
    if !String::from_utf8_lossy(&version.stdout).starts_with(&format!("cargo {RUST_TOOLCHAIN} ")) {
        return Err(format!(
            "Rust Cargo {RUST_TOOLCHAIN} is required to build EMP"
        ));
    }
    let mut command = Command::new(&cargo);
    command.current_dir(root);
    if std::env::var("CARGO_NET_OFFLINE")
        .is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
    {
        command.arg("--offline");
    }
    command.args([
        "build",
        "--locked",
        "--release",
        "--package",
        "emp-app",
        "--bin",
        PRODUCT_NAME,
    ]);
    if target.os == Os::Windows {
        let resource =
            resource.ok_or("Windows packaging requires compiled icon/version resources")?;
        command
            .env("EMP_PACKAGE_RESOURCE", resource)
            .env("EMP_REQUIRE_WINDOWS_RESOURCES", "1");
    }
    append_package_path_remaps(&mut command, root)?;
    run_command(&mut command)?;
    let target_root = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map_or_else(|| root.join("target"), |path| root.join(path));
    let executable = target_root
        .join("release")
        .join(format!("{PRODUCT_NAME}{}", target.executable_suffix()));
    if !executable.is_file() {
        return Err("Cargo did not produce the expected EMP executable".to_owned());
    }
    make_executable(&executable)?;
    Ok(executable)
}

fn append_package_path_remaps(command: &mut Command, root: &Path) -> Result {
    let remap_flags = package_path_remap_flags(root)?;
    let rustflags = merge_rustflags(
        std::env::var_os("CARGO_ENCODED_RUSTFLAGS").as_deref(),
        std::env::var_os("RUSTFLAGS").as_deref(),
        &remap_flags,
    )?;
    command.env("CARGO_ENCODED_RUSTFLAGS", rustflags);
    Ok(())
}

fn package_path_remap_flags(root: &Path) -> Result<Vec<String>> {
    let workspace = crate::physical(root)?;
    let home = user_home_directory();
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|path| path.join(".cargo")));
    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|path| path.join(".rustup")));
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"));
    let roots = [
        (Some(workspace.clone()), "/workspace"),
        (Some(target_dir), "/build"),
        (cargo_home, "/cargo"),
        (rustup_home, "/rustup"),
        (home, "/user"),
    ];
    let mut remaps = Vec::new();
    for (path, destination) in roots {
        let Some(path) = path else {
            continue;
        };
        let path = if path.is_absolute() {
            path
        } else {
            workspace.join(path)
        };
        if !path.is_dir() {
            continue;
        }
        let path = crate::physical(&path)
            .map_err(|_| "could not resolve a package source directory for remapping".to_owned())?;
        if remaps.iter().any(|(existing, _)| existing == &path) {
            continue;
        }
        let source = path
            .to_str()
            .ok_or_else(|| "package source path remapping requires Unicode paths".to_owned())?
            .to_owned();
        remaps.push((path, format!("--remap-path-prefix={source}={destination}")));
    }
    // Rustc applies the last matching prefix, so nested roots must follow their parents.
    remaps.sort_by(|(left, _), (right, _)| {
        left.components().count().cmp(&right.components().count())
    });
    Ok(remaps.into_iter().map(|(_, flag)| flag).collect())
}

#[cfg(windows)]
fn user_home_directory() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|| {
            let mut home = std::env::var_os("HOMEDRIVE")?;
            home.push(std::env::var_os("HOMEPATH")?);
            Some(PathBuf::from(home))
        })
}

#[cfg(not(windows))]
fn user_home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn merge_rustflags(
    encoded: Option<&OsStr>,
    plain: Option<&OsStr>,
    additions: &[String],
) -> Result<OsString> {
    if let Some(encoded) = encoded {
        let mut merged = encoded.to_os_string();
        if !merged.is_empty() && !additions.is_empty() {
            merged.push("\u{1f}");
        }
        merged.push(additions.join("\u{1f}"));
        return Ok(merged);
    }
    if let Some(plain) = plain {
        let plain = plain.to_str().ok_or_else(|| {
            "RUSTFLAGS must be Unicode to preserve package path remapping".to_owned()
        })?;
        let mut flags = plain
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        flags.extend_from_slice(additions);
        return Ok(OsString::from(flags.join("\u{1f}")));
    }
    Ok(OsString::from(additions.join("\u{1f}")))
}

fn probe_version(executable: &Path) -> Result {
    let output = Command::new(executable)
        .arg("--version")
        .output()
        .map_err(|error| format!("packaged version probe failed: {error}"))?;
    let reported = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || reported.trim() != format!("{PRODUCT_NAME} {VERSION}") {
        return Err(format!(
            "packaged version probe returned {:?}",
            reported.trim()
        ));
    }
    Ok(())
}

pub fn reserve_loopback_port() -> Result<u16> {
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|error| format!("reserve port: {error}"))?;
    Ok(listener
        .local_addr()
        .map_err(|error| format!("reserve port: {error}"))?
        .port())
}

/// Starts the packaged service in an isolated home, then checks health, the
/// served Web UI bytes, the one-use bootstrap exchange and a session request.
fn smoke_service(root: &Path, executable: &Path, target: Target) -> Result {
    let temporary = crate::temporary_directory("emp-package-smoke-")?;
    let home = crate::physical(temporary.path())?;
    let codex_home = home.join("codex-home");
    fs::create_dir(&codex_home).map_err(|error| error.to_string())?;
    let port = reserve_loopback_port()?;
    let config = home.join("config.json");
    fs::write(
        &config,
        serde_json::json!({"host": "127.0.0.1", "port": port}).to_string(),
    )
    .map_err(|error| error.to_string())?;
    let output_path = home.join("service-output.txt");
    let output = File::create(&output_path).map_err(|error| error.to_string())?;
    let mut command = Command::new(executable);
    command
        .args(["serve", "--config"])
        .arg(&config)
        .args(["--port", &port.to_string()])
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("EASY_MULTI_PROVIDER_MASTER_KEY", "")
        .env(
            "EASY_MULTI_PROVIDER_MASTER_KEY_FILE",
            home.join("state/master.key"),
        )
        .env_remove("EMP_UPDATE_READY")
        .env_remove("EMP_UPDATE_RESULT")
        .stdin(Stdio::null())
        .stdout(output.try_clone().map_err(|error| error.to_string())?)
        .stderr(output);
    match target.os {
        Os::Windows => command.env("LOCALAPPDATA", home.join("local-app-data")),
        Os::Macos => command.env("HOME", home.join("home")),
        Os::Linux => command.env("XDG_CONFIG_HOME", home.join("xdg-config")),
    };
    let mut service = KillOnDrop(
        command
            .spawn()
            .map_err(|error| format!("start packaged service: {error}"))?,
    );
    let expected_ui =
        fs::read(root.join("crates/emp-app/web/index.html")).map_err(|error| error.to_string())?;

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last_error = String::from("no response");
    while Instant::now() < deadline {
        if let Ok(Some(status)) = service.0.try_wait() {
            let log = fs::read_to_string(&output_path).unwrap_or_default();
            return Err(format!(
                "packaged service exited ({status}) during smoke test: {}",
                log.trim()
            ));
        }
        let log = fs::read_to_string(&output_path).unwrap_or_default();
        let Some(bootstrap) = log
            .lines()
            .find_map(|line| line.strip_prefix("Open in browser: "))
            .and_then(|url| {
                url.split_once("bootstrap=")
                    .map(|(_, token)| token.split('&').next().unwrap_or("").trim().to_owned())
            })
        else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        match http(port, "GET", "/healthz", &[]) {
            Ok((200, _)) => {}
            Ok((status, _)) => {
                last_error = format!("health check returned HTTP {status}");
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(error) => {
                last_error = error;
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        }
        // The page carries no secrets; its script exchanges the one-use
        // bootstrap token for a header session.
        let (status, body) = http(port, "GET", "/", &[])?;
        if status != 200 {
            return Err(format!("packaged UI returned HTTP {status}"));
        }
        if body != expected_ui {
            return Err("packaged service did not serve the source Web UI bytes".to_owned());
        }
        let (status, body) = http(
            port,
            "POST",
            "/api/session",
            &[("X-EMP-Bootstrap", &bootstrap)],
        )?;
        let session = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value.get("session")?.as_str().map(str::to_owned))
            .filter(|session| status == 200 && !session.is_empty() && !bootstrap.is_empty())
            .ok_or("packaged service bootstrap did not establish a session")?;
        let (status, _) = http(port, "GET", "/api/config", &[("X-EMP-Session", &session)])?;
        if status != 200 {
            return Err(format!("packaged session was rejected with HTTP {status}"));
        }
        return Ok(());
    }
    Err(format!(
        "packaged service did not become ready: {last_error}"
    ))
}

pub struct KillOnDrop(pub Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One HTTP/1.1 request on a fresh loopback connection; returns status and body.
pub fn http(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> Result<(u16, Vec<u8>)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| error.to_string())?;
    let body = if method == "POST" { "{}" } else { "" };
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream
        .write_all(request.as_bytes())
        .map_err(|error| error.to_string())?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|error| error.to_string())?;
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("incomplete HTTP response")?;
    let head = String::from_utf8_lossy(&response[..split]).to_ascii_lowercase();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or("invalid HTTP status line")?;
    let body = response[split + 4..].to_vec();
    if head.contains("\r\ntransfer-encoding: chunked") {
        return Ok((status, dechunk(&body)?));
    }
    Ok((status, body))
}

fn dechunk(mut raw: &[u8]) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line_end = raw
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or("invalid chunk")?;
        let size_text = String::from_utf8_lossy(&raw[..line_end]);
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "invalid chunk size")?;
        raw = &raw[line_end + 2..];
        if size == 0 {
            return Ok(body);
        }
        body.extend_from_slice(raw.get(..size).ok_or("truncated chunk")?);
        raw = raw.get(size + 2..).ok_or("truncated chunk")?;
    }
}

fn stage_release_files(
    root: &Path,
    destination: &Path,
    executable: &Path,
    target: Target,
) -> Result {
    fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    let binary = destination.join(format!("{PRODUCT_NAME}{}", target.executable_suffix()));
    copy(executable, &binary)?;
    make_executable(&binary)?;
    for name in RELEASE_DOCUMENTS {
        copy(&root.join(name), &destination.join(name))?;
    }
    if target.os == Os::Linux {
        let installer = destination.join("install-user.sh");
        let script =
            fs::read(root.join("packaging/install-user.sh")).map_err(|error| error.to_string())?;
        fs::write(
            &installer,
            String::from_utf8_lossy(&script).replace("\r\n", "\n"),
        )
        .map_err(|error| error.to_string())?;
        make_executable(&installer)?;
        copy(
            &root.join("assets/branding/easy-multi-provider-icon.svg"),
            &destination.join("easy-multi-provider.svg"),
        )?;
    }
    Ok(())
}

fn write_zip(build_root: &Path, output: &Path, executable: &Path, target: Target) -> Result {
    let stage = build_root.join("zip-root");
    stage_release_files(
        &project_root(),
        &stage.join(PRODUCT_NAME),
        executable,
        target,
    )?;
    // Windows ships bsdtar, which writes zip archives selected by `--format`.
    let system_root = std::env::var_os("SystemRoot").ok_or("SystemRoot is not set")?;
    run_command(
        Command::new(Path::new(&system_root).join("System32/tar.exe"))
            .current_dir(&stage)
            .args(["--format", "zip", "-cf"])
            .arg(output)
            .arg(PRODUCT_NAME),
    )
}

fn write_tar(build_root: &Path, output: &Path, executable: &Path, target: Target) -> Result {
    let stage = build_root.join("tar-root");
    stage_release_files(&project_root(), &stage, executable, target)?;
    let file = File::create(output).map_err(|error| error.to_string())?;
    let mut archive = tar::Builder::new(GzEncoder::new(file, Compression::default()));
    archive.mode(tar::HeaderMode::Deterministic);
    archive
        .append_dir_all(PRODUCT_NAME, &stage)
        .map_err(|error| format!("write {}: {error}", output.display()))?;
    archive
        .into_inner()
        .and_then(GzEncoder::finish)
        .map(drop)
        .map_err(|error| format!("write {}: {error}", output.display()))
}

fn validate_linux_archive(archive_path: &Path, executable: &Path) -> Result {
    let expected: BTreeSet<&str> = [
        "EMP",
        "EMP/EMP",
        "EMP/install-user.sh",
        "EMP/easy-multi-provider.svg",
        "EMP/README.md",
        "EMP/README.zh-CN.md",
        "EMP/LICENSE",
        "EMP/THIRD_PARTY_NOTICES.md",
    ]
    .into();
    let binary = fs::read(executable).map_err(|error| error.to_string())?;
    let file = File::open(archive_path).map_err(|error| error.to_string())?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut seen = BTreeSet::new();
    for entry in archive.entries().map_err(|error| error.to_string())? {
        let mut entry = entry.map_err(|error| error.to_string())?;
        let path = entry
            .path()
            .map_err(|error| error.to_string())?
            .into_owned();
        let name = path.to_string_lossy().trim_end_matches('/').to_owned();
        if path.is_absolute()
            || path
                .components()
                .any(|part| part == std::path::Component::ParentDir)
        {
            return Err("Linux archive contains an unsafe path".to_owned());
        }
        let kind = entry.header().entry_type();
        if (name == "EMP" && !kind.is_dir()) || (name != "EMP" && !kind.is_file()) {
            return Err("Linux archive may contain only regular files and its root".to_owned());
        }
        if name.ends_with(".py")
            || name.ends_with(".pyc")
            || name.to_ascii_lowercase().contains("python")
        {
            return Err("Linux archive must not contain a Python runtime".to_owned());
        }
        if name == "EMP/EMP" {
            if entry.header().mode().map_err(|error| error.to_string())? & 0o111 == 0 {
                return Err("Linux EMP executable is not marked executable".to_owned());
            }
            let mut bundled = Vec::new();
            entry
                .read_to_end(&mut bundled)
                .map_err(|error| error.to_string())?;
            if bundled != binary {
                return Err("Linux archive does not contain the Cargo-built EMP binary".to_owned());
            }
        }
        if !seen.insert(name) {
            return Err("Linux archive contains duplicate paths".to_owned());
        }
    }
    if seen.iter().map(String::as_str).collect::<BTreeSet<_>>() != expected {
        return Err("Linux archive layout differs from the updater contract".to_owned());
    }
    Ok(())
}

fn info_plist(version: &str) -> String {
    let bundle_version = version.split(['-', '+']).next().unwrap_or(version);
    let strings = [
        ("CFBundleDevelopmentRegion", "en"),
        ("CFBundleDisplayName", PRODUCT_NAME),
        ("CFBundleExecutable", PRODUCT_NAME),
        ("CFBundleIconFile", "easy-multi-provider.icns"),
        (
            "CFBundleIdentifier",
            "io.github.Killow1998.EasyMultiProvider",
        ),
        ("CFBundleInfoDictionaryVersion", "6.0"),
        ("CFBundleName", PRODUCT_NAME),
        ("CFBundlePackageType", "APPL"),
        ("CFBundleShortVersionString", bundle_version),
        ("CFBundleVersion", bundle_version),
    ];
    let mut plist = String::from(concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
        "<plist version=\"1.0\">\n<dict>\n",
    ));
    for (key, value) in strings {
        plist.push_str(&format!("\t<key>{key}</key>\n\t<string>{value}</string>\n"));
    }
    plist.push_str("\t<key>NSHighResolutionCapable</key>\n\t<true/>\n</dict>\n</plist>\n");
    plist
}

fn write_macos_app(destination: &Path, executable: &Path, icon: &Path) -> Result {
    let contents = destination.join("Contents");
    let launchers = contents.join("MacOS");
    let resources = contents.join("Resources");
    for directory in [&launchers, &resources] {
        fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    }
    copy(executable, &resources.join(PRODUCT_NAME))?;
    make_executable(&resources.join(PRODUCT_NAME))?;
    copy(icon, &resources.join("easy-multi-provider.icns"))?;
    let scripts = [
        (
            launchers.join(PRODUCT_NAME),
            "#!/bin/sh\nset -eu\ncontents_dir=$(CDPATH= cd \"$(dirname \"$0\")/..\" && pwd)\nexec /usr/bin/open -a Terminal \"$contents_dir/Resources/launch.command\"\n",
        ),
        (
            resources.join("launch.command"),
            "#!/bin/sh\nset -eu\nresources_dir=$(CDPATH= cd \"$(dirname \"$0\")\" && pwd)\nconfig_path=\"$HOME/Library/Application Support/EasyMultiProvider/config.json\"\nexec \"$resources_dir/EMP\" serve --config \"$config_path\" --open-browser\n",
        ),
    ];
    for (path, body) in scripts {
        fs::write(&path, body).map_err(|error| error.to_string())?;
        make_executable(&path)?;
    }
    fs::write(contents.join("Info.plist"), info_plist(VERSION)).map_err(|error| error.to_string())
}

const HDIUTIL_CREATE_MAX_ATTEMPTS: usize = 3;
const HDIUTIL_CREATE_RETRY_DELAYS: [Duration; HDIUTIL_CREATE_MAX_ATTEMPTS - 1] =
    [Duration::from_millis(250), Duration::from_millis(750)];

fn is_hdiutil_resource_busy(stderr: &[u8]) -> bool {
    stderr.split(|byte| *byte == b'\n').any(|line| {
        line.strip_suffix(b"\r").unwrap_or(line) == b"hdiutil: create failed - Resource busy"
    })
}

fn run_hdiutil_create(command: &mut Command) -> Result {
    for attempt in 1..=HDIUTIL_CREATE_MAX_ATTEMPTS {
        let output = command
            .output()
            .map_err(|error| format!("could not run {:?}: {error}", command.get_program()))?;
        std::io::stdout()
            .write_all(&output.stdout)
            .map_err(|error| format!("could not write hdiutil stdout: {error}"))?;
        std::io::stderr()
            .write_all(&output.stderr)
            .map_err(|error| format!("could not write hdiutil stderr: {error}"))?;

        if output.status.success() {
            return Ok(());
        }
        if !is_hdiutil_resource_busy(&output.stderr) || attempt == HDIUTIL_CREATE_MAX_ATTEMPTS {
            return Err(format!(
                "{:?} failed with {}",
                command.get_program(),
                output.status
            ));
        }

        eprintln!(
            "hdiutil create reported transient Resource busy; retrying ({attempt}/{HDIUTIL_CREATE_MAX_ATTEMPTS})"
        );
        std::thread::sleep(HDIUTIL_CREATE_RETRY_DELAYS[attempt - 1]);
    }

    unreachable!("hdiutil create attempts are bounded")
}

fn write_dmg(
    root: &Path,
    build_root: &Path,
    output: &Path,
    executable: &Path,
    icon: &Path,
) -> Result {
    let stage = build_root.join("dmg-root");
    fs::create_dir_all(&stage).map_err(|error| error.to_string())?;
    write_macos_app(&stage.join(format!("{PRODUCT_NAME}.app")), executable, icon)?;
    for name in RELEASE_DOCUMENTS {
        copy(&root.join(name), &stage.join(name))?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink("/Applications", stage.join("Applications"))
        .map_err(|error| error.to_string())?;
    let mut command = Command::new("hdiutil");
    command
        .args(["create", "-volname", PRODUCT_NAME, "-srcfolder"])
        .arg(&stage)
        .args(["-ov", "-format", "UDZO"])
        .arg(output);
    run_hdiutil_create(&mut command)
}

fn write_checksum(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or("artifact has no file name")?
        .to_string_lossy();
    let checksum = path.with_file_name(format!("{name}.sha256"));
    fs::write(
        &checksum,
        format!("{}  {name}\n", crate::sha256_file(path)?),
    )
    .map_err(|error| error.to_string())?;
    Ok(checksum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn fake_hdiutil(directory: &Path, action: &str) -> Command {
        let executable = directory.join("fake-hdiutil");
        let body = format!(
            r#"count=0
if [ -f "$FAKE_HDIUTIL_COUNT" ]; then
  count=$(cat "$FAKE_HDIUTIL_COUNT")
fi
count=$((count + 1))
printf '%s' "$count" > "$FAKE_HDIUTIL_COUNT"
{action}
"#
        );
        fs::write(&executable, format!("#!/bin/sh\nset -eu\n{body}")).unwrap();
        make_executable(&executable).unwrap();
        let mut command = Command::new(executable);
        command.env("FAKE_HDIUTIL_COUNT", directory.join("count"));
        command
    }

    #[test]
    fn hdiutil_busy_match_requires_the_exact_transient_diagnostic() {
        assert!(is_hdiutil_resource_busy(
            b"hdiutil: create failed - Resource busy\r\n"
        ));
        assert!(!is_hdiutil_resource_busy(
            b"hdiutil: create failed - Operation not permitted\n"
        ));
        assert!(!is_hdiutil_resource_busy(
            b"other command failed - Resource busy\n"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn hdiutil_create_recovers_from_one_transient_busy_failure() {
        let directory = crate::temporary_directory("hdiutil-retry-").unwrap();
        let mut command = fake_hdiutil(
            directory.path(),
            r#"if [ "$count" -eq 1 ]; then
  echo 'hdiutil: create failed - Resource busy' >&2
  exit 1
fi
echo created"#,
        );

        run_hdiutil_create(&mut command).unwrap();

        assert_eq!(
            fs::read_to_string(directory.path().join("count")).unwrap(),
            "2"
        );
    }

    #[cfg(unix)]
    #[test]
    fn hdiutil_create_stops_after_three_busy_failures() {
        let directory = crate::temporary_directory("hdiutil-busy-limit-").unwrap();
        let mut command = fake_hdiutil(
            directory.path(),
            "echo 'hdiutil: create failed - Resource busy' >&2\nexit 1",
        );

        assert!(run_hdiutil_create(&mut command).is_err());

        assert_eq!(
            fs::read_to_string(directory.path().join("count")).unwrap(),
            "3"
        );
    }

    #[cfg(unix)]
    #[test]
    fn hdiutil_create_does_not_retry_other_failures() {
        let directory = crate::temporary_directory("hdiutil-other-error-").unwrap();
        let mut command = fake_hdiutil(
            directory.path(),
            "echo 'hdiutil: create failed - Operation not permitted' >&2\nexit 1",
        );

        assert!(run_hdiutil_create(&mut command).is_err());

        assert_eq!(
            fs::read_to_string(directory.path().join("count")).unwrap(),
            "1"
        );
    }

    #[test]
    fn package_path_remaps_preserve_encoded_rustflags() {
        let additions = ["--remap-path-prefix=/cargo cache=/cargo".to_owned()];
        let merged = merge_rustflags(
            Some(OsStr::new("-C\u{1f}debuginfo=2")),
            Some(OsStr::new("-D ignored")),
            &additions,
        )
        .unwrap();
        assert_eq!(
            merged.to_str().unwrap().split('\u{1f}').collect::<Vec<_>>(),
            [
                "-C",
                "debuginfo=2",
                "--remap-path-prefix=/cargo cache=/cargo"
            ]
        );
    }

    #[test]
    fn package_path_remaps_convert_plain_rustflags_by_whitespace() {
        let addition = "--remap-path-prefix=/cargo=/cargo";
        let additions = [addition.to_owned()];
        let merged = merge_rustflags(
            None,
            Some(OsStr::new("-C opt-level=2  --cfg plain_flag")),
            &additions,
        )
        .unwrap();
        assert_eq!(
            merged.to_str().unwrap().split('\u{1f}').collect::<Vec<_>>(),
            ["-C", "opt-level=2", "--cfg", "plain_flag", addition]
        );
    }

    #[test]
    fn package_path_remaps_use_generic_workspace_prefixes() {
        let flags = package_path_remap_flags(&crate::project_root()).unwrap();
        assert!(flags.iter().any(|flag| flag.ends_with("=/workspace")));
        assert!(
            flags
                .iter()
                .all(|flag| flag.starts_with("--remap-path-prefix="))
        );
    }

    #[test]
    fn info_plist_lists_sorted_keys_with_the_numeric_bundle_version() {
        let plist = info_plist("0.12.2");
        assert!(plist.contains("<key>CFBundleShortVersionString</key>\n\t<string>0.12.2</string>"));
        let keys: Vec<_> = plist
            .lines()
            .filter_map(|line| line.trim().strip_prefix("<key>")?.strip_suffix("</key>"))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted);
        assert_eq!(keys.len(), 11);
    }

    #[test]
    fn dechunk_joins_chunks_and_rejects_truncation() {
        assert_eq!(
            dechunk(b"3\r\nabc\r\n2;x=1\r\nde\r\n0\r\n\r\n").unwrap(),
            b"abcde"
        );
        assert!(dechunk(b"5\r\nab").is_err());
    }

    #[test]
    fn update_package_names_match_the_release_manifest() {
        let names: Vec<_> = [
            Target {
                os: Os::Windows,
                arch: "x86_64",
            },
            Target {
                os: Os::Linux,
                arch: "x86_64",
            },
            Target {
                os: Os::Macos,
                arch: "x86_64",
            },
            Target {
                os: Os::Macos,
                arch: "arm64",
            },
        ]
        .into_iter()
        .map(Target::update_package)
        .collect();
        // Intel and Apple Silicon Macs each get their own native disk image.
        assert_eq!(
            names,
            [
                "EMP.exe",
                "EMP-linux-x86_64.tar.gz",
                "EMP-macos-x86_64.dmg",
                "EMP-macos-arm64.dmg"
            ]
        );
    }
}
