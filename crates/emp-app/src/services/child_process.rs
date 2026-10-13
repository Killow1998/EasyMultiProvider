//! Ownership and teardown of one CLI process group or Windows job.
use std::process::Child;
use std::thread;
use std::time::Duration;

pub(crate) struct ChildTree {
    pub(crate) child: Child,
    stopped: bool,
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

impl ChildTree {
    #[allow(unused_mut)]
    pub(crate) fn new(mut child: Child) -> Result<Self, &'static str> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW,
            };
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() || unsafe { AssignProcessToJobObject(job, child.as_raw_handle()) } == 0
            {
                if !job.is_null() {
                    unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err("cli_process_tree_unavailable");
            }
            Ok(Self {
                child,
                stopped: false,
                job,
            })
        }
        #[cfg(not(windows))]
        {
            Ok(Self {
                child,
                stopped: false,
            })
        }
    }

    pub(crate) fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGTERM);
        }
        #[cfg(windows)]
        if !self.job.is_null() {
            unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1) };
        }
        #[cfg(unix)]
        {
            thread::sleep(Duration::from_millis(100));
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
    }

    pub(crate) fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait()
    }
}

impl Drop for ChildTree {
    fn drop(&mut self) {
        if !self.stopped {
            self.stop();
            let _ = self.child.wait();
        }
        #[cfg(windows)]
        if !self.job.is_null() {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.job) };
        }
    }
}
