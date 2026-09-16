//! Platform-local transport plus retained Phase 0 request/response fixtures.

use std::io;

#[cfg(unix)]
use std::path::{Path, PathBuf};

use sylvops_core::protocol::{
    Frame, MessageClass, PROTOCOL_MAJOR, PROTOCOL_MINOR, PhaseZeroRequest, PhaseZeroResponse,
    read_frame, write_frame,
};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{DaemonError, Result};

pub const PHASE_ZERO_OPCODE: u16 = 1;

pub trait AsyncStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T> AsyncStream for T where T: AsyncRead + AsyncWrite + Send + Unpin {}
pub type BoxStream = Box<dyn AsyncStream>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalEndpoint {
    #[cfg(unix)]
    Unix(PathBuf),
    #[cfg(windows)]
    WindowsPipe(String),
}

impl LocalEndpoint {
    #[cfg(unix)]
    #[must_use]
    pub fn unix(path: impl Into<PathBuf>) -> Self {
        Self::Unix(path.into())
    }

    #[cfg(windows)]
    #[must_use]
    pub fn windows_pipe(name: impl Into<String>) -> Self {
        Self::WindowsPipe(name.into())
    }
}

#[derive(Debug)]
pub struct LocalListener {
    inner: ListenerInner,
}

#[derive(Debug)]
enum ListenerInner {
    #[cfg(unix)]
    Unix {
        listener: tokio::net::UnixListener,
        path: PathBuf,
    },
    #[cfg(windows)]
    Windows {
        name: String,
        next: tokio::net::windows::named_pipe::NamedPipeServer,
    },
}

impl LocalListener {
    /// Binds the platform-local daemon listener.
    ///
    /// # Errors
    ///
    /// Returns an error when the endpoint is invalid, occupied, or cannot be created securely.
    pub fn bind(endpoint: &LocalEndpoint) -> Result<Self> {
        match endpoint {
            #[cfg(unix)]
            LocalEndpoint::Unix(path) => bind_unix(path),
            #[cfg(windows)]
            LocalEndpoint::WindowsPipe(name) => bind_windows(name),
        }
    }

    /// Accepts one local client connection.
    ///
    /// # Errors
    ///
    /// Returns an operating-system error if accepting or preparing the next listener fails.
    pub async fn accept(&mut self) -> Result<BoxStream> {
        match &mut self.inner {
            #[cfg(unix)]
            ListenerInner::Unix { listener, .. } => {
                let (stream, _) = listener.accept().await?;
                Ok(Box::new(stream))
            }
            #[cfg(windows)]
            ListenerInner::Windows { name, next } => {
                next.connect().await?;
                let replacement = create_pipe_server(name, false)?;
                let connected = std::mem::replace(next, replacement);
                Ok(Box::new(connected))
            }
        }
    }
}

