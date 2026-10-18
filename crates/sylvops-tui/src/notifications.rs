//! Bounded, presentation-only Attention Notifications.

use sylvops_core::{
    attention::{AttentionNotificationClaim, MAX_ATTENTION_NOTIFICATION_LABEL_CHARS},
    session_state::AttentionReason,
};

#[cfg(test)]
pub(crate) use sylvops_core::attention::AttentionTracker;

const MAX_NOTIFICATION_TITLE_CHARS: usize = 48;
const MAX_NOTIFICATION_MESSAGE_CHARS: usize = 160;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InClientNotification {
    pub reason: AttentionReason,
    pub title: String,
    pub message: String,
}

pub(crate) trait NotificationChannels: std::fmt::Debug {
    fn native(&self, title: &str, message: &str) -> bool;
    fn sound(&self) -> bool;
}

#[derive(Debug)]
pub(crate) struct NotificationPresenter<C> {
    channels: C,
}

impl<C: NotificationChannels> NotificationPresenter<C> {
    pub(crate) const fn new(channels: C) -> Self {
        Self { channels }
    }

    pub(crate) fn present(
        &self,
        claim: &AttentionNotificationClaim,
    ) -> Option<InClientNotification> {
        let notification = presentation(claim);
        if claim.preferences.sound_enabled {
            let _ = self.channels.sound();
        }
        if !claim.preferences.native_notifications_enabled {
            return None;
        }
        let _ = self
            .channels
            .native(&notification.title, &notification.message);
        Some(notification)
    }

    #[cfg(test)]
    pub(crate) const fn channels(&self) -> &C {
        &self.channels
    }
}

fn presentation(claim: &AttentionNotificationClaim) -> InClientNotification {
    let name = safe_label(&claim.session_display_name);
    let (title, message) = match claim.reason {
        AttentionReason::InputRequested => (
            "Input requested",
            format!("{name} is waiting for your input."),
        ),
        AttentionReason::SuccessfulCompletion => (
            "Session completed",
            format!("{name} completed successfully."),
        ),
        AttentionReason::RecoverableFailure => (
            "Recoverable failure",
            format!("{name} needs attention before it can continue."),
        ),
        AttentionReason::TerminalFailure => {
            ("Session failed", format!("{name} stopped with a failure."))
        }
    };
    InClientNotification {
        reason: claim.reason,
        title: title.chars().take(MAX_NOTIFICATION_TITLE_CHARS).collect(),
        message: message
            .chars()
            .take(MAX_NOTIFICATION_MESSAGE_CHARS)
            .collect(),
    }
}

fn safe_label(label: &str) -> String {
    let label = label
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_ATTENTION_NOTIFICATION_LABEL_CHARS)
        .collect::<String>();
    if label.trim().is_empty() {
        "A Session".into()
    } else {
        label
    }
}

#[derive(Debug)]
pub(crate) struct SystemChannels {
    #[cfg(windows)]
    native: Option<platform::Notifier>,
    sound: Option<SoundNotifier>,
}

impl SystemChannels {
    pub(crate) fn new() -> Self {
        Self {
            #[cfg(windows)]
            native: platform::Notifier::new().ok(),
            sound: SoundNotifier::new().ok(),
        }
    }
}

impl NotificationChannels for SystemChannels {
    fn native(&self, title: &str, message: &str) -> bool {
        #[cfg(windows)]
        {
            self.native
                .as_ref()
                .is_some_and(|notifier| notifier.send(title, message))
        }
        #[cfg(not(windows))]
        {
            let _ = (title, message);
            false
        }
    }

    fn sound(&self) -> bool {
        self.sound.as_ref().is_some_and(SoundNotifier::notify)
    }
}

#[derive(Debug)]
struct SoundNotifier {
    sender: std::sync::mpsc::SyncSender<()>,
}

