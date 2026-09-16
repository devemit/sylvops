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
        ptr,
    };

    use windows_sys::Win32::{
        Foundation::HANDLE,
        System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
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