#[cfg(unix)]
impl Drop for LocalListener {
    fn drop(&mut self) {
        let ListenerInner::Unix { path, .. } = &self.inner;
        if let Ok(metadata) = std::fs::symlink_metadata(path) {
            use std::os::unix::fs::FileTypeExt;
            if metadata.file_type().is_socket() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

#[cfg(windows)]
impl Drop for LocalListener {
    fn drop(&mut self) {}
}

/// Connects to a platform-local daemon endpoint.
///
/// # Errors
///
/// Returns an operating-system error when the endpoint cannot be opened.
#[cfg_attr(windows, allow(clippy::unused_async))]
pub async fn connect(endpoint: &LocalEndpoint) -> Result<BoxStream> {
    match endpoint {
        #[cfg(unix)]
        LocalEndpoint::Unix(path) => Ok(Box::new(tokio::net::UnixStream::connect(path).await?)),
        #[cfg(windows)]
        LocalEndpoint::WindowsPipe(name) => {
            let client = tokio::net::windows::named_pipe::ClientOptions::new().open(name)?;
            Ok(Box::new(client))
        }
    }
}

/// Serves Phase 0 health requests on one connected stream until disconnect.
///
/// # Errors
///
/// Returns a protocol or I/O error for malformed frames or failed responses.
pub async fn serve_phase_zero_connection(mut stream: BoxStream) -> Result<()> {
    loop {
        let frame = match read_frame(&mut stream).await {
            Ok(frame) => frame,
            Err(sylvops_core::CoreError::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        if frame.class != MessageClass::Request || frame.opcode != PHASE_ZERO_OPCODE {
            let response = PhaseZeroResponse::Error {
                code: "unsupported_message".into(),
                message: "Phase 0 accepts only the spike request opcode".into(),
            };
            write_frame(
                &mut stream,
                &Frame::response(PHASE_ZERO_OPCODE, frame.message_id, &response)?,
            )
            .await?;
            continue;
        }

        let response = match frame.payload_as::<PhaseZeroRequest>() {
            Ok(PhaseZeroRequest::Hello { .. }) => PhaseZeroResponse::Welcome {
                server_name: "sylvops-phase-zero".into(),
            },
            Ok(PhaseZeroRequest::Health) => PhaseZeroResponse::Healthy {
                protocol: format!("{PROTOCOL_MAJOR}.{PROTOCOL_MINOR}"),
            },
            Err(error) => PhaseZeroResponse::Error {
                code: "invalid_payload".into(),
                message: error.to_string(),
            },
        };
        write_frame(
            &mut stream,
            &Frame::response(PHASE_ZERO_OPCODE, frame.message_id, &response)?,
        )
        .await?;
    }
}

/// Sends one Phase 0 request and validates response correlation.
///
/// # Errors
///
/// Returns an error for I/O, framing, payload, or request-correlation failure.
pub async fn request(
    stream: &mut BoxStream,
    request: &PhaseZeroRequest,
) -> Result<PhaseZeroResponse> {
    let frame = Frame::message(MessageClass::Request, PHASE_ZERO_OPCODE, request)?;
    let request_id = frame.message_id;
    write_frame(stream, &frame).await?;
    let response = read_frame(stream).await?;
    if response.class != MessageClass::Response || response.correlation_id != Some(request_id) {
        return Err(DaemonError::InvalidSession(
            "IPC response did not correlate to its request".into(),
        ));
    }
    response.payload_as().map_err(Into::into)
}

#[cfg(unix)]
fn bind_unix(path: &Path) -> Result<LocalListener> {
    use std::os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::UnixStream,
    };

    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if !metadata.file_type().is_socket() {
            return Err(DaemonError::Ipc(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "refusing to replace a non-socket IPC path",
            )));
        }
        if metadata.uid() != nix::unistd::Uid::effective().as_raw() {
            return Err(DaemonError::Ipc(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to replace a socket owned by another user",
            )));
        }
        match UnixStream::connect(path) {
            Ok(_) => {
                return Err(DaemonError::Ipc(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "a SylvOps daemon is already listening",
                )));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                std::fs::remove_file(path)?;
            }
            Err(error) => return Err(DaemonError::Ipc(error)),
        }
    }
    if let Some(parent) = path.parent() {
        match std::fs::symlink_metadata(parent) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() {
                    return Err(DaemonError::Ipc(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "IPC socket parent is not a directory",
                    )));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                std::fs::create_dir_all(parent)?;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
            Err(error) => return Err(DaemonError::Ipc(error)),
        }
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(LocalListener {
        inner: ListenerInner::Unix {
            listener,
            path: path.to_path_buf(),
        },
    })
}

#[cfg(windows)]
fn bind_windows(name: &str) -> Result<LocalListener> {
    validate_pipe_name(name)?;
    Ok(LocalListener {
        inner: ListenerInner::Windows {
            name: name.to_owned(),
            next: create_pipe_server(name, true)?,
        },
    })
}

