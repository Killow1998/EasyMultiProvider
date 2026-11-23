use crate::{Result, project_root, temporary_directory};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MODULES: [&str; 4] = ["decision", "gateway", "aggregate", "team"];
const USAGE: &str = "usage: cargo xtask module-e2e [--skip-aero] [--features <emp-app features>]";

pub fn run(args: &[String]) -> Result {
    let mut skip_aero = false;
    let mut features = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match (arg.as_str(), features.is_none()) {
            ("--skip-aero", _) if !skip_aero => skip_aero = true,
            ("--features", true) => features = Some(rest.next().ok_or(USAGE)?.clone()),
            _ => return Err(USAGE.into()),
        }
    }
    if !cfg!(unix) {
        return Err("module-e2e requires Unix process inspection; CI runs it on Linux".into());
    }
    if skip_aero {
        println!("module-e2e: 显式跳过 M04 Aero (--skip-aero)");
    } else if std::env::var_os("EMP_AERO_PYTHON").is_none_or(|value| value.is_empty()) {
        return Err("设置 EMP_AERO_PYTHON 指向装有 PyInstaller 的 Python".into());
    }
    let root = project_root();
    let temporary = temporary_directory("emp-module-e2e-")?;
    let next_root = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| root.join("target"), PathBuf::from)
        .join("module-e2e");
    let source = next_root.join("source");
    let next_target = next_root.join("target");
    build(&root, None, true)?;
    if source.exists() {
        execute(
            Command::new("git")
                .current_dir(&root)
                .args(["worktree", "remove", "--force"])
                .arg(&source),
        )?;
    }
    execute(
        Command::new("git")
            .current_dir(&root)
            .args(["worktree", "add", "--detach"])
            .arg(&source)
            .arg("HEAD"),
    )?;
    let next = (|| {
        bump_versions(&source)?;
        build(&source, Some(&next_target), false)
    })();
    let removed = execute(
        Command::new("git")
            .current_dir(&root)
            .args(["worktree", "remove", "--force"])
            .arg(&source),
    );
    let next = next?;
    removed?;
    let mut test = cargo(&root);
    test.args(["test", "--locked", "-p", "emp-app", "--test", "e2e"])
        .env("EMP_E2E_MODULE_NEXT_DIR", &next)
        .env_remove("EMP_E2E_AERO_PACKAGE");
    if let Some(features) = &features {
        test.args(["--features", features]);
    }
    if !skip_aero {
        let output = temporary.path().join("aero");
        execute(
            cargo(&root)
                .args(["xtask", "package-module", "aero", "--output"])
                .arg(&output),
        )?;
        test.env(
            "EMP_E2E_AERO_PACKAGE",
            output.join(format!(
                "EMP-mod-aero-{}-{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            )),
        );
    }
    test.args([
        "--",
        "--ignored",
        "--test-threads=1",
        "--nocapture",
        "e2e::modules::",
        "e2e::team::",
    ]);
    if !skip_aero {
        test.arg("e2e::aero::");
    }
    execute(&mut test)
}

fn cargo(root: &Path) -> Command {
    let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command
        .current_dir(root)
        .env_remove("EMP_LEGACY_CORE_VERSION");
    command
}

fn execute(command: &mut Command) -> Result {
    eprintln!("module-e2e: {command:?}");
    let status = command
        .status()
        .map_err(|error| format!("{command:?}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{command:?} failed with {status}"))
    }
}

fn build(root: &Path, target: Option<&Path>, app: bool) -> Result<PathBuf> {
    let mut command = cargo(root);
    command.args([
        "build",
        "--locked",
        "--bins",
        "--message-format=json-render-diagnostics",
    ]);
    if app {
        command.args(["-p", "emp-app"]);
    }
    for id in MODULES {
        command.arg("-p").arg(format!("emp-mod-{id}"));
    }
    if let Some(target) = target {
        command.env("CARGO_TARGET_DIR", target);
    }
    eprintln!("module-e2e: {command:?}");
    let output = command
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| format!("{command:?}: {error}"))?;
    let mut directory = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let message: Value = serde_json::from_str(line)
            .map_err(|error| format!("{command:?}: invalid Cargo message: {error}: {line}"))?;
        if let Some(rendered) = message["message"]["rendered"].as_str() {
            eprint!("{rendered}");
        }
        if message["reason"] == "compiler-artifact"
            && message["target"]["name"] == "empmod-decision"
        {
            directory = message["executable"]
                .as_str()
                .and_then(|path| Path::new(path).parent())
                .map(Path::to_owned);
        }
    }
    if !output.status.success() {
        return Err(format!("{command:?} failed with {}", output.status));
    }
    directory.ok_or_else(|| format!("{command:?}: no empmod-decision executable in Cargo output"))
}

fn bump_versions(root: &Path) -> Result {
    let lock_path = root.join("Cargo.lock");
    let mut lock = fs::read_to_string(&lock_path).map_err(|error| error.to_string())?;
    for id in MODULES {
        let name = format!("emp-mod-{id}");
        let path = root.join(format!("crates/{name}/Cargo.toml"));
        let manifest = fs::read_to_string(&path).map_err(|error| error.to_string())?;
        let old = manifest
            .lines()
            .find_map(|line| line.strip_prefix("version = \"")?.strip_suffix('"'))
            .ok_or_else(|| format!("{}: missing package version", path.display()))?;
        let boundary = old.find(['-', '+']).unwrap_or(old.len());
        let numbers = old[..boundary].split('.').collect::<Vec<_>>();
        let [major, minor, patch] = numbers.as_slice() else {
            return Err(format!("{}: invalid semver {old}", path.display()));
        };
        let patch = patch.parse::<u64>().map_err(|error| error.to_string())? + 1;
        let next = format!("{major}.{minor}.{patch}{}", &old[boundary..]);
        fs::write(
            &path,
            manifest.replacen(
                &format!("version = \"{old}\""),
                &format!("version = \"{next}\""),
                1,
            ),
        )
        .map_err(|error| error.to_string())?;
        let locked = format!("name = \"{name}\"\nversion = \"{old}\"");
        if !lock.contains(&locked) {
            return Err(format!("{}: missing {name} {old}", lock_path.display()));
        }
        lock = lock.replacen(
            &locked,
            &format!("name = \"{name}\"\nversion = \"{next}\""),
            1,
        );
        println!("module-e2e: {name} {old} -> {next} (temporary HEAD worktree)");
    }
    fs::write(lock_path, lock).map_err(|error| error.to_string())
}
