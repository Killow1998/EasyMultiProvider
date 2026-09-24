//! Process birth identity and update-owned child process groups.
use super::{Result, UpdateError};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn created(pid: u32) -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let suffix = stat.rsplit_once(')')?.1;
        let ticks = suffix.split_whitespace().nth(19)?.parse::<f64>().ok()?;
        let boot = std::fs::read_to_string("/proc/stat")
            .ok()?
            .lines()
            .find_map(|line| {
                line.strip_prefix("btime ")
                    .and_then(|value| value.parse::<f64>().ok())
            })?;
        // sysconf has no side effects; the kernel defines the process clock tick.
        let frequency = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        (frequency > 0).then_some(boot + ticks / frequency as f64)
    }
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>();
        let read = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size as i32,
            )
        };
        if read != size as i32 {
            return None;
        }
        let info = unsafe { info.assume_init() };
        Some(info.pbi_start_tvsec as f64 + info.pbi_start_tvusec as f64 / 1e6)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, FILETIME},
            System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
        };
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return None;
        }
        let mut times = [FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        }; 4];
        let result = unsafe {
            GetProcessTimes(
                process,
                &mut times[0],
                &mut times[1],
                &mut times[2],
                &mut times[3],
            )
        };
        unsafe { CloseHandle(process) };
        if result == 0 {
            return None;
        }
        let ticks = ((times[0].dwHighDateTime as u64) << 32) | times[0].dwLowDateTime as u64;
        Some(ticks as f64 / 1e7 - 11644473600.0)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        None
    }
}
pub struct OwnedChild {
    pub child: Child,
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}
#[cfg(windows)]
struct QuietLaunch {
    previous: u32,
}
#[cfg(windows)]
impl QuietLaunch {
    fn begin() -> Result<Self> {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            GetThreadErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX,
            SEM_NOOPENFILEERRORBOX, SetThreadErrorMode,
        };
        let mut previous = 0;
        let quiet = SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX;
        if unsafe { SetThreadErrorMode(GetThreadErrorMode() | quiet, &mut previous) } == 0 {
            return Err(UpdateError("worker_failed"));
        }
        Ok(Self { previous })
    }
}
#[cfg(windows)]
impl Drop for QuietLaunch {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Diagnostics::Debug::SetThreadErrorMode(
                self.previous,
                std::ptr::null_mut(),
            );
        }
    }
}
#[cfg(windows)]
pub(super) fn spawn_quiet(command: &mut Command) -> Result<Child> {
    // CreateProcess can show a bad-image dialog before returning for a broken update.
    // This guard only changes the calling thread and restores its mode after spawn.
    let _quiet_launch = QuietLaunch::begin()?;
    Ok(command.spawn()?)
}
pub fn spawn(
    executable: &Path,
    args: &[String],
    environment: &[(&str, String)],
    visible: bool,
) -> Result<OwnedChild> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .env("PYINSTALLER_RESET_ENVIRONMENT", "1")
        .env_remove("EMP_UPDATE_READY")
        .env_remove("EMP_UPDATE_RESULT");
    for (key, value) in environment {
        match (*key, value.as_str()) {
            ("EMP_UPDATE_READY", value) if !value.is_empty() => {
                command.env("EMP_UPDATE_READY", value);
            }
            ("EMP_UPDATE_RESULT", "rolled_back") => {
                command.env("EMP_UPDATE_RESULT", "rolled_back");
            }
            _ => return Err(UpdateError("worker_failed")),
        }
    }
    if !cfg!(windows) || !visible {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x00000200 | if visible { 0x00000010 } else { 0x08000000 });
    }
    #[cfg(windows)]
    let child = spawn_quiet(&mut command)?;
    #[cfg(not(windows))]
    let child = command.spawn()?;
    #[cfg(windows)]
    {
        let mut child = child;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW},
        };
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(UpdateError("worker_failed"));
        }
        if unsafe { AssignProcessToJobObject(job, child.as_raw_handle()) } == 0 {
            unsafe { CloseHandle(job) };
            let _ = child.kill();
            let _ = child.wait();
            return Err(UpdateError("worker_failed"));
        }
        Ok(OwnedChild { child, job })
    }
    #[cfg(not(windows))]
    {
        Ok(OwnedChild { child })
    }
}
impl OwnedChild {
    pub fn stop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGTERM);
        }
        #[cfg(windows)]
        if !self.job.is_null() {
            unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1) };
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        if self.child.try_wait().ok().flatten().is_none() {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
    pub fn wait_for_exit(&mut self, timeout: Duration) -> Result<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                self.stop();
                return Err(UpdateError("worker_failed"));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

#[cfg(windows)]
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.job.is_null() {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.job) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::spawn;

    #[test]
    fn updater_children_reject_environment_outside_the_marker_allowlist() {
        let executable = std::env::current_exe().unwrap();
        assert!(matches!(
            spawn(&executable, &[], &[("RUST_LOG", "trace".to_owned())], false),
            Err(super::super::UpdateError("worker_failed"))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn invalid_update_executable_restores_thread_error_mode() {
        use windows_sys::Win32::System::Diagnostics::Debug::GetThreadErrorMode;

        let root = tempfile::TempDir::new().unwrap();
        let executable = root.path().join("candidate.exe");
        std::fs::write(&executable, b"deliberately invalid executable").unwrap();
        let previous = unsafe { GetThreadErrorMode() };
        assert!(spawn(&executable, &[], &[], true).is_err());
        assert_eq!(unsafe { GetThreadErrorMode() }, previous);
    }
}
