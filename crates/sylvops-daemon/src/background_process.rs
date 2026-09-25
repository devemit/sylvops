//! Background subprocess construction with platform-appropriate window behavior.

use std::ffi::OsStr;

use tokio::process::Command;

pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    configure(&mut command);
    command
}

#[cfg(not(windows))]
fn configure(_command: &mut Command) {}

#[cfg(windows)]
fn configure(command: &mut Command) {
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    command.creation_flags(CREATE_NO_WINDOW);
}
