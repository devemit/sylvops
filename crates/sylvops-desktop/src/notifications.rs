//! Bounded, presentation-only sound playback. Never pass provider text to a process.

#[derive(Debug)]
pub(crate) struct SoundNotifier {
    sender: std::sync::mpsc::SyncSender<()>,
}

impl SoundNotifier {
    pub fn new() -> std::io::Result<Self> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("sylvops-attention-sound".into())
            .spawn(move || {
                for () in receiver {
                    play_sound();
                }
            })?;
        Ok(Self { sender })
    }

    pub fn notify(&self) {
        let _ = self.sender.try_send(());
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn play_sound() {
    // SAFETY: MessageBeep accepts a constant sound identifier and has no pointer arguments.
    unsafe {
        windows_sys::Win32::System::Diagnostics::Debug::MessageBeep(
            windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONINFORMATION,
        );
    }
}

#[cfg(not(windows))]
fn play_sound() {
    use std::{
        path::Path,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    #[cfg(target_os = "macos")]
    let choices: &[(&str, &[&str])] =
        &[("/usr/bin/afplay", &["/System/Library/Sounds/Glass.aiff"])];
    #[cfg(not(target_os = "macos"))]
    let choices: &[(&str, &[&str])] = &[
        (
            "/usr/bin/canberra-gtk-play",
            &[
                "--id=message-new-instant",
                "--description=SylvOps needs your input",
            ],
        ),
        (
            "/usr/bin/paplay",
            &["/usr/share/sounds/freedesktop/stereo/message.oga"],
        ),
    ];
    for (executable, arguments) in choices {
        if !Path::new(executable).is_file() {
            continue;
        }
        let Ok(mut child) = Command::new(executable)
            .args(*arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return,
                Ok(Some(_)) => break,
                Ok(None) if started.elapsed() < Duration::from_secs(2) => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
    }
}
