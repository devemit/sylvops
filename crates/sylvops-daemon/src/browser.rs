//! Explicit platform-browser launching from validated typed targets.

use std::process::Stdio;

use sylvops_core::github::GitHubBrowserTarget;

use crate::background_process;

#[derive(Clone, Debug, Eq, PartialEq)]
struct BrowserCommand {
    program: &'static str,
    arguments: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DesktopPlatform {
    Windows,
    MacOs,
    Linux,
}

impl DesktopPlatform {
    fn current() -> Self {
        match std::env::consts::OS {
            "windows" => Self::Windows,
            "macos" => Self::MacOs,
            _ => Self::Linux,
        }
    }
}

fn command_for(platform: DesktopPlatform, target: &GitHubBrowserTarget) -> BrowserCommand {
    let target = target.as_str().to_owned();
    match platform {
        DesktopPlatform::Windows => BrowserCommand {
            program: "rundll32.exe",
            arguments: vec!["url.dll,FileProtocolHandler".into(), target],
        },
        DesktopPlatform::MacOs => BrowserCommand {
            program: "/usr/bin/open",
            arguments: vec![target],
        },
        DesktopPlatform::Linux => BrowserCommand {
            program: "xdg-open",
            arguments: vec![target],
        },
    }
}

pub(crate) fn open(target: &GitHubBrowserTarget) -> Result<(), String> {
    let specification = command_for(DesktopPlatform::current(), target);
    let mut command = background_process::command(specification.program);
    command
        .args(specification.arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
        .spawn()
        .map(|_| ())
        .map_err(|_| "The platform browser could not be opened.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_commands_are_structured_for_every_supported_platform() {
        let target: sylvops_core::github::GitHubBrowserTarget =
            "https://github.example.com/acme/widgets/issues/125"
                .to_owned()
                .try_into()
                .unwrap();

        let windows = command_for(DesktopPlatform::Windows, &target);
        assert_eq!(windows.program, "rundll32.exe");
        assert_eq!(
            windows.arguments,
            ["url.dll,FileProtocolHandler", target.as_str()]
        );

        let macos = command_for(DesktopPlatform::MacOs, &target);
        assert_eq!(macos.program, "/usr/bin/open");
        assert_eq!(macos.arguments, [target.as_str()]);

        let linux = command_for(DesktopPlatform::Linux, &target);
        assert_eq!(linux.program, "xdg-open");
        assert_eq!(linux.arguments, [target.as_str()]);
    }
}