impl SoundNotifier {
    fn new() -> std::io::Result<Self> {
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

    fn notify(&self) -> bool {
        self.sender.try_send(()).is_ok()
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn play_sound() {
    // SAFETY: MessageBeep accepts a constant sound identifier and no pointer arguments.
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
                "--description=SylvOps Attention Notification",
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use sylvops_core::{
        attention::{AttentionNotificationClaim, AttentionNotificationPreferences},
        ids::SessionId,
        session_state::{AttentionEventId, AttentionReason},
    };

    use super::{NotificationChannels, NotificationPresenter};

    #[derive(Debug, Default)]
    struct RecordingChannels {
        native: Mutex<Vec<(String, String)>>,
        sounds: Mutex<usize>,
        fail_native: bool,
        fail_sound: bool,
    }

    impl NotificationChannels for RecordingChannels {
        fn native(&self, title: &str, message: &str) -> bool {
            self.native
                .lock()
                .unwrap()
                .push((title.into(), message.into()));
            !self.fail_native
        }

        fn sound(&self) -> bool {
            *self.sounds.lock().unwrap() += 1;
            !self.fail_sound
        }
    }

    fn claim(reason: AttentionReason) -> AttentionNotificationClaim {
        AttentionNotificationClaim {
            session_id: SessionId::new(),
            attention_id: AttentionEventId::try_from(1).unwrap(),
            reason,
            session_display_name: "Build session".into(),
            preferences: AttentionNotificationPreferences::default(),
        }
    }

    #[test]
    fn presenter_maps_every_attention_reason_to_distinct_bounded_copy() {
        let presenter = NotificationPresenter::new(RecordingChannels::default());
        let presentations = [
            AttentionReason::InputRequested,
            AttentionReason::SuccessfulCompletion,
            AttentionReason::RecoverableFailure,
            AttentionReason::TerminalFailure,
        ]
        .map(|reason| presenter.present(&claim(reason)).unwrap());

        let titles = presentations
            .iter()
            .map(|presentation| presentation.title.as_str())
            .collect::<std::collections::HashSet<_>>();
        let messages = presentations
            .iter()
            .map(|presentation| presentation.message.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(titles.len(), 4);
        assert_eq!(messages.len(), 4);
        assert!(presentations.iter().all(|presentation| {
            presentation.title.chars().count() <= 48 && presentation.message.chars().count() <= 160
        }));
    }

    #[test]
    fn independent_preferences_and_channel_failures_do_not_block_in_client_delivery() {
        let channels = RecordingChannels {
            fail_native: true,
            fail_sound: true,
            ..RecordingChannels::default()
        };
        let presenter = NotificationPresenter::new(channels);
        assert!(
            presenter
                .present(&AttentionNotificationClaim {
                    preferences: AttentionNotificationPreferences {
                        native_notifications_enabled: true,
                        sound_enabled: false,
                    },
                    ..claim(AttentionReason::RecoverableFailure)
                })
                .is_some()
        );
        assert_eq!(presenter.channels().native.lock().unwrap().len(), 1);
        assert_eq!(*presenter.channels().sounds.lock().unwrap(), 0);

        assert!(
            presenter
                .present(&AttentionNotificationClaim {
                    preferences: AttentionNotificationPreferences {
                        native_notifications_enabled: false,
                        sound_enabled: true,
                    },
                    ..claim(AttentionReason::TerminalFailure)
                })
                .is_none()
        );
        assert_eq!(presenter.channels().native.lock().unwrap().len(), 1);
        assert_eq!(*presenter.channels().sounds.lock().unwrap(), 1);
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) mod platform {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };
    use windows_sys::{
        Win32::UI::{
            Shell::{
                NIF_ICON, NIF_INFO, NIF_TIP, NIIF_INFO, NIIF_NOSOUND, NIIF_RESPECT_QUIET_TIME,
                NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, DispatchMessageW, HWND_MESSAGE, IDI_INFORMATION,
                LoadIconW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
            },
        },
        core::w,
    };

    #[derive(Debug)]
    pub(crate) struct Notifier {
        sender: mpsc::SyncSender<(String, String)>,
        stopped: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Notifier {
        pub fn new() -> std::io::Result<Self> {
            let (sender, receiver) = mpsc::sync_channel::<(String, String)>(8);
            let stopped = Arc::new(AtomicBool::new(false));
            let stop = stopped.clone();
            let thread = thread::Builder::new()
                .name("sylvops-notifications".into())
                .spawn(move || {
                    // SAFETY: all handles and message pumping remain on this dedicated thread.
                    // STATIC is a built-in class; HWND_MESSAGE creates no visible application window.
                    unsafe {
                        let window = CreateWindowExW(
                            0,
                            w!("STATIC"),
                            w!("SylvOps"),
                            0,
                            0,
                            0,
                            0,
                            0,
                            HWND_MESSAGE,
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                            std::ptr::null(),
                        );
                        if window.is_null() {
                            return;
                        }
                        let mut data = NOTIFYICONDATAW {
                            cbSize: u32::try_from(std::mem::size_of::<NOTIFYICONDATAW>())
                                .unwrap_or_default(),
                            hWnd: window,
                            uID: 1,
                            uFlags: NIF_ICON | NIF_TIP,
                            hIcon: LoadIconW(std::ptr::null_mut(), IDI_INFORMATION),
                            ..Default::default()
                        };
                        copy_text(&mut data.szTip, "SylvOps session alerts");
                        if Shell_NotifyIconW(NIM_ADD, &raw const data) == 0 {
                            DestroyWindow(window);
                            return;
                        }
                        while !stop.load(Ordering::Acquire) {
                            match receiver.recv_timeout(Duration::from_millis(100)) {
                                Ok((title, message)) => {
                                    data.uFlags = NIF_INFO;
                                    data.dwInfoFlags =
                                        NIIF_INFO | NIIF_NOSOUND | NIIF_RESPECT_QUIET_TIME;
                                    copy_text(&mut data.szInfoTitle, &title);
                                    copy_text(&mut data.szInfo, &message);
                                    if Shell_NotifyIconW(NIM_MODIFY, &raw const data) == 0 {
                                        break;
                                    }
                                }
                                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                                Err(mpsc::RecvTimeoutError::Timeout) => {}
                            }
                            let mut message = MSG::default();
                            while PeekMessageW(&raw mut message, window, 0, 0, PM_REMOVE) != 0 {
                                TranslateMessage(&raw const message);
                                DispatchMessageW(&raw const message);
                            }
                        }
                        Shell_NotifyIconW(NIM_DELETE, &raw const data);
                        DestroyWindow(window);
                    }
                })?;
            Ok(Self {
                sender,
                stopped,
                thread: Some(thread),
            })
        }

        pub fn send(&self, title: &str, message: &str) -> bool {
            self.sender
                .try_send((title.to_owned(), message.to_owned()))
                .is_ok()
        }
    }

    impl Drop for Notifier {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn copy_text<const N: usize>(target: &mut [u16; N], text: &str) {
        target.fill(0);
        let mut offset = 0;
        for character in text.chars().filter(|character| !character.is_control()) {
            let mut encoded = [0_u16; 2];
            let encoded = character.encode_utf16(&mut encoded);
            if offset + encoded.len() >= N {
                break;
            }
            target[offset..offset + encoded.len()].copy_from_slice(encoded);
            offset += encoded.len();
        }
    }
}