#[cfg(windows)]
fn validate_pipe_name(name: &str) -> Result<()> {
    if !name.starts_with(r"\\.\pipe\sylvops-")
        || name.len() > 240
        || name.chars().any(char::is_control)
    {
        return Err(DaemonError::Ipc(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid SylvOps named-pipe path",
        )));
    }
    Ok(())
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn create_pipe_server(
    name: &str,
    first: bool,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use std::{ffi::c_void, mem::size_of, ptr};

    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            },
            PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        },
    };

    let mut options = tokio::net::windows::named_pipe::ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true);

    // Protected DACL: grant file-all access only to the process token's actual user SID. Using the
    // generic owner SID is insufficient for split administrator tokens: the default object owner
    // can be the Administrators group, which is deny-only in the unelevated client token.
    let user_sid = current_process_user_sid()?;
    let sddl: Vec<u16> = format!("D:P(A;;FA;;;{user_sid})")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated; `descriptor` is a valid output pointer. On success the
    // returned descriptor is owned by LocalAlloc and released with LocalFree below.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(DaemonError::Ipc(io::Error::last_os_error()));
    }
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).expect("structure size fits u32"),
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    // SAFETY: `attributes` and its descriptor remain valid for the complete CreateNamedPipeW
    // call. Windows copies the security descriptor into the new object before returning.
    let created = unsafe {
        options.create_with_security_attributes_raw(name, (&raw mut attributes).cast::<c_void>())
    };
    // SAFETY: `descriptor` was allocated by the successful conversion call above and has not
    // previously been freed.
    let _ = unsafe { LocalFree(descriptor) };
    created.map_err(DaemonError::Ipc)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn current_process_user_sid() -> Result<String> {
    use std::{mem::size_of, ptr, slice};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, LocalFree},
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation, TOKEN_QUERY, TOKEN_USER,
            TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    let mut token = ptr::null_mut();
    // SAFETY: the pseudo process handle is valid and `token` is a valid output pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(DaemonError::Ipc(io::Error::last_os_error()));
    }

    let result = (|| {
        let mut required = 0_u32;
        // SAFETY: a null buffer with length zero is the documented size-query form.
        let _ =
            unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &raw mut required) };
        if required < u32::try_from(size_of::<TOKEN_USER>()).expect("TOKEN_USER size fits u32") {
            return Err(DaemonError::Ipc(io::Error::last_os_error()));
        }

        let words = usize::try_from(required)
            .expect("Windows token information length fits usize")
            .div_ceil(size_of::<usize>());
        let mut buffer = vec![0_usize; words];
        // SAFETY: the word-aligned buffer has at least `required` writable bytes.
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                required,
                &raw mut required,
            )
        } == 0
        {
            return Err(DaemonError::Ipc(io::Error::last_os_error()));
        }
        // SAFETY: successful TokenUser information begins with a valid TOKEN_USER structure.
        let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        let mut string_sid = ptr::null_mut();
        // SAFETY: the token owns a valid SID and the output pointer is valid. Windows allocates
        // the returned UTF-16 string with LocalAlloc.
        if unsafe { ConvertSidToStringSidW(token_user.User.Sid, &raw mut string_sid) } == 0 {
            return Err(DaemonError::Ipc(io::Error::last_os_error()));
        }
        let sid_result = {
            let mut length = 0_usize;
            // SAFETY: `string_sid` points to a NUL-terminated UTF-16 string.
            while unsafe { *string_sid.add(length) } != 0 {
                length += 1;
            }
            // SAFETY: `length` was found by scanning the valid NUL-terminated allocation.
            let units = unsafe { slice::from_raw_parts(string_sid, length) };
            String::from_utf16(units).map_err(|error| {
                DaemonError::Ipc(io::Error::new(io::ErrorKind::InvalidData, error))
            })
        };
        // SAFETY: `string_sid` was allocated by ConvertSidToStringSidW and is released once.
        let _ = unsafe { LocalFree(string_sid.cast()) };
        sid_result
    })();

    // SAFETY: `token` is the valid handle returned by OpenProcessToken and is closed once.
    let _ = unsafe { CloseHandle(token) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_response_correlates_over_duplex_stream() {
        let (client, server) = tokio::io::duplex(4096);
        let server_task = tokio::spawn(serve_phase_zero_connection(Box::new(server)));
        let mut client: BoxStream = Box::new(client);

        let response = request(&mut client, &PhaseZeroRequest::Health)
            .await
            .unwrap();
        assert!(matches!(response, PhaseZeroResponse::Healthy { .. }));

        drop(client);
        server_task.await.unwrap().unwrap();
    }
}
