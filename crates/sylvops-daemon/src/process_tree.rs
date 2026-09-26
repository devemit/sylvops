//! Platform-specific ownership of a spawned session process tree.

use crate::Result;

#[cfg(unix)]
mod platform {
    use nix::{
        sys::signal::{Signal, killpg},
        unistd::{Pid, getpgid},
    };

    use crate::{DaemonError, Result};

    #[derive(Debug)]
    pub struct ProcessTree {
        process_group: Pid,
    }

    impl ProcessTree {
        pub fn attach(process_id: u32, reported_group: Option<i32>) -> Result<Self> {
            let raw_id = i32::try_from(process_id).map_err(|_| {
                DaemonError::ProcessTree(format!("process ID {process_id} does not fit pid_t"))
            })?;
            let root = Pid::from_raw(raw_id);
            let process_group =
                getpgid(Some(root)).map_err(|error| DaemonError::ProcessTree(error.to_string()))?;
            if process_group != root || reported_group != Some(raw_id) {
                return Err(DaemonError::ProcessTree(format!(
                    "PTY process {process_id} does not lead a dedicated process group"
                )));
            }
            Ok(Self { process_group })
        }

        pub fn terminate(&self) -> Result<()> {
            match killpg(self.process_group, Signal::SIGKILL) {
                Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
                Err(error) => Err(DaemonError::ProcessTree(error.to_string())),
            }
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    use std::{
        ffi::c_void,
        mem::size_of,
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::Child,
        ptr,
    };

    use windows_sys::Win32::{
        Foundation::{HANDLE, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject, TerminateJobObject,
            },
            Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };

    use crate::{DaemonError, Result};

    pub struct ProcessTree {
        job: OwnedHandle,
    }

    impl std::fmt::Debug for ProcessTree {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("ProcessTree")
                .finish_non_exhaustive()
        }
    }

    impl ProcessTree {
        pub fn create() -> Result<Self> {
            // SAFETY: null security attributes and name request an unnamed job with defaults.
            let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
            if job.is_null() {
                return Err(last_os_error("create Windows Job Object"));
            }
            // SAFETY: `job` is a new owned handle, checked non-null, and transferred exactly once.
            let job = unsafe { OwnedHandle::from_raw_handle(job) };

            // SAFETY: the structure is plain Windows ABI data where zero is a valid baseline.
            let mut information: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
                unsafe { std::mem::zeroed() };
            information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `job` is valid and the pointer/length describe `information` exactly.
            let configured = unsafe {
                SetInformationJobObject(
                    job.as_raw_handle().cast::<c_void>(),
                    JobObjectExtendedLimitInformation,
                    (&raw const information).cast::<c_void>(),
                    u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                        .expect("Windows job structure fits u32"),
                )
            };
            if configured == 0 {
                return Err(last_os_error("configure Windows Job Object"));
            }

            Ok(Self { job })
        }

        pub fn raw_handle(&self) -> HANDLE {
            self.job.as_raw_handle().cast::<c_void>()
        }

        pub fn assign(&self, child: &Child) -> Result<()> {
            // SAFETY: both handles are live and owned by `self` and `child` for this call.
            if unsafe {
                AssignProcessToJobObject(
                    self.job.as_raw_handle().cast::<c_void>(),
                    child.as_raw_handle().cast::<c_void>(),
                )
            } == 0
            {
                return Err(last_os_error("assign native command to Windows Job Object"));
            }
            Ok(())
        }

        pub fn resume(process_id: u32) -> Result<()> {
            // SAFETY: the snapshot is an owned kernel handle, checked before ownership transfer.
            let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
            if snapshot == INVALID_HANDLE_VALUE {
                return Err(last_os_error("enumerate suspended Windows process threads"));
            }
            // SAFETY: `snapshot` is valid and transferred exactly once.
            let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
            let mut entry = THREADENTRY32 {
                dwSize: u32::try_from(size_of::<THREADENTRY32>())
                    .expect("Windows thread entry fits u32"),
                ..Default::default()
            };
            let mut found = false;
            // SAFETY: the snapshot and correctly sized output structure are valid for this call.
            let mut available =
                unsafe { Thread32First(snapshot.as_raw_handle().cast(), &raw mut entry) };
            while available != 0 {
                if entry.th32OwnerProcessID == process_id {
                    // SAFETY: the requested access is limited to resuming the enumerated thread.
                    let thread =
                        unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                    if thread.is_null() {
                        return Err(last_os_error("open suspended Windows process thread"));
                    }
                    // SAFETY: `thread` is valid and transferred exactly once.
                    let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
                    // SAFETY: the handle grants THREAD_SUSPEND_RESUME for a suspended thread.
                    if unsafe { ResumeThread(thread.as_raw_handle().cast()) } == u32::MAX {
                        return Err(last_os_error("resume Windows process thread"));
                    }
                    found = true;
                }
                entry.dwSize = u32::try_from(size_of::<THREADENTRY32>())
                    .expect("Windows thread entry fits u32");
                // SAFETY: the snapshot and correctly sized output structure remain valid.
                available =
                    unsafe { Thread32Next(snapshot.as_raw_handle().cast(), &raw mut entry) };
            }
            if !found {
                return Err(DaemonError::ProcessTree(
                    "suspended Windows process had no resumable thread".into(),
                ));
            }
            Ok(())
        }

        pub fn terminate(&self) -> Result<()> {
            // SAFETY: `job` remains valid for the lifetime of `self`.
            if unsafe { TerminateJobObject(self.job.as_raw_handle().cast::<c_void>(), 1) } == 0 {
                return Err(last_os_error("terminate Windows Job Object"));
            }
            Ok(())
        }
    }

    fn last_os_error(operation: &str) -> DaemonError {
        DaemonError::ProcessTree(format!("{operation}: {}", std::io::Error::last_os_error()))
    }
}

pub(crate) use platform::ProcessTree;

#[cfg(unix)]
pub(crate) fn attach(process_id: u32, reported_group: Option<i32>) -> Result<ProcessTree> {
    ProcessTree::attach(process_id, reported_group)
}

#[cfg(windows)]
pub(crate) fn create() -> Result<ProcessTree> {
    ProcessTree::create()
}
