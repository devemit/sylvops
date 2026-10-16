//! Presentation-only alerts for newly blocked Sessions. Never include terminal contents.

use std::collections::HashSet;
use sylvops_core::{
    domain::{Session, SessionState},
    ids::SessionId,
};

#[derive(Debug)]
pub(crate) struct AttentionTracker {
    waiting: HashSet<SessionId>,
}

impl AttentionTracker {
    pub fn new(sessions: &[Session]) -> Self {
        Self {
            waiting: waiting_sessions(sessions),
        }
    }

    pub fn update(&mut self, sessions: &[Session]) -> Option<String> {
        let waiting = waiting_sessions(sessions);
        let newly_waiting = sessions
            .iter()
            .filter(|session| waiting.contains(&session.id) && !self.waiting.contains(&session.id))
            .collect::<Vec<_>>();
        let message = match newly_waiting.as_slice() {
            [] => None,
            [session] => Some(format!(
                "{} needs your input or approval.",
                safe_label(&session.display_name)
            )),
            [first, ..] => Some(format!(
                "{} sessions need your input or approval, including {}.",
                newly_waiting.len(),
                safe_label(&first.display_name)
            )),
        };
        self.waiting = waiting;
        message
    }
}

fn waiting_sessions(sessions: &[Session]) -> HashSet<SessionId> {
    sessions
        .iter()
        .filter(|session| session.state == SessionState::NeedsFeedback)
        .map(|session| session.id)
        .collect()
}

fn safe_label(label: &str) -> String {
    let label = label
        .chars()
        .filter(|character| !character.is_control())
        .take(80)
        .collect::<String>();
    if label.trim().is_empty() {
        "A session".into()
    } else {
        label
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
        sender: mpsc::SyncSender<String>,
        stopped: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Notifier {
        pub fn new() -> std::io::Result<Self> {
            let (sender, receiver) = mpsc::sync_channel::<String>(8);
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
                                Ok(message) => {
                                    data.uFlags = NIF_INFO;
                                    data.dwInfoFlags =
                                        NIIF_INFO | NIIF_NOSOUND | NIIF_RESPECT_QUIET_TIME;
                                    copy_text(&mut data.szInfoTitle, "SylvOps · Needs your input");
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

        pub fn send(&self, message: String) -> bool {
            self.sender.try_send(message).is_ok()
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
