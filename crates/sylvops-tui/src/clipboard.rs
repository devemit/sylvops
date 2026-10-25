//! Single-owner native clipboard, isolated from terminal input and rendering.

use std::{sync::mpsc, thread};

use crate::terminal_selection::MAX_SELECTION_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CopyFailure {
    Unavailable,
    Busy,
    TooLarge,
    Failed,
}

impl CopyFailure {
    pub fn message(self) -> &'static str {
        match self {
            Self::Unavailable => "System clipboard is unavailable. Selection was kept.",
            Self::Busy => "Clipboard is busy. Try Copy again when the current copy finishes.",
            Self::TooLarge => "Terminal selection exceeds the 1 MiB clipboard limit.",
            Self::Failed => "Could not copy terminal text. Selection was kept.",
        }
    }
}

trait Backend: Send + 'static {
    fn copy(&mut self, text: String) -> Result<(), CopyFailure>;
}

struct NativeClipboard(Option<arboard::Clipboard>);

impl Backend for NativeClipboard {
    fn copy(&mut self, text: String) -> Result<(), CopyFailure> {
        if self.0.is_none() {
            self.0 = Some(arboard::Clipboard::new().map_err(|_| CopyFailure::Unavailable)?);
        }
        self.0
            .as_mut()
            .ok_or(CopyFailure::Unavailable)?
            .set_text(text)
            .map_err(|_| CopyFailure::Failed)
    }
}

#[derive(Debug)]
pub(crate) struct ClipboardService {
    requests: mpsc::SyncSender<String>,
    results: mpsc::Receiver<Result<(), CopyFailure>>,
    busy: bool,
}

impl ClipboardService {
    pub fn system() -> Result<Self, CopyFailure> {
        Self::with_backend(|| NativeClipboard(None))
    }

    fn with_backend<B: Backend>(
        factory: impl FnOnce() -> B + Send + 'static,
    ) -> Result<Self, CopyFailure> {
        let (requests, receiver) = mpsc::sync_channel(1);
        let (sender, results) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("tui-clipboard".into())
            .spawn(move || {
                // Retain the backend until the TUI closes: Linux serves clipboard data
                // from its owner, and Windows clipboard operations must be serialized.
                let mut backend = factory();
                for text in receiver {
                    if sender.try_send(backend.copy(text)).is_err() {
                        break;
                    }
                }
            })
            .map_err(|_| CopyFailure::Unavailable)?;
        Ok(Self {
            requests,
            results,
            busy: false,
        })
    }

    pub fn copy(&mut self, text: String) -> Result<(), CopyFailure> {
        if text.len() > MAX_SELECTION_BYTES {
            return Err(CopyFailure::TooLarge);
        }
        if self.busy {
            return Err(CopyFailure::Busy);
        }
        self.requests.try_send(text).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => CopyFailure::Busy,
            mpsc::TrySendError::Disconnected(_) => CopyFailure::Unavailable,
        })?;
        self.busy = true;
        Ok(())
    }

    pub fn poll(&mut self) -> Option<Result<(), CopyFailure>> {
        match self.results.try_recv() {
            Ok(result) => {
                self.busy = false;
                Some(result)
            }
            Err(mpsc::TryRecvError::Disconnected) if self.busy => {
                self.busy = false;
                Some(Err(CopyFailure::Unavailable))
            }
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    struct FakeClipboard {
        started: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
        copied: Arc<Mutex<Vec<String>>>,
        result: Result<(), CopyFailure>,
    }

    impl Backend for FakeClipboard {
        fn copy(&mut self, text: String) -> Result<(), CopyFailure> {
            self.started.send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(2)).unwrap();
            self.copied.lock().unwrap().push(text);
            self.result
        }
    }

    fn wait(service: &mut ClipboardService) -> Result<(), CopyFailure> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(result) = service.poll() {
                return result;
            }
            assert!(
                Instant::now() < deadline,
                "bounded clipboard completion timeout"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn clipboard_is_serial_bounded_and_reports_backend_results() {
        for expected in [
            Ok(()),
            Err(CopyFailure::Unavailable),
            Err(CopyFailure::Failed),
        ] {
            let (started, began) = mpsc::sync_channel(1);
            let (release, blocked) = mpsc::sync_channel(1);
            let copied = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&copied);
            let mut service = ClipboardService::with_backend(move || FakeClipboard {
                started,
                release: blocked,
                copied: recorded,
                result: expected,
            })
            .unwrap();
            assert_eq!(service.copy("selected text".into()), Ok(()));
            began.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(
                service.copy("must not be queued".into()),
                Err(CopyFailure::Busy)
            );
            release.send(()).unwrap();
            assert_eq!(wait(&mut service), expected);
            assert_eq!(*copied.lock().unwrap(), ["selected text"]);
            assert_eq!(
                service.copy("x".repeat(MAX_SELECTION_BYTES + 1)),
                Err(CopyFailure::TooLarge)
            );
            assert_eq!(copied.lock().unwrap().len(), 1);
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "opt-in native clipboard; requires an existing text clipboard and restores it"]
    fn windows_native_clipboard_copy_round_trip_restores_previous_text() {
        struct Restore(arboard::Clipboard, String);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = self.0.set_text(self.1.clone());
            }
        }
        let mut clipboard = arboard::Clipboard::new().expect("interactive Windows clipboard");
        let previous = clipboard
            .get_text()
            .expect("existing text clipboard required to restore safely");
        let mut restore = Restore(clipboard, previous);
        let mut service = ClipboardService::system().unwrap();
        service
            .copy("SylvOps clipboard smoke: 界 e\u{301}".into())
            .unwrap();
        assert!(wait(&mut service).is_ok());
        let observed = restore.0.get_text().expect("read copied test text");
        // Do not include clipboard contents in a failing assertion's diagnostics.
        let copied_as_expected = observed == "SylvOps clipboard smoke: 界 e\u{301}";
        assert!(
            copied_as_expected,
            "native clipboard text round-trip failed"
        );
    }
}
