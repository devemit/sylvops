//! Platform PTY launch boundary used by the session actor.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::{Read, Write},
};

use crate::{Result, process_tree::ProcessTree, session::SessionSpec};

pub(crate) trait PtyMaster: Send {
    fn resize(&mut self, columns: u16, rows: u16) -> Result<()>;
}

pub(crate) trait PtyChild: Send {
    fn wait(&mut self) -> Result<u32>;
}

pub(crate) struct SpawnedPty {
    pub master: Box<dyn PtyMaster>,
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub child: Box<dyn PtyChild>,
    pub process_tree: ProcessTree,
    pub process_id: u32,
}

pub(crate) fn spawn(
    spec: &SessionSpec,
    environment: Option<&BTreeMap<OsString, OsString>>,
) -> Result<SpawnedPty> {
    platform::spawn(spec, environment)
}

#[cfg(unix)]
mod platform {
    use std::{collections::BTreeMap, ffi::OsString};

    use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

    use super::{PtyChild, PtyMaster, SpawnedPty};
    use crate::{DaemonError, Result, process_tree, session::SessionSpec};

    struct PortableMaster(Box<dyn MasterPty + Send>);

    impl PtyMaster for PortableMaster {
        fn resize(&mut self, columns: u16, rows: u16) -> Result<()> {
            self.0
                .resize(PtySize {
                    rows,
                    cols: columns,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|error| DaemonError::Pty(error.to_string()))
        }
    }

    struct PortableChild(Box<dyn portable_pty::Child + Send + Sync>);

    impl PtyChild for PortableChild {
        fn wait(&mut self) -> Result<u32> {
            self.0
                .wait()
                .map(|status| status.exit_code())
                .map_err(|error| DaemonError::Pty(error.to_string()))
        }
    }

