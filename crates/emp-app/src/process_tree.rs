use std::{
    io,
    process::{Child, Command, ExitStatus},
    sync::Mutex,
};

#[cfg(unix)]
pub(crate) mod pipes;

#[cfg(unix)]
use rustix::{
    io::Errno,
    process::{Pid, Signal, WaitId, WaitIdOptions, kill_process, kill_process_group, waitid},
};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::INVALID_HANDLE_VALUE,
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        },
        JobObjects::{AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject},
        Threading::{CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
    },
};

pub(crate) struct ProcessTree {
    stopped: Mutex<bool>,
    #[cfg(unix)]
    leader: Pid,
    #[cfg(windows)]
    job: OwnedHandle,
}

impl ProcessTree {
    pub(crate) fn configure(command: &mut Command, #[cfg(windows)] flags: u32) {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(flags | CREATE_SUSPENDED);
        }
    }

    pub(crate) fn attach(child: &Child) -> io::Result<Self> {
        #[cfg(windows)]
        let job = {
            // SAFETY: Null security attributes and name are valid for an unnamed job.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: CreateJobObjectW returned a newly owned job handle.
            let job = unsafe { OwnedHandle::from_raw_handle(handle) };
            let child_handle = child.as_raw_handle();
            // SAFETY: Both handles are live; the child is suspended until assignment succeeds.
            if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child_handle) } == 0 {
                return Err(io::Error::last_os_error());
            }
            resume_primary_thread(child.id())?;
            job
        };
        Ok(Self {
            stopped: Mutex::new(false),
            #[cfg(unix)]
            leader: Pid::from_child(child),
            #[cfg(windows)]
            job,
        })
    }

    pub(crate) fn request_graceful_stop(&self) {
        #[cfg(unix)]
        {
            {
                let stopped = self.stopped.lock().unwrap();
                if *stopped {
                    return;
                }
                let _ = kill_process_group(self.leader, Signal::TERM);
            }
            #[expect(
                clippy::disallowed_methods,
                reason = "向进程组发送 SIGTERM 后保留既有宽限期，再发送 SIGKILL"
            )]
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        self.request_stop();
    }

    pub(crate) fn request_stop(&self) {
        #[cfg(unix)]
        {
            let stopped = self.stopped.lock().unwrap();
            if !*stopped {
                let _ = kill_process(self.leader, Signal::KILL);
            }
        }
        #[cfg(windows)]
        self.terminate();
    }

    pub(crate) fn has_exited(&self, child: &mut Child) -> io::Result<bool> {
        #[cfg(unix)]
        return self
            .observe_exit(WaitIdOptions::NOHANG)
            .map_err(|error| self.handle_observation_error(child, error));
        #[cfg(windows)]
        return child.try_wait().map(|status| status.is_some());
    }

    #[cfg(unix)]
    fn observe_exit(&self, options: WaitIdOptions) -> rustix::io::Result<bool> {
        let options = options | WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
        loop {
            match waitid(WaitId::Pid(self.leader), options) {
                Ok(status) => return Ok(status.is_some()),
                Err(Errno::INTR) => continue,
                Err(error) => return Err(error),
            }
        }
    }

    pub(crate) fn wait(&self, child: &mut Child) -> io::Result<ExitStatus> {
        #[cfg(unix)]
        {
            self.observe_exit(WaitIdOptions::empty())
                .map_err(|error| self.handle_observation_error(child, error))?;
            self.terminate_and_reap(child)
        }
        #[cfg(windows)]
        {
            let status = child.wait();
            self.terminate();
            status
        }
    }

    #[cfg(unix)]
    pub(crate) fn handle_observation_error(&self, child: &mut Child, error: Errno) -> io::Error {
        if error == Errno::CHILD {
            *self.stopped.lock().unwrap() = true;
        } else {
            let _ = self.terminate_and_reap(child);
        }
        error.into()
    }

    #[cfg(unix)]
    fn terminate_and_reap(&self, child: &mut Child) -> io::Result<ExitStatus> {
        self.terminate();
        child.wait()
    }

    pub(crate) fn is_stopped(&self) -> bool {
        *self.stopped.lock().unwrap()
    }

    fn terminate(&self) {
        let mut stopped = self.stopped.lock().unwrap();
        if *stopped {
            return;
        }
        *stopped = true;
        #[cfg(unix)]
        {
            let _ = kill_process_group(self.leader, Signal::KILL);
        }
        #[cfg(windows)]
        // SAFETY: self owns the live job handle through termination.
        unsafe {
            TerminateJobObject(self.job.as_raw_handle(), 1);
        }
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(windows)]
fn resume_primary_thread(pid: u32) -> io::Result<()> {
    // SAFETY: A thread snapshot ignores the PID argument and returns an owned handle.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateToolhelp32Snapshot returned a valid, newly owned handle.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    // SAFETY: The snapshot is live and entry is writable with dwSize set.
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
    while found != 0 {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: The ID comes from the snapshot and zero disables handle inheritance.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: OpenThread returned a valid, newly owned handle.
            let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
            // SAFETY: The live thread handle has THREAD_SUSPEND_RESUME access.
            return if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            };
        }
        // SAFETY: The snapshot remains live and entry remains writable.
        found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "suspended child thread",
    ))
}
