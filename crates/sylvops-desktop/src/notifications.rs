//! Bounded, presentation-only Attention Notifications.

use sylvops_core::{
    attention::{AttentionNotificationClaim, MAX_ATTENTION_NOTIFICATION_LABEL_CHARS},
    ids::SessionId,
    session_state::AttentionReason,
};

const MAX_NOTIFICATION_TITLE_CHARS: usize = 48;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotificationPayload {
    pub session_id: SessionId,
    pub reason: AttentionReason,
    pub title: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InClientNotification {
    pub title: String,
    pub message: String,
}

impl From<NotificationPayload> for InClientNotification {
    fn from(payload: NotificationPayload) -> Self {
        Self {
            title: payload.title,
            message: payload.message,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NotificationDelivery {
    Disabled,
    Native,
    InClient(InClientNotification),
}

pub(crate) trait NotificationChannels: std::fmt::Debug {
    fn native(&self, payload: &NotificationPayload) -> bool;
    fn sound(&self) -> bool;
    fn take_activation(&self) -> Option<SessionId>;
    fn take_native_failure(&self) -> Option<NotificationPayload> {
        None
    }
}

#[derive(Debug)]
pub(crate) struct AttentionNotificationAdapter {
    channels: Box<dyn NotificationChannels>,
}

impl AttentionNotificationAdapter {
    pub(crate) fn system() -> Self {
        Self::new(SystemChannels::new())
    }

    pub(crate) fn new(channels: impl NotificationChannels + 'static) -> Self {
        Self {
            channels: Box::new(channels),
        }
    }

    pub(crate) fn deliver(&self, claim: &AttentionNotificationClaim) -> NotificationDelivery {
        if claim.preferences.sound_enabled {
            let _ = self.channels.sound();
        }
        if !claim.preferences.native_notifications_enabled {
            return NotificationDelivery::Disabled;
        }
        let payload = presentation(claim);
        if self.channels.native(&payload) {
            NotificationDelivery::Native
        } else {
            NotificationDelivery::InClient(payload.into())
        }
    }

    pub(crate) fn take_activation(&self) -> Option<SessionId> {
        self.channels.take_activation()
    }

    pub(crate) fn take_fallback(&self) -> Option<InClientNotification> {
        self.channels
            .take_native_failure()
            .map(InClientNotification::from)
    }
}

fn presentation(claim: &AttentionNotificationClaim) -> NotificationPayload {
    let title = match claim.reason {
        AttentionReason::InputRequested => "Input requested",
        AttentionReason::SuccessfulCompletion => "Session completed",
        AttentionReason::RecoverableFailure => "Recoverable failure",
        AttentionReason::TerminalFailure => "Session failed",
    };
    NotificationPayload {
        session_id: claim.session_id,
        reason: claim.reason,
        title: title.chars().take(MAX_NOTIFICATION_TITLE_CHARS).collect(),
        message: safe_label(&claim.session_display_name),
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
struct SystemChannels {
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    native: Option<platform::Notifier>,
    sound: Option<SoundNotifier>,
}

impl SystemChannels {
    fn new() -> Self {
        Self {
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            native: platform::Notifier::new().ok(),
            sound: SoundNotifier::new().ok(),
        }
    }
}

impl NotificationChannels for SystemChannels {
    fn native(&self, payload: &NotificationPayload) -> bool {
        #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
        {
            self.native
                .as_ref()
                .is_some_and(|notifier| notifier.send(payload))
        }
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        {
            let _ = payload;
            false
        }
    }

    fn sound(&self) -> bool {
        self.sound.as_ref().is_some_and(SoundNotifier::notify)
    }

    fn take_activation(&self) -> Option<SessionId> {
        #[cfg(windows)]
        {
            self.native
                .as_ref()
                .and_then(platform::Notifier::take_activation)
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    fn take_native_failure(&self) -> Option<NotificationPayload> {
        #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
        {
            self.native
                .as_ref()
                .and_then(platform::Notifier::take_failure)
        }
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        {
            None
        }
    }
}

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

    pub fn notify(&self) -> bool {
        self.sender.try_send(()).is_ok()
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
    payload: &NotificationPayload,
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
                payload.title.clone(),
                payload.message.clone(),
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
                payload.title.clone(),
                payload.message.clone(),
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
        NotificationPayload, native_notification_available, native_notification_command,
        native_notification_executable,
    };

    const PROCESS_TIMEOUT: Duration = Duration::from_secs(2);

    #[derive(Debug)]
    pub(super) struct Notifier {
        sender: mpsc::SyncSender<NotificationPayload>,
        failures: mpsc::Receiver<NotificationPayload>,
        stopped: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Notifier {
        pub(super) fn new() -> std::io::Result<Self> {
            let platform = current_platform();
            if !native_session_available(platform) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "native Attention Notifications are unavailable",
                ));
            }

            let (sender, receiver) = mpsc::sync_channel::<NotificationPayload>(8);
            let (failure_sender, failures) = mpsc::sync_channel(9);
            let stopped = Arc::new(AtomicBool::new(false));
            let stop = stopped.clone();
            let thread = thread::Builder::new()
                .name("sylvops-notifications".into())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        let payload = match receiver.recv_timeout(Duration::from_millis(100)) {
                            Ok(payload) => payload,
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        };
                        let command = native_notification_command(platform, &payload);
                        if !run_bounded(&command, &stop) && !stop.load(Ordering::Acquire) {
                            let _ = failure_sender.try_send(payload);
                        }
                    }
                })?;
            Ok(Self {
                sender,
                failures,
                stopped,
                thread: Some(thread),
            })
        }

        pub(super) fn send(&self, payload: &NotificationPayload) -> bool {
            self.sender.try_send(payload.clone()).is_ok()
        }

        pub(super) fn take_failure(&self) -> Option<NotificationPayload> {
            self.failures.try_recv().ok()
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
    pub(super) fn smoke_deliver(payload: &NotificationPayload) -> bool {
        let platform = current_platform();
        if !native_session_available(platform) {
            return false;
        }
        run_bounded(
            &native_notification_command(platform, payload),
            &AtomicBool::new(false),
        )
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    use sylvops_core::ids::SessionId;
    use windows_sys::{
        Win32::UI::{
            Shell::{
                NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_NOSOUND,
                NIIF_RESPECT_QUIET_TIME, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIN_BALLOONHIDE,
                NIN_BALLOONTIMEOUT, NIN_BALLOONUSERCLICK, NOTIFYICONDATAW, Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, DispatchMessageW, HWND_MESSAGE, IDI_INFORMATION,
                LoadIconW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage, WM_APP,
            },
        },
        core::w,
    };

    use super::NotificationPayload;

    const NOTIFICATION_CALLBACK_MESSAGE: u32 = WM_APP + 1;

    #[derive(Debug)]
    pub(super) struct Notifier {
        sender: mpsc::SyncSender<NotificationPayload>,
        activations: mpsc::Receiver<SessionId>,
        failures: mpsc::Receiver<NotificationPayload>,
        stopped: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Notifier {
        pub(super) fn new() -> std::io::Result<Self> {
            let (sender, receiver) = mpsc::sync_channel::<NotificationPayload>(8);
            let (activation_sender, activations) = mpsc::sync_channel(8);
            let (failure_sender, failures) = mpsc::sync_channel(9);
            let (initialized_sender, initialized) = mpsc::sync_channel(1);
            let stopped = Arc::new(AtomicBool::new(false));
            let stop = stopped.clone();
            let thread = thread::Builder::new()
                .name("sylvops-notifications".into())
                .spawn(move || {
                    run_notification_loop(
                        &receiver,
                        &activation_sender,
                        &failure_sender,
                        &initialized_sender,
                        &stop,
                    );
                })?;
            if initialized.recv_timeout(Duration::from_secs(2)) != Ok(true) {
                stopped.store(true, Ordering::Release);
                let _ = thread.join();
                return Err(std::io::Error::other(
                    "native Attention Notifications are unavailable",
                ));
            }
            Ok(Self {
                sender,
                activations,
                failures,
                stopped,
                thread: Some(thread),
            })
        }

        pub(super) fn send(&self, payload: &NotificationPayload) -> bool {
            self.sender.try_send(payload.clone()).is_ok()
        }

        pub(super) fn take_activation(&self) -> Option<SessionId> {
            self.activations.try_recv().ok()
        }

        pub(super) fn take_failure(&self) -> Option<NotificationPayload> {
            self.failures.try_recv().ok()
        }
    }

    #[allow(clippy::too_many_lines)] // Win32 handles and pumping stay on one owning thread.
    fn run_notification_loop(
        receiver: &mpsc::Receiver<NotificationPayload>,
        activation_sender: &mpsc::SyncSender<SessionId>,
        failure_sender: &mpsc::SyncSender<NotificationPayload>,
        initialized_sender: &mpsc::SyncSender<bool>,
        stop: &AtomicBool,
    ) {
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
                let _ = initialized_sender.try_send(false);
                return;
            }
            let mut data = NOTIFYICONDATAW {
                cbSize: u32::try_from(std::mem::size_of::<NOTIFYICONDATAW>()).unwrap_or_default(),
                hWnd: window,
                uID: 1,
                uFlags: NIF_ICON | NIF_TIP | NIF_MESSAGE,
                uCallbackMessage: NOTIFICATION_CALLBACK_MESSAGE,
                hIcon: LoadIconW(std::ptr::null_mut(), IDI_INFORMATION),
                ..Default::default()
            };
            copy_text(&mut data.szTip, "SylvOps Attention Notifications");
            if Shell_NotifyIconW(NIM_ADD, &raw const data) == 0 {
                let _ = initialized_sender.try_send(false);
                DestroyWindow(window);
                return;
            }
            let _ = initialized_sender.try_send(true);
            let mut active_session_id = None;
            let mut active_since = Instant::now();
            while !stop.load(Ordering::Acquire) {
                // Explorer can omit balloon callbacks (quiet time, restart, or policy).
                // Bound ownership so one missing callback cannot stall the delivery queue.
                if active_since.elapsed() >= Duration::from_secs(15) {
                    active_session_id = None;
                }
                let next = if active_session_id.is_none() {
                    match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(payload) => Some(payload),
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                    }
                } else {
                    thread::sleep(Duration::from_millis(100));
                    None
                };
                let mut message = MSG::default();
                while PeekMessageW(&raw mut message, window, 0, 0, PM_REMOVE) != 0 {
                    if message.message == NOTIFICATION_CALLBACK_MESSAGE
                        && u32::try_from(message.wParam).ok() == Some(data.uID)
                    {
                        let callback = u32::try_from(message.lParam).ok();
                        if callback == Some(NIN_BALLOONUSERCLICK)
                            && let Some(session_id) = active_session_id
                        {
                            let _ = activation_sender.try_send(session_id);
                        }
                        if matches!(
                            callback,
                            Some(NIN_BALLOONHIDE | NIN_BALLOONTIMEOUT | NIN_BALLOONUSERCLICK)
                        ) {
                            active_session_id = None;
                        }
                    }
                    TranslateMessage(&raw const message);
                    DispatchMessageW(&raw const message);
                }
                if active_session_id.is_none()
                    && let Some(payload) = next
                {
                    // A fresh icon ID binds callbacks to this delivery. Late callbacks for a
                    // timed-out/deleted balloon must never activate the next Session.
                    Shell_NotifyIconW(NIM_DELETE, &raw const data);
                    let Some(next_id) = data.uID.checked_add(1) else {
                        let _ = failure_sender.try_send(payload);
                        break;
                    };
                    data.uID = next_id;
                    data.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE;
                    if Shell_NotifyIconW(NIM_ADD, &raw const data) == 0 {
                        let _ = failure_sender.try_send(payload);
                        for pending in receiver.try_iter() {
                            let _ = failure_sender.try_send(pending);
                        }
                        break;
                    }
                    data.uFlags = NIF_INFO;
                    data.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND | NIIF_RESPECT_QUIET_TIME;
                    copy_text(&mut data.szInfoTitle, &payload.title);
                    copy_text(&mut data.szInfo, &payload.message);
                    if Shell_NotifyIconW(NIM_MODIFY, &raw const data) == 0 {
                        let _ = failure_sender.try_send(payload);
                        for pending_payload in receiver.try_iter() {
                            let _ = failure_sender.try_send(pending_payload);
                        }
                        break;
                    }
                    active_session_id = Some(payload.session_id);
                    active_since = Instant::now();
                }
            }
            Shell_NotifyIconW(NIM_DELETE, &raw const data);
            DestroyWindow(window);
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

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use sylvops_core::{
        attention::{AttentionNotificationClaim, AttentionNotificationPreferences},
        ids::SessionId,
        session_state::{AttentionEventId, AttentionReason},
    };

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an interactive Windows Explorer notification service"]
    fn windows_native_notification_smoke_is_bounded() {
        let notifier = super::platform::Notifier::new().expect("native notification service");
        let payload = super::presentation(&AttentionNotificationClaim::test(
            AttentionNotificationPreferences::default(),
        ));
        assert!(notifier.send(&payload));
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(notifier.take_failure().is_none());
        // Acceptance by Explorer does not prove visible display: OS policy remains authoritative.
    }

    use super::{
        AttentionNotificationAdapter, NativeDesktopSession, NativeNotificationPlatform,
        NotificationChannels, NotificationDelivery, NotificationPayload,
        native_notification_available, native_notification_command,
    };

    #[derive(Debug, Default)]
    struct RecordingChannels {
        native: Mutex<Vec<NotificationPayload>>,
        sounds: Mutex<usize>,
        activations: Mutex<VecDeque<SessionId>>,
        failures: Mutex<VecDeque<NotificationPayload>>,
        native_succeeds: Mutex<bool>,
    }

    impl RecordingChannels {
        fn succeeding() -> Arc<Self> {
            Arc::new(Self {
                native_succeeds: Mutex::new(true),
                ..Self::default()
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self::default())
        }
    }

    impl NotificationChannels for Arc<RecordingChannels> {
        fn native(&self, payload: &NotificationPayload) -> bool {
            self.native.lock().unwrap().push(payload.clone());
            *self.native_succeeds.lock().unwrap()
        }

        fn sound(&self) -> bool {
            *self.sounds.lock().unwrap() += 1;
            true
        }

        fn take_activation(&self) -> Option<SessionId> {
            self.activations.lock().unwrap().pop_front()
        }

        fn take_native_failure(&self) -> Option<NotificationPayload> {
            self.failures.lock().unwrap().pop_front()
        }
    }

    fn claim(reason: AttentionReason) -> AttentionNotificationClaim {
        AttentionNotificationClaim {
            session_id: SessionId::new(),
            attention_id: AttentionEventId::try_from(1).unwrap(),
            reason,
            session_display_name: "User-selected Session".into(),
            preferences: AttentionNotificationPreferences::default(),
        }
    }

    #[test]
    fn payload_maps_every_reason_without_untrusted_session_context() {
        let channels = RecordingChannels::succeeding();
        let adapter = AttentionNotificationAdapter::new(channels.clone());
        let reasons = [
            (AttentionReason::InputRequested, "Input requested"),
            (AttentionReason::SuccessfulCompletion, "Session completed"),
            (AttentionReason::RecoverableFailure, "Recoverable failure"),
            (AttentionReason::TerminalFailure, "Session failed"),
        ];

        for (reason, expected_title) in reasons {
            assert_eq!(
                adapter.deliver(&claim(reason)),
                NotificationDelivery::Native
            );
            let payload = channels.native.lock().unwrap().last().unwrap().clone();
            assert_eq!(payload.title, expected_title);
            assert_eq!(payload.message, "User-selected Session");
            assert!(payload.title.chars().count() <= 48);
            assert!(payload.message.chars().count() <= 80);
            for forbidden in [
                "provider prompt",
                "terminal output",
                "raw error",
                "C:/repository/path",
                "Issue #177",
            ] {
                assert!(!payload.title.contains(forbidden));
                assert!(!payload.message.contains(forbidden));
            }
        }
    }

    #[test]
    fn notification_and_sound_preferences_are_independent() {
        let channels = RecordingChannels::succeeding();
        let adapter = AttentionNotificationAdapter::new(channels.clone());
        let mut native_only = claim(AttentionReason::InputRequested);
        native_only.preferences.sound_enabled = false;
        assert_eq!(adapter.deliver(&native_only), NotificationDelivery::Native);
        assert_eq!(channels.native.lock().unwrap().len(), 1);
        assert_eq!(*channels.sounds.lock().unwrap(), 0);

        let mut sound_only = claim(AttentionReason::SuccessfulCompletion);
        sound_only.preferences.native_notifications_enabled = false;
        assert_eq!(adapter.deliver(&sound_only), NotificationDelivery::Disabled);
        assert_eq!(channels.native.lock().unwrap().len(), 1);
        assert_eq!(*channels.sounds.lock().unwrap(), 1);
    }

    #[test]
    fn failed_native_presentation_returns_bounded_in_client_fallback() {
        let channels = RecordingChannels::failing();
        let adapter = AttentionNotificationAdapter::new(channels);

        let NotificationDelivery::InClient(fallback) =
            adapter.deliver(&claim(AttentionReason::RecoverableFailure))
        else {
            panic!("failed native delivery must fall back in-client");
        };
        assert_eq!(fallback.title, "Recoverable failure");
        assert_eq!(fallback.message, "User-selected Session");
    }

    #[test]
    fn delayed_native_failure_returns_the_same_safe_in_client_fallback() {
        let channels = RecordingChannels::succeeding();
        let adapter = AttentionNotificationAdapter::new(channels.clone());
        assert_eq!(
            adapter.deliver(&claim(AttentionReason::TerminalFailure)),
            NotificationDelivery::Native
        );
        channels
            .failures
            .lock()
            .unwrap()
            .push_back(channels.native.lock().unwrap().last().unwrap().clone());

        let fallback = adapter.take_fallback().unwrap();
        assert_eq!(fallback.title, "Session failed");
        assert_eq!(fallback.message, "User-selected Session");
    }

    #[test]
    fn native_activation_returns_only_the_claimed_session_identity() {
        let channels = RecordingChannels::succeeding();
        let adapter = AttentionNotificationAdapter::new(channels.clone());
        let claim = claim(AttentionReason::TerminalFailure);
        channels
            .activations
            .lock()
            .unwrap()
            .push_back(claim.session_id);

        assert_eq!(adapter.take_activation(), Some(claim.session_id));
        assert_eq!(adapter.take_activation(), None);
    }

    #[test]
    fn native_commands_pass_private_payload_as_arguments_without_a_shell() {
        let payload = NotificationPayload {
            session_id: SessionId::new(),
            reason: AttentionReason::InputRequested,
            title: "Input requested".into(),
            message: "quoted ' Session".into(),
        };

        let macos = native_notification_command(NativeNotificationPlatform::Macos, &payload);
        assert_eq!(macos.executable, "/usr/bin/osascript");
        assert_eq!(macos.arguments[0], "-e");
        assert_eq!(macos.arguments[2], "--");
        assert_eq!(macos.arguments[3], payload.title);
        assert_eq!(macos.arguments[4], payload.message);

        let linux = native_notification_command(NativeNotificationPlatform::Linux, &payload);
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
        let payload = NotificationPayload {
            session_id: SessionId::new(),
            reason: AttentionReason::InputRequested,
            title: "SylvOps native smoke".into(),
            message: "Bounded Attention Notification smoke test".into(),
        };

        assert!(super::platform::smoke_deliver(&payload));
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
