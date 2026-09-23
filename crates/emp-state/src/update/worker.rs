//! Detached replacement worker. Failed startup restores the exact previous installation.
use super::{Result, UpdateError, created, spawn};
use serde::Deserialize;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
#[derive(Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub created: f64,
}
#[derive(Deserialize)]
pub struct Plan {
    pub target: PathBuf,
    pub candidate: PathBuf,
    pub relative_binary: String,
    pub parents: Vec<ProcessIdentity>,
    pub args: Vec<String>,
    pub version: String,
    pub nonce: String,
}
fn load(path: &Path) -> Result<Plan> {
    if path.is_symlink() || fs::metadata(path)?.len() > 1024 * 1024 {
        return Err(UpdateError("invalid_update_plan"));
    }
    serde_json::from_slice(&fs::read(path)?).map_err(|_| UpdateError("invalid_update_plan"))
}
pub fn installation_target(executable: &Path) -> Result<(PathBuf, String)> {
    if executable.is_symlink() {
        return Err(UpdateError("unsupported_installation"));
    }
    let executable = std::path::absolute(executable)?;
    #[cfg(target_os = "macos")]
    {
        let app = executable
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .ok_or(UpdateError("unsupported_installation"))?;
        if app.extension().is_none_or(|extension| extension != "app")
            || executable.strip_prefix(app).ok() != Some(Path::new("Contents/Resources/EMP"))
        {
            return Err(UpdateError("unsupported_installation"));
        }
        return Ok((app.to_owned(), "Contents/Resources/EMP".into()));
    }
    #[cfg(not(target_os = "macos"))]
    Ok((executable, String::new()))
}
fn write(path: &Path, value: &serde_json::Value) -> Result<()> {
    crate::filesystem::atomic_write_private_state(
        path,
        &serde_json::to_vec(value).map_err(|_| UpdateError("invalid_update_plan"))?,
    )
    .map_err(|_| UpdateError("update_failed"))
}
fn phase(job: &Path, name: &str) {
    let _ = write(&job.join("worker-status.json"), &json!({"phase":name}));
}
fn touch(path: &Path) -> Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    Ok(())
}
pub fn run(plan_path: &Path) -> Result<u8> {
    let plan_path = plan_path.canonicalize()?;
    let job = plan_path
        .parent()
        .ok_or(UpdateError("invalid_update_plan"))?;
    let plan = load(&plan_path)?;
    let target_parent = plan
        .target
        .parent()
        .and_then(|path| path.canonicalize().ok());
    let candidate_parent = plan
        .candidate
        .parent()
        .and_then(|path| path.canonicalize().ok());
    if !job
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with(".emp-update-"))
        || target_parent.as_deref() != job.parent()
        || candidate_parent.as_deref() != Some(job)
        || plan.target.is_symlink()
        || plan.candidate.is_symlink()
        || !matches!(plan.relative_binary.as_str(), "" | "Contents/Resources/EMP")
    {
        return Err(UpdateError("invalid_update_plan"));
    }
    phase(job, "waiting_for_exit");
    touch(&job.join("worker-ready"))?;
    let deadline = Instant::now() + Duration::from_secs(90);
    while plan
        .parents
        .iter()
        .any(|parent| created(parent.pid) == Some(parent.created))
    {
        if Instant::now() > deadline {
            return Err(UpdateError("shutdown_timeout"));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let backup = job.join("previous");
    let binary = if plan.relative_binary.is_empty() {
        plan.target.clone()
    } else {
        plan.target.join(&plan.relative_binary)
    };
    let mut child = None;
    let install = (|| -> Result<()> {
        phase(job, "replacing");
        fs::rename(&plan.target, &backup)?;
        fs::rename(&plan.candidate, &plan.target)?;
        phase(job, "starting");
        child = Some(spawn(
            &binary,
            &plan.args,
            &[(
                "EMP_UPDATE_READY",
                job.join("ready.json").to_string_lossy().into_owned(),
            )],
            true,
        )?);
        phase(job, "waiting_for_startup");
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if let Ok(raw) = fs::read(job.join("ready.json"))
                && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&raw)
                && value == json!({"version":plan.version,"nonce":plan.nonce})
            {
                phase(job, "complete");
                touch(&job.join("success"))?;
                return Ok(());
            }
            if child.as_mut().unwrap().child.try_wait()?.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err(UpdateError("startup_failed"))
    })();
    if install.is_ok() {
        return Ok(0);
    }
    phase(job, "restoring");
    if let Some(child) = &mut child {
        child.stop();
    }
    let restore = (|| -> Result<()> {
        if backup.exists() {
            if plan.target.exists() {
                fs::rename(&plan.target, job.join("failed"))?;
            }
            fs::rename(&backup, &plan.target)?;
        }
        phase(job, "restarting_previous");
        spawn(
            &binary,
            &plan.args,
            &[("EMP_UPDATE_RESULT", "rolled_back".into())],
            true,
        )?;
        phase(job, "rolled_back");
        touch(&job.join("rolled-back"))?;
        Ok(())
    })();
    if restore.is_err() {
        phase(job, "recovery_required");
        touch(&job.join("recovery-required"))?;
    }
    Ok(1)
}
pub fn mark_ready(version: &str) -> Result<()> {
    let Some(marker) = std::env::var_os("EMP_UPDATE_READY").filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    let ready = PathBuf::from(marker);
    let job = ready
        .parent()
        .ok_or(UpdateError("invalid_update_plan"))?
        .canonicalize()?;
    let plan = load(&job.join("plan.json"))?;
    let (target, _) = installation_target(&std::env::current_exe()?)?;
    if ready.file_name().is_none_or(|name| name != "ready.json")
        || plan.target.canonicalize()? != target.canonicalize()?
        || job.parent() != target.parent()
        || !job
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(".emp-update-"))
    {
        return Err(UpdateError("invalid_update_plan"));
    }
    write(
        &job.join("ready.json"),
        &json!({"version":version,"nonce":plan.nonce}),
    )?;
    std::thread::Builder::new()
        .name("emp-update-cleanup".into())
        .spawn(move || {
            for _ in 0..30 {
                std::thread::sleep(Duration::from_secs(2));
                if job.join("success").is_file() && fs::remove_dir_all(&job).is_ok() {
                    break;
                }
            }
        })?;
    Ok(())
}
