use std::{io, process::Command};

#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
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
    stopped: bool,
    #[cfg(unix)]
    group: libc::pid_t,
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

    pub(crate) fn attach(pid: u32, #[cfg(windows)] child: RawHandle) -> io::Result<Self> {
        #[cfg(windows)]
        let job = {
            // SAFETY: Null security attributes and name are valid for an unnamed job.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: CreateJobObjectW returned a newly owned job handle.
            let job = unsafe { OwnedHandle::from_raw_handle(handle) };
            // SAFETY: Both handles are live; the child is suspended until assignment succeeds.
            if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child) } == 0 {
                return Err(io::Error::last_os_error());
            }
            resume_primary_thread(pid)?;
            job
        };
        Ok(Self {
            stopped: false,
            #[cfg(unix)]
            group: pid as libc::pid_t,
            #[cfg(windows)]
            job,
        })
    }

    pub(crate) fn terminate_gracefully(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: process_group(0) gives the child a group with its PID.
            unsafe { libc::killpg(self.group, libc::SIGTERM) };
            #[expect(
                clippy::disallowed_methods,
                reason = "向进程组发送 SIGTERM 后保留既有宽限期，再发送 SIGKILL"
            )]
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        self.terminate();
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped
    }

    pub(crate) fn terminate(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        #[cfg(unix)]
        // SAFETY: process_group(0) gives the child a group with its PID.
        unsafe {
            libc::killpg(self.group, libc::SIGKILL);
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
