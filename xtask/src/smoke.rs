//! Smoke tests for a freshly built package in `artifacts/`: the in-app update
//! worker replaces or restores a synthetic installation, and on Linux the
//! archive installer sets up a desktop user. No real Codex state is touched.
mod parent;

use crate::package::{KillOnDrop, Os, Target, http, reserve_loopback_port};
use crate::{PRODUCT_NAME, Result, copy, physical, project_root, sha256_file, temporary_directory};
use emp_state::update::extract::extract_candidate;
use emp_state::update::worker::installation_target;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn run(args: &[String]) -> Result {
    if !args.is_empty() {
        return Err("usage: cargo xtask package-smoke".to_owned());
    }
    let target = Target::current()?;
    let artifacts = project_root().join("artifacts");
    let package = artifacts.join(target.update_package());
    let binary = artifacts.join(format!(
        "{}{}",
        target.artifact_base(),
        target.executable_suffix()
    ));
    if !package.is_file() || !binary.is_file() {
        return Err("build the native package first: cargo xtask package".to_owned());
    }
    for fails_startup in [false, true] {
        update_scenario(target, &package, &binary, fails_startup)?;
        println!("update smoke passed (failed startup: {fails_startup})");
    }
    if target.os == Os::Linux && !is_root()? {
        native_user_install(&artifacts.join(format!("{}.tar.gz", target.artifact_base())))?;
        println!("Linux archive install smoke passed");
        launcher_preserves_arguments_config_and_update_target()?;
        println!("Linux launcher smoke passed");
    }
    Ok(())
}

fn is_root() -> Result<bool> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|error| error.to_string())?;
    Ok(String::from_utf8_lossy(&output.stdout).trim() == "0")
}

fn hash_tree(root: &Path) -> Result<BTreeMap<PathBuf, String>> {
    let mut hashes = BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).map_err(|error| error.to_string())? {
            let path = entry.map_err(|error| error.to_string())?.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                hashes.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    sha256_file(&path)?,
                );
            }
        }
    }
    Ok(hashes)
}

fn update_scenario(target: Target, package: &Path, binary: &Path, fails_startup: bool) -> Result {
    let version_output = Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|error| error.to_string())?;
    let reported = String::from_utf8_lossy(&version_output.stdout);
    let version = reported
        .trim()
        .strip_prefix("EMP ")
        .ok_or("packaged binary reported an unexpected version")?
        .to_owned();
    let temporary = temporary_directory("emp-update-smoke-")?;
    let root = physical(temporary.path())?;
    let result = update_in(target, package, binary, fails_startup, &version, &root);
    stop_processes_under(&root);
    result
}

