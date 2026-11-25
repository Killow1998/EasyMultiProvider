use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
        Threading::{CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
    },
};

pub(super) struct Job(OwnedHandle);

impl Job {
    pub(super) fn terminate(&self) -> io::Result<()> {
        // SAFETY: self owns a live job handle with termination access.
        if unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

fn own(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: callers transfer newly created handles; the check above excludes invalid values.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

pub(super) fn spawn(mut command: Command, flags: u32) -> io::Result<(Child, Job)> {
    // SAFETY: null attributes and name request an unnamed job with default security.
    let job = Job(own(unsafe {
        CreateJobObjectW(std::ptr::null(), std::ptr::null())
    })?);
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: job is live and limits matches the selected information class and byte size.
    if unsafe {
        SetInformationJobObject(
            job.0.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut child = command.creation_flags(CREATE_SUSPENDED | flags).spawn()?;
    // SAFETY: both handles remain owned and live through assignment.
    let assigned =
        unsafe { AssignProcessToJobObject(job.0.as_raw_handle(), child.as_raw_handle()) };
    let result = if assigned == 0 {
        Err(io::Error::last_os_error())
    } else {
        resume_primary_thread(child.id())
    };
    if let Err(error) = result {
        let _ = job.terminate();
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok((child, job))
}

fn resume_primary_thread(pid: u32) -> io::Result<()> {
    // SAFETY: thread snapshots ignore the process ID argument and return a new owned handle.
    let snapshot = own(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) })?;
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    // SAFETY: snapshot is live and entry is writable with dwSize set to its structure size.
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
    while found != 0 {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: the ID comes from the snapshot; zero disables handle inheritance.
            let thread = own(unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) })?;
            // SAFETY: thread is a live handle opened with THREAD_SUSPEND_RESUME access.
            return if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            };
        }
        // SAFETY: snapshot remains live and entry remains writable with the required dwSize.
        found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "suspended child thread",
    ))
}
