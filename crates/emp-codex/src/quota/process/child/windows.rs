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
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
        },
    },
};

pub(super) struct Job(OwnedHandle);

impl Job {
    pub(super) fn stop(&self) {
        unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) };
    }
}

fn own(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // Each caller transfers a newly created handle to exactly one owner.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

pub(super) fn spawn(command: &mut Command) -> io::Result<(Child, Job)> {
    let job = Job(own(unsafe {
        CreateJobObjectW(std::ptr::null(), std::ptr::null())
    })?);
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
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
    // Suspend before assigning the job: npm launchers must not be able to
    // spawn unowned children between CreateProcess and assignment.
    let mut child = command
        .creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW)
        .spawn()?;
    let assigned =
        unsafe { AssignProcessToJobObject(job.0.as_raw_handle(), child.as_raw_handle()) };
    let result = if assigned == 0 {
        Err(io::Error::last_os_error())
    } else {
        resume_primary_thread(child.id())
    };
    if let Err(error) = result {
        job.stop();
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok((child, job))
}

fn resume_primary_thread(pid: u32) -> io::Result<()> {
    // std::process retains the process handle, not the initial thread handle.
    // A suspended fresh process has only its initial thread to resume.
    let snapshot = own(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) })?;
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
    while found != 0 {
        if entry.th32OwnerProcessID == pid {
            let thread = own(unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) })?;
            return if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            };
        }
        found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "quota helper thread",
    ))
}
