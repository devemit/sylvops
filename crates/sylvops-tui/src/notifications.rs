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
    pub native_queued: bool,
    pub reason: AttentionReason,
    pub title: String,
    pub message: String,
}

pub(crate) trait NotificationChannels: std::fmt::Debug {
    fn native(&self, title: &str, message: &str) -> bool;
    fn sound(&self) -> bool;
    fn take_native_failure(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub(crate) struct NotificationPresenter<C> {
    channels: C,
}

impl<C: NotificationChannels> NotificationPresenter<C> {
    pub(crate) fn take_native_failure(&self) -> bool {
        self.channels.take_native_failure()
    }
    pub(crate) const fn new(channels: C) -> Self {
        Self { channels }
    }

    pub(crate) fn present(
        &self,
        claim: &AttentionNotificationClaim,
    ) -> Option<InClientNotification> {
        let mut notification = presentation(claim);
        if claim.preferences.sound_enabled {
            let _ = self.channels.sound();
        }
        if !claim.preferences.native_notifications_enabled {
            return None;
        }
        notification.native_queued = self
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
        native_queued: false,
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
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    native: Option<platform::Notifier>,
    sound: Option<SoundNotifier>,
}

impl SystemChannels {
    pub(crate) fn new() -> Self {
        Self {
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            native: platform::Notifier::new().ok(),
            sound: SoundNotifier::new().ok(),
        }
    }
}

impl NotificationChannels for SystemChannels {
    fn take_native_failure(&self) -> bool {
        #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
        {
            self.native
                .as_ref()
                .is_some_and(platform::Notifier::take_failure)
        }
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        {
            false
        }
    }
    fn native(&self, title: &str, message: &str) -> bool {
        #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
        {
            self.native
                .as_ref()
                .is_some_and(|notifier| notifier.send(title, message))
        }
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
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

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeNotificationPlatform {
    #[cfg(any(test, target_os = "macos"))]
    Macos,
    #[cfg(any(test, target_os = "linux"))]
    Linux,
}

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
#[derive(Debug, Eq, PartialEq)]
struct NativeNotificationCommand {
    executable: &'static str,
    arguments: Vec<String>,
}

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
fn native_notification_command(
    platform: NativeNotificationPlatform,
    title: &str,
    message: &str,
) -> NativeNotificationCommand {
    // Notification Center and the freedesktop notification daemon retain authority over
    // quiet-time and do-not-disturb presentation; SylvOps does not schedule around them.
    match platform {
        #[cfg(any(test, target_os = "macos"))]
        NativeNotificationPlatform::Macos => NativeNotificationCommand {
            executable: native_notification_executable(platform),
            arguments: vec![
                "-e".into(),
                "on run argv\n display notification (item 2 of argv) with title (item 1 of argv)\nend run".into(),
                "--".into(),
                title.into(),
                message.into(),
            ],
        },
        #[cfg(any(test, target_os = "linux"))]
        NativeNotificationPlatform::Linux => NativeNotificationCommand {
            executable: native_notification_executable(platform),
            arguments: vec![
                "--app-name=SylvOps".into(),
                "--expire-time=10000".into(),
                "--urgency=normal".into(),
                "--".into(),
                title.into(),
                message.into(),
            ],
        },
    }
}

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
const fn native_notification_executable(platform: NativeNotificationPlatform) -> &'static str {
    match platform {
        #[cfg(any(test, target_os = "macos"))]
        NativeNotificationPlatform::Macos => "/usr/bin/osascript",
        #[cfg(any(test, target_os = "linux"))]
        NativeNotificationPlatform::Linux => "/usr/bin/notify-send",
    }
}

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeDesktopSession {
    Available,
    Unavailable,
}

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
const fn native_notification_available(
    platform: NativeNotificationPlatform,
    executable_exists: bool,
    desktop_session: NativeDesktopSession,
) -> bool {
    let _ = desktop_session;
    executable_exists
        && match platform {
            #[cfg(any(test, target_os = "macos"))]
            NativeNotificationPlatform::Macos => true,
            #[cfg(any(test, target_os = "linux"))]
            NativeNotificationPlatform::Linux => {
                matches!(desktop_session, NativeDesktopSession::Available)
            }
        }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod platform {
    use std::{
        path::Path,
        process::{Command, Stdio},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    use super::{
        NativeDesktopSession, NativeNotificationCommand, NativeNotificationPlatform,
        native_notification_available, native_notification_command, native_notification_executable,
    };

    const PROCESS_TIMEOUT: Duration = Duration::from_secs(2);

    #[derive(Debug)]
    pub(crate) struct Notifier {
        sender: mpsc::SyncSender<(String, String)>,
        failed: Arc<AtomicBool>,
        stopped: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Notifier {
        pub fn new() -> std::io::Result<Self> {
            let platform = current_platform();
            if !native_session_available(platform) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "native Attention Notifications are unavailable",
                ));
            }

            let (sender, receiver) = mpsc::sync_channel::<(String, String)>(8);
            let stopped = Arc::new(AtomicBool::new(false));
            let stop = stopped.clone();
            let failed = Arc::new(AtomicBool::new(false));
            let failure = failed.clone();
            let thread = thread::Builder::new()
                .name("sylvops-notifications".into())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        let (title, message) =
                            match receiver.recv_timeout(Duration::from_millis(100)) {
                                Ok(notification) => notification,
                                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                            };
                        let command = native_notification_command(platform, &title, &message);
                        if !run_bounded(&command, &stop) {
                            failure.store(true, Ordering::Release);
                        }
                    }
                })?;
            Ok(Self {
                sender,
                failed,
                stopped,
                thread: Some(thread),
            })
        }

        pub fn take_failure(&self) -> bool {
            self.failed.swap(false, Ordering::AcqRel)
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

    #[cfg(target_os = "macos")]
    const fn current_platform() -> NativeNotificationPlatform {
        NativeNotificationPlatform::Macos
    }

    #[cfg(target_os = "linux")]
    const fn current_platform() -> NativeNotificationPlatform {
        NativeNotificationPlatform::Linux
    }

    fn native_session_available(platform: NativeNotificationPlatform) -> bool {
        let desktop_session = if environment_present("DBUS_SESSION_BUS_ADDRESS")
            && (environment_present("DISPLAY") || environment_present("WAYLAND_DISPLAY"))
        {
            NativeDesktopSession::Available
        } else {
            NativeDesktopSession::Unavailable
        };
        native_notification_available(
            platform,
            Path::new(native_notification_executable(platform)).is_file(),
            desktop_session,
        )
    }

    fn environment_present(name: &str) -> bool {
        std::env::var_os(name).is_some_and(|value| !value.is_empty())
    }

    fn run_bounded(command: &NativeNotificationCommand, stopped: &AtomicBool) -> bool {
        let Ok(mut child) = Command::new(command.executable)
            .args(&command.arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None)
                    if !stopped.load(Ordering::Acquire) && started.elapsed() < PROCESS_TIMEOUT =>
                {
                    thread::sleep(Duration::from_millis(25));
                }
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn smoke_deliver(title: &str, message: &str) -> bool {
        let platform = current_platform();
        if !native_session_available(platform) {
            return false;
        }
        run_bounded(
            &native_notification_command(platform, title, message),
            &AtomicBool::new(false),
        )
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

    use super::{
        NativeDesktopSession, NativeNotificationPlatform, NotificationChannels,
        NotificationPresenter, native_notification_available, native_notification_command,
    };

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

    #[test]
    fn native_commands_pass_private_payload_as_arguments_without_a_shell() {
        let macos = native_notification_command(
            NativeNotificationPlatform::Macos,
            "Input requested",
            "quoted ' Session",
        );
        assert_eq!(macos.executable, "/usr/bin/osascript");
        assert_eq!(macos.arguments[0], "-e");
        assert_eq!(macos.arguments[2], "--");
        assert_eq!(macos.arguments[3], "Input requested");
        assert_eq!(macos.arguments[4], "quoted ' Session");

        let linux = native_notification_command(
            NativeNotificationPlatform::Linux,
            "Input requested",
            "quoted ' Session",
        );
        assert_eq!(linux.executable, "/usr/bin/notify-send");
        assert_eq!(
            linux.arguments,
            [
                "--app-name=SylvOps",
                "--expire-time=10000",
                "--urgency=normal",
                "--",
                "Input requested",
                "quoted ' Session",
            ]
        );
    }

    #[test]
    fn native_availability_requires_a_tool_and_linux_desktop_session() {
        assert!(native_notification_available(
            NativeNotificationPlatform::Macos,
            true,
            NativeDesktopSession::Unavailable,
        ));
        assert!(!native_notification_available(
            NativeNotificationPlatform::Macos,
            false,
            NativeDesktopSession::Available,
        ));
        assert!(native_notification_available(
            NativeNotificationPlatform::Linux,
            true,
            NativeDesktopSession::Available,
        ));
        assert!(native_notification_available(
            NativeNotificationPlatform::Linux,
            true,
            NativeDesktopSession::Available,
        ));
        assert!(!native_notification_available(
            NativeNotificationPlatform::Linux,
            true,
            NativeDesktopSession::Unavailable,
        ));
        assert!(!native_notification_available(
            NativeNotificationPlatform::Linux,
            true,
            NativeDesktopSession::Unavailable,
        ));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[ignore = "requires an interactive native desktop notification service"]
    fn native_notification_smoke_is_bounded() {
        assert!(super::platform::smoke_deliver(
            "SylvOps native smoke",
            "Bounded Attention Notification smoke test",
        ));
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
        failed: Arc<AtomicBool>,
        stopped: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Notifier {
        pub fn new() -> std::io::Result<Self> {
            let (sender, receiver) = mpsc::sync_channel::<(String, String)>(8);
            let stopped = Arc::new(AtomicBool::new(false));
            let stop = stopped.clone();
            let failed = Arc::new(AtomicBool::new(false));
            let failure = failed.clone();
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
                            failure.store(true, Ordering::Release);
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
                            failure.store(true, Ordering::Release);
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
                                        failure.store(true, Ordering::Release);
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
                failed,
                stopped,
                thread: Some(thread),
            })
        }

        pub fn take_failure(&self) -> bool {
            self.failed.swap(false, Ordering::AcqRel)
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