fn update_in(
    target: Target,
    package: &Path,
    binary: &Path,
    fails_startup: bool,
    version: &str,
    root: &Path,
) -> Result {
    let error = |error: std::io::Error| error.to_string();
    let job = root.join(".emp-update-smoke");
    fs::create_dir(&job).map_err(error)?;
    let package_name = target.update_package();
    let relative = if target.os == Os::Macos {
        "Contents/Resources/EMP"
    } else {
        ""
    };
    let installed_target = root.join(match target.os {
        Os::Macos => "EMP.app",
        Os::Windows => "EMP.exe",
        Os::Linux => "EMP",
    });
    let installed = if relative.is_empty() {
        installed_target.clone()
    } else {
        installed_target.join(relative)
    };
    if relative.is_empty() {
        copy(binary, &installed)?;
    } else {
        // Install from the real DMG, including its launcher, Info.plist and icon.
        let initial = root.join("initial-package");
        fs::create_dir(&initial).map_err(error)?;
        let initial_package = initial.join(&package_name);
        copy(package, &initial_package)?;
        let app = extract_candidate(&initial_package, &initial, relative)
            .map_err(|error| error.to_string())?;
        fs::rename(app, &installed_target).map_err(error)?;
        fs::remove_dir_all(&initial).map_err(error)?;
    }
    let bundle = if relative.is_empty() {
        BTreeMap::new()
    } else {
        hash_tree(&installed_target)?
    };
    let original = sha256_file(&installed)?;

    let copied_package = job.join(&package_name);
    copy(package, &copied_package)?;
    let candidate =
        extract_candidate(&copied_package, &job, relative).map_err(|error| error.to_string())?;
    let candidate_binary = if relative.is_empty() {
        candidate.clone()
    } else {
        candidate.join(relative)
    };
    if fails_startup {
        fs::write(&candidate_binary, b"deliberately invalid executable").map_err(error)?;
    }
    let helper = job.join(format!("worker{}", target.executable_suffix()));
    copy(binary, &helper)?;
    let port = reserve_loopback_port()?;
    let codex_home = root.join("codex-home");
    fs::create_dir(&codex_home).map_err(error)?;
    let config = root.join("config.json");
    fs::write(
        &config,
        serde_json::json!({"host": "127.0.0.1", "port": port}).to_string(),
    )
    .map_err(error)?;
    let mut parent = parent::Parent::start(&installed, root, &config, port)?;
    let plan = serde_json::json!({
        "target": installed_target,
        "candidate": candidate,
        "relative_binary": relative,
        "parents": [parent.identity],
        "args": ["serve", "--config", config, "--port", port.to_string()],
        "version": version,
        "nonce": "packaged-smoke",
    });
    let plan_path = job.join("plan.json");
    fs::write(&plan_path, plan.to_string()).map_err(error)?;

    let output_path = root.join("worker-output.txt");
    let output = fs::File::create(&output_path).map_err(error)?;
    let mut command = Command::new(&helper);
    command
        .arg("--emp-apply-update")
        .arg(&plan_path)
        .current_dir(root)
        .env("CODEX_HOME", &codex_home)
        .env("EASY_MULTI_PROVIDER_CONFIG", &config)
        .env("EASY_MULTI_PROVIDER_MASTER_KEY", "")
        .env(
            "EASY_MULTI_PROVIDER_MASTER_KEY_FILE",
            root.join("master.key"),
        )
        .env_remove("EMP_UPDATE_READY")
        .env_remove("EMP_UPDATE_RESULT")
        .stdin(Stdio::null())
        .stdout(output.try_clone().map_err(error)?)
        .stderr(output);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, 0x0800_0000); // CREATE_NO_WINDOW
    let mut worker = KillOnDrop(
        command
            .spawn()
            .map_err(|error| format!("start update worker: {error}"))?,
    );
    let ready_deadline = Instant::now() + Duration::from_secs(10);
    while !job.join("worker-ready").is_file() {
        if worker.0.try_wait().map_err(error)?.is_some() || Instant::now() >= ready_deadline {
            return Err("update worker did not become ready before old EMP exits".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    parent.quit()?;
    let deadline = Instant::now() + Duration::from_secs(85);
    let status = loop {
        if let Some(status) = worker.0.try_wait().map_err(error)? {
            break status;
        }
        if Instant::now() > deadline {
            return Err("packaged update worker did not finish".to_owned());
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let expected_exit = if fails_startup { 1 } else { 0 };
    if status.code() != Some(expected_exit) {
        let phase = fs::read_to_string(job.join("worker-status.json"))
            .unwrap_or_else(|_| "missing".to_owned());
        let log = fs::read_to_string(&output_path).unwrap_or_default();
        let tail = &log[log.len().saturating_sub(2048)..];
        return Err(format!(
            "packaged update worker failed ({status}): status={phase}; output={tail}"
        ));
    }

    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if let Ok((200, body)) = http(port, "GET", "/healthz", &[])
            && serde_json::from_slice::<serde_json::Value>(&body).ok()
                == Some(serde_json::json!({"status": "ok"}))
        {
            break;
        }
        if Instant::now() > deadline {
            return Err("replacement/rollback service did not become healthy".to_owned());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if sha256_file(&installed)? != original {
        return Err("installed binary differs from the packaged binary".to_owned());
    }
    if !relative.is_empty() {
        if hash_tree(&installed_target)? != bundle {
            return Err("installed app bundle differs from the packaged bundle".to_owned());
        }
        for launcher in ["Contents/MacOS/EMP", "Contents/Resources/launch.command"] {
            if !is_executable(&installed_target.join(launcher)) {
                return Err(format!("{launcher} is not executable after the update"));
            }
        }
    }
    if fails_startup {
        if !job.join("rolled-back").is_file() {
            return Err("failed startup did not record a rollback".to_owned());
        }
    } else {
        let deadline = Instant::now() + Duration::from_secs(15);
        while job.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
        }
        if job.exists() {
            return Err("successful update staging should be cleaned".to_owned());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Stops every process whose executable lives under `root`, which only
/// contains synthetic installations created by this smoke test.
fn stop_processes_under(root: &Path) {
    #[cfg(target_os = "linux")]
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if fs::read_link(entry.path().join("exe")).is_ok_and(|exe| exe.starts_with(root)) {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status();
            }
        }
    }
    #[cfg(target_os = "macos")]
    if let Ok(output) = Command::new("ps").args(["-axo", "pid=,comm="]).output() {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Some((pid, executable)) = line.trim().split_once(' ') else {
                continue;
            };
            if Path::new(executable.trim()).starts_with(root) {
                let _ = Command::new("kill").args(["-KILL", pid]).status();
            }
        }
    }
    #[cfg(windows)]
    {
        let root = root.display().to_string().replace('\'', "''");
        let script = format!(
            "Get-Process | Where-Object {{ $_.Path -and $_.Path.StartsWith('{root}', [StringComparison]::OrdinalIgnoreCase) }} | Stop-Process -Force"
        );
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .status();
    }
    // Let the OS release file handles before the temporary tree is removed.
    std::thread::sleep(Duration::from_secs(1));
}

fn run_output(command: &mut Command) -> Result<std::process::Output> {
    command
        .output()
        .map_err(|error| format!("could not run {:?}: {error}", command.get_program()))
}

fn extract_archive(archive: &Path, destination: &Path) -> Result {
    let file = fs::File::open(archive).map_err(|error| error.to_string())?;
    tar::Archive::new(flate2::read::GzDecoder::new(file))
        .unpack(destination)
        .map_err(|error| format!("extract {}: {error}", archive.display()))
}

fn native_user_install(archive: &Path) -> Result {
    let temporary = temporary_directory("emp-native-user-install-")?;
    let root = temporary.path();
    let extracted = root.join("extracted");
    extract_archive(archive, &extracted)?;
    let bundle = extracted.join(PRODUCT_NAME);
    let bundled_binary = bundle.join(PRODUCT_NAME);
    let version = run_output(Command::new(&bundled_binary).arg("--version"))?.stdout;

    let home = root.join("desktop user's home $100%");
    let config_home = root.join("config");
    let data_home = root.join("data");
    let config = config_home.join("easy-multi-provider/config.json");
    fs::create_dir_all(config.parent().unwrap()).map_err(|error| error.to_string())?;
    let original_config = b"{\"retained\":true}\n";
    fs::write(&config, original_config).map_err(|error| error.to_string())?;

    let install = run_output(
        Command::new("sh")
            .arg(bundle.join("install-user.sh"))
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &config_home)
            .env("XDG_DATA_HOME", &data_home),
    )?;
    if !install.status.success() {
        return Err(format!(
            "install-user.sh failed: {}",
            String::from_utf8_lossy(&install.stderr)
        ));
    }
    let installed = data_home.join("easy-multi-provider/EMP");
    let read = |path: &Path| fs::read(path).map_err(|error| format!("{}: {error}", path.display()));
    if read(&installed)? != read(&bundled_binary)? {
        return Err("installed binary differs from the archive".to_owned());
    }
    if read(&config)? != original_config {
        return Err("installer changed the existing configuration".to_owned());
    }
    if installation_target(&installed).map_err(|error| error.to_string())?
        != (installed.clone(), String::new())
    {
        return Err("installed binary is not an updatable installation target".to_owned());
    }
    let launched = run_output(Command::new(home.join(".local/bin/EMP")).arg("--version"))?;
    if launched.stdout != version {
        return Err("installed launcher reported a different version".to_owned());
    }
    Ok(())
}

fn launcher_preserves_arguments_config_and_update_target() -> Result {
    let temporary = temporary_directory("emp-user-install-")?;
    let root = temporary.path();
    let source = root.join("archive");
    fs::create_dir(&source).map_err(|error| error.to_string())?;
    let script = fs::read_to_string(project_root().join("packaging/install-user.sh"))
        .map_err(|error| error.to_string())?;
    fs::write(source.join("install-user.sh"), script.replace("\r\n", "\n"))
        .map_err(|error| error.to_string())?;
    fs::write(source.join("EMP"), "#!/bin/sh\nprintf \"%s\\n\" \"$@\"\n")
        .map_err(|error| error.to_string())?;
    fs::write(source.join("easy-multi-provider.svg"), "<svg/>")
        .map_err(|error| error.to_string())?;
    let home = root.join("desktop user's home $100%");
    for custom_xdg in [false, true] {
        let active_home = home.join(custom_xdg.to_string());
        let mut environment = vec![("HOME", active_home.clone())];
        let (config_root, data_root) = if custom_xdg {
            (active_home.join("settings"), active_home.join("data"))
        } else {
            (
                active_home.join(".config"),
                active_home.join(".local/share"),
            )
        };
        if custom_xdg {
            environment.push(("XDG_CONFIG_HOME", config_root.clone()));
            environment.push(("XDG_DATA_HOME", data_root.clone()));
        }
        let installer = || {
            let mut command = Command::new("sh");
            command
                .arg(source.join("install-user.sh"))
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("XDG_DATA_HOME")
                .envs(environment.iter().map(|(name, value)| (*name, value)));
            command
        };
        let config = config_root.join("easy-multi-provider/config.json");
        fs::create_dir_all(config.parent().unwrap()).map_err(|error| error.to_string())?;
        fs::write(&config, b"existing configuration").map_err(|error| error.to_string())?;
        let first = run_output(&mut installer())?;
        if !first.status.success() {
            return Err(format!(
                "install-user.sh failed: {}",
                String::from_utf8_lossy(&first.stderr)
            ));
        }
        let launched = run_output(Command::new(active_home.join(".local/bin/EMP")).args([
            "--version",
            "argument with spaces",
            "$(false)",
        ]))?;
        if String::from_utf8_lossy(&launched.stdout)
            .lines()
            .collect::<Vec<_>>()
            != ["--version", "argument with spaces", "$(false)"]
        {
            return Err("installed launcher did not preserve its arguments".to_owned());
        }
        let binary = data_root.join("easy-multi-provider/EMP");
        if installation_target(&binary).map_err(|error| error.to_string())?
            != (binary.clone(), String::new())
        {
            return Err("installed binary is not an updatable installation target".to_owned());
        }
        let job = root.join(format!("update-{custom_xdg}"));
        fs::create_dir(&job).map_err(|error| error.to_string())?;
        let archive = job.join("EMP-linux-x86_64.tar.gz");
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            fs::File::create(&archive).map_err(|error| error.to_string())?,
            flate2::Compression::default(),
        ));
        builder
            .append_path_with_name(&binary, "EMP/EMP")
            .and_then(|()| builder.into_inner())
            .and_then(flate2::write::GzEncoder::finish)
            .map_err(|error| error.to_string())?;
        let candidate = extract_candidate(&archive, &job, "").map_err(|error| error.to_string())?;
        if fs::read(&candidate).ok() != fs::read(&binary).ok() {
            return Err("update candidate differs from the installed binary".to_owned());
        }
        let second = run_output(&mut installer())?;
        if second.status.success() {
            return Err("reinstalling over an existing installation should fail".to_owned());
        }
        if fs::read(&config).ok().as_deref() != Some(b"existing configuration".as_slice()) {
            return Err("installer changed the existing configuration".to_owned());
        }
    }
    Ok(())
}