    pub(super) fn spawn(
        spec: &SessionSpec,
        environment: Option<&BTreeMap<OsString, OsString>>,
    ) -> Result<SpawnedPty> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: spec.rows,
                cols: spec.columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| DaemonError::Pty(error.to_string()))?;
        let mut command = CommandBuilder::new(spec.program.as_os_str());
        command.args(&spec.arguments);
        command.cwd(&spec.cwd);
        if let Some(environment) = environment {
            command.env_clear();
            for (key, value) in environment {
                command.env(key, value);
            }
        }
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| DaemonError::Pty(error.to_string()))?;
        drop(pair.slave);
        let process_id = child.process_id().ok_or_else(|| {
            DaemonError::ProcessTree("PTY library did not report a process ID".into())
        })?;
        let process_tree =
            match process_tree::attach(process_id, pair.master.process_group_leader()) {
                Ok(tree) => tree,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            };
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| DaemonError::Pty(error.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| DaemonError::Pty(error.to_string()))?;
        Ok(SpawnedPty {
            master: Box::new(PortableMaster(pair.master)),
            reader,
            writer,
            child: Box::new(PortableChild(child)),
            process_tree,
            process_id,
        })
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    use std::{
        collections::BTreeMap,
        ffi::{OsStr, OsString, c_void},
        fs::File,
        mem::size_of,
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
        ptr,
    };

    use windows_sys::Win32::{
        Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
        System::{
            Console::{COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole},
            Pipes::CreatePipe,
            Threading::{
                CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
                EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
                InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
                PROC_THREAD_ATTRIBUTE_JOB_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
                UpdateProcThreadAttribute, WaitForSingleObject,
            },
        },
    };

    use super::{PtyChild, PtyMaster, SpawnedPty};
    use crate::{DaemonError, Result, process_tree, session::SessionSpec};

    struct ConPtyMaster {
        handle: HPCON,
    }

    impl PtyMaster for ConPtyMaster {
        fn resize(&mut self, columns: u16, rows: u16) -> Result<()> {
            let size = coord(columns, rows)?;
            // SAFETY: the pseudoconsole is owned by this value and `size` is validated.
            let result = unsafe { ResizePseudoConsole(self.handle, size) };
            if result < 0 {
                return Err(DaemonError::Pty(format!(
                    "resize Windows pseudoconsole failed with HRESULT {result:#x}"
                )));
            }
            Ok(())
        }
    }

    impl Drop for ConPtyMaster {
        fn drop(&mut self) {
            // SAFETY: this value uniquely owns the non-zero pseudoconsole handle.
            unsafe { ClosePseudoConsole(self.handle) };
        }
    }

    struct WindowsChild {
        process: OwnedHandle,
    }

    impl PtyChild for WindowsChild {
        fn wait(&mut self) -> Result<u32> {
            // SAFETY: `process` is a live process handle and remains owned throughout the wait.
            let wait = unsafe { WaitForSingleObject(self.raw_process(), INFINITE) };
            if wait != WAIT_OBJECT_0 {
                return Err(last_os_error("wait for Windows PTY child"));
            }
            let mut exit_code = 1;
            // SAFETY: the process has signalled and `exit_code` is a valid output pointer.
            if unsafe { GetExitCodeProcess(self.raw_process(), &raw mut exit_code) } == 0 {
                return Err(last_os_error("read Windows PTY child exit code"));
            }
            Ok(exit_code)
        }
    }

    impl WindowsChild {
        fn raw_process(&self) -> HANDLE {
            self.process.as_raw_handle().cast::<c_void>()
        }
    }

    struct AttributeList {
        storage: Vec<usize>,
        job_handle: Option<Box<HANDLE>>,
    }

    impl AttributeList {
        fn new(attributes: u32) -> Result<Self> {
            let mut bytes = 0_usize;
            // SAFETY: the first call intentionally supplies a null list to query its size.
            unsafe {
                InitializeProcThreadAttributeList(ptr::null_mut(), attributes, 0, &raw mut bytes)
            };
            if bytes == 0 {
                return Err(last_os_error("size process attribute list"));
            }
            let words = bytes.div_ceil(size_of::<usize>());
            let mut list = Self {
                storage: vec![0; words],
                job_handle: None,
            };
            // SAFETY: the aligned storage is at least the byte count requested by Windows.
            if unsafe {
                InitializeProcThreadAttributeList(list.as_ptr(), attributes, 0, &raw mut bytes)
            } == 0
            {
                return Err(last_os_error("initialize process attribute list"));
            }
            Ok(list)
        }

        fn set_pseudoconsole(&mut self, handle: HPCON) -> Result<()> {
            // The documented pseudoconsole attribute consumes the HPCON value itself.
            // SAFETY: the list is initialized and the handle remains alive through process spawn.
            if unsafe {
                UpdateProcThreadAttribute(
                    self.as_ptr(),
                    0,
                    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                    handle as *const c_void,
                    size_of::<HPCON>(),
                    ptr::null_mut(),
                    ptr::null(),
                )
            } == 0
            {
                return Err(last_os_error("set pseudoconsole process attribute"));
            }
            Ok(())
        }

        fn set_job(&mut self, job: HANDLE) -> Result<()> {
            let job_handle = Box::new(job);
            // SAFETY: the list is initialized and the boxed handle value remains at a stable
            // address through process creation.
            if unsafe {
                UpdateProcThreadAttribute(
                    self.as_ptr(),
                    0,
                    PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                    (&raw const *job_handle).cast::<c_void>(),
                    size_of::<HANDLE>(),
                    ptr::null_mut(),
                    ptr::null(),
                )
            } == 0
            {
                return Err(last_os_error("set Job Object process attribute"));
            }
            // UpdateProcThreadAttribute retains the pointer rather than copying this handle list.
            // Keep its heap allocation stable until after CreateProcessW consumes the attributes.
            self.job_handle = Some(job_handle);
            Ok(())
        }

        fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
            self.storage.as_mut_ptr().cast::<c_void>()
        }
    }

    impl Drop for AttributeList {
        fn drop(&mut self) {
            // SAFETY: successful construction initialized this list exactly once.
            unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
            drop(self.job_handle.take());
        }
    }

    pub(super) fn spawn(
        spec: &SessionSpec,
        environment: Option<&BTreeMap<OsString, OsString>>,
    ) -> Result<SpawnedPty> {
        let size = coord(spec.columns, spec.rows)?;
        let (console_input, host_writer) = pipe()?;
        let (host_reader, console_output) = pipe()?;
        let mut pseudoconsole = 0;
        // SAFETY: all four handles are valid, distinct pipe endpoints and the output pointer lives.
        let result = unsafe {
            CreatePseudoConsole(
                size,
                console_input.as_raw_handle().cast::<c_void>(),
                console_output.as_raw_handle().cast::<c_void>(),
                0,
                &raw mut pseudoconsole,
            )
        };
        if result < 0 || pseudoconsole == 0 {
            return Err(DaemonError::Pty(format!(
                "create Windows pseudoconsole failed with HRESULT {result:#x}"
            )));
        }
        let master = ConPtyMaster {
            handle: pseudoconsole,
        };

        let process_tree = process_tree::create()?;
        let mut attributes = AttributeList::new(2)?;
        attributes.set_pseudoconsole(master.handle)?;
        attributes.set_job(process_tree.raw_handle())?;

        let application = nul_terminated(spec.program.as_os_str(), "program")?;
        let mut command_line = build_command_line(spec.program.as_os_str(), &spec.arguments)?;
        let cwd = nul_terminated(spec.cwd.as_os_str(), "working directory")?;
        let environment_block = environment.map(build_environment_block).transpose()?;
        let environment_pointer = environment_block
            .as_ref()
            .map_or(ptr::null(), |block| block.as_ptr().cast::<c_void>());
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb =
            u32::try_from(size_of::<STARTUPINFOEXW>()).expect("Windows startup structure fits u32");
        // Prevent redirected or console standard handles from the daemon from bypassing ConPTY.
        // The pseudoconsole supplies the child's actual terminal handles during process creation.
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
        startup.lpAttributeList = attributes.as_ptr();
        let mut process = PROCESS_INFORMATION::default();
        // SAFETY: all pointers refer to mutable or immutable, NUL-terminated buffers that remain
        // alive for this call. The attribute list owns valid ConPTY and Job Object attributes.
        let created = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                environment_pointer,
                cwd.as_ptr(),
                (&raw const startup.StartupInfo),
                &raw mut process,
            )
        };
        if created == 0 {
            return Err(last_os_error(
                "create Windows PTY child atomically inside Job Object",
            ));
        }
        // The ConPTY connection endpoints must remain open through CreateProcessW. The
        // pseudoconsole owns its references after the child has been attached.
        drop(console_input);
        drop(console_output);
        // SAFETY: CreateProcessW returned these two independently owned handles.
        let process_handle = unsafe { OwnedHandle::from_raw_handle(process.hProcess) };
        // SAFETY: the initial thread is resumed by CreateProcessW and no longer needed.
        drop(unsafe { OwnedHandle::from_raw_handle(process.hThread) });

        Ok(SpawnedPty {
            master: Box::new(master),
            reader: Box::new(File::from(host_reader)),
            writer: Box::new(File::from(host_writer)),
            child: Box::new(WindowsChild {
                process: process_handle,
            }),
            process_tree,
            process_id: process.dwProcessId,
        })
    }

    fn pipe() -> Result<(OwnedHandle, OwnedHandle)> {
        let mut read = ptr::null_mut();
        let mut write = ptr::null_mut();
        // SAFETY: both output pointers are valid; null security attributes request defaults.
        if unsafe { CreatePipe(&raw mut read, &raw mut write, ptr::null(), 0) } == 0 {
            return Err(last_os_error("create Windows pseudoconsole pipe"));
        }
        // SAFETY: CreatePipe returned two distinct owned handles.
        Ok(unsafe {
            (
                OwnedHandle::from_raw_handle(read),
                OwnedHandle::from_raw_handle(write),
            )
        })
    }

    fn coord(columns: u16, rows: u16) -> Result<COORD> {
        let x = i16::try_from(columns)
            .map_err(|_| DaemonError::InvalidSession("terminal columns exceed i16".into()))?;
        let y = i16::try_from(rows)
            .map_err(|_| DaemonError::InvalidSession("terminal rows exceed i16".into()))?;
        Ok(COORD { X: x, Y: y })
    }

    fn nul_terminated(value: &OsStr, description: &str) -> Result<Vec<u16>> {
        let mut wide: Vec<u16> = value.encode_wide().collect();
        if wide.contains(&0) {
            return Err(DaemonError::InvalidSession(format!(
                "{description} contains a NUL"
            )));
        }
        wide.push(0);
        Ok(wide)
    }

    fn build_command_line(program: &OsStr, arguments: &[OsString]) -> Result<Vec<u16>> {
        let mut command = Vec::new();
        append_quoted_argument(&mut command, program)?;
        for argument in arguments {
            command.push(u16::from(b' '));
            append_quoted_argument(&mut command, argument)?;
        }
        command.push(0);
        Ok(command)
    }

    fn append_quoted_argument(command: &mut Vec<u16>, argument: &OsStr) -> Result<()> {
        let argument: Vec<u16> = argument.encode_wide().collect();
        if argument.contains(&0) {
            return Err(DaemonError::InvalidSession(
                "process argument contains a NUL".into(),
            ));
        }
        command.push(u16::from(b'"'));
        let mut backslashes = 0_usize;
        for unit in argument {
            if unit == u16::from(b'\\') {
                backslashes += 1;
                continue;
            }
            if unit == u16::from(b'"') {
                command.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2 + 1));
            } else {
                command.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
            }
            backslashes = 0;
            command.push(unit);
        }
        command.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
        command.push(u16::from(b'"'));
        Ok(())
    }

    fn build_environment_block(environment: &BTreeMap<OsString, OsString>) -> Result<Vec<u16>> {
        let mut entries = Vec::with_capacity(environment.len());
        for (key, value) in environment {
            let key_wide: Vec<u16> = key.encode_wide().collect();
            let value_wide: Vec<u16> = value.encode_wide().collect();
            if key_wide.is_empty()
                || key_wide.contains(&0)
                || key_wide.contains(&u16::from(b'='))
                || value_wide.contains(&0)
            {
                return Err(DaemonError::InvalidSession(
                    "environment entry has an invalid name, equals sign, or NUL".into(),
                ));
            }
            entries.push((key_wide, value_wide));
        }
        entries.sort_by(|left, right| compare_environment_keys(&left.0, &right.0));
        for pair in entries.windows(2) {
            if compare_environment_keys(&pair[0].0, &pair[1].0).is_eq() {
                return Err(DaemonError::InvalidSession(
                    "environment contains case-insensitive duplicate names".into(),
                ));
            }
        }
        let mut block = Vec::new();
        for (key, value) in entries {
            block.extend(key);
            block.push(u16::from(b'='));
            block.extend(value);
            block.push(0);
        }
        block.push(0);
        if environment.is_empty() {
            block.push(0);
        }
        Ok(block)
    }

    fn compare_environment_keys(left: &[u16], right: &[u16]) -> std::cmp::Ordering {
        left.iter()
            .map(|unit| ascii_uppercase(*unit))
            .cmp(right.iter().map(|unit| ascii_uppercase(*unit)))
    }

    const fn ascii_uppercase(unit: u16) -> u16 {
        if unit >= b'a' as u16 && unit <= b'z' as u16 {
            unit - (b'a' - b'A') as u16
        } else {
            unit
        }
    }

    fn last_os_error(operation: &str) -> DaemonError {
        DaemonError::Pty(format!("{operation}: {}", std::io::Error::last_os_error()))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn decode(value: &[u16]) -> String {
            String::from_utf16_lossy(value)
        }

        #[test]
        fn command_line_quotes_spaces_quotes_and_trailing_backslashes() {
            let command = build_command_line(
                OsStr::new(r"C:\Program Files\SylvOps\agent.exe"),
                &[
                    OsString::from("plain"),
                    OsString::from("two words"),
                    OsString::from("quoted\"value"),
                    OsString::from(r"C:\tail\"),
                ],
            )
            .unwrap();
            assert_eq!(
                decode(&command[..command.len() - 1]),
                r#""C:\Program Files\SylvOps\agent.exe" "plain" "two words" "quoted\"value" "C:\tail\\""#
            );
        }

        #[test]
        fn environment_rejects_case_insensitive_duplicates() {
            let environment = BTreeMap::from([
                (OsString::from("Path"), OsString::from("one")),
                (OsString::from("PATH"), OsString::from("two")),
            ]);
            assert!(build_environment_block(&environment).is_err());
        }

        #[test]
        fn unicode_arguments_and_environment_are_preserved() {
            let command = build_command_line(
                OsStr::new(r"C:\SylvOps\agent.exe"),
                &[OsString::from("héllo-测试")],
            )
            .unwrap();
            assert!(decode(&command).contains("héllo-测试"));

            let environment =
                BTreeMap::from([(OsString::from("SYLVOPS_TEST"), OsString::from("välue-测试"))]);
            let block = build_environment_block(&environment).unwrap();
            assert!(decode(&block).contains("SYLVOPS_TEST=välue-测试"));
        }

        #[test]
        fn command_line_and_environment_reject_nuls() {
            assert!(
                build_command_line(
                    OsStr::new(r"C:\SylvOps\agent.exe"),
                    &[OsString::from("bad\0argument")],
                )
                .is_err()
            );
            let environment =
                BTreeMap::from([(OsString::from("SYLVOPS_TEST"), OsString::from("bad\0value"))]);
            assert!(build_environment_block(&environment).is_err());
        }
    }
}
