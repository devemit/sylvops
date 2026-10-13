use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{fs, time};

use crate::forms::Form;

const MAX_SCANNED_ENTRIES: usize = 16_384;
const MAX_DIRECTORY_ENTRIES: usize = 4_096;
const MAX_FILTER_BYTES: usize = 1_024;
const DIRECTORY_IO_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DirectoryBrowserEntry {
    SelectCurrent,
    Navigate { label: String, path: PathBuf },
}

impl DirectoryBrowserEntry {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::SelectCurrent => "Select current directory".into(),
            Self::Navigate { label, .. } => label.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DirectoryBrowser {
    pub form: Form,
    pub current: PathBuf,
    pub filter: String,
    pub selected: usize,
    pub error: Option<String>,
    current_is_canonical: bool,
    directories: Vec<PathBuf>,
    roots: Vec<PathBuf>,
}

impl DirectoryBrowser {
    pub(crate) async fn new(form: Form, start: PathBuf) -> Self {
        let mut browser = Self {
            form,
            current: start.clone(),
            filter: String::new(),
            selected: 0,
            error: None,
            current_is_canonical: false,
            directories: Vec::new(),
            roots: platform_roots(),
        };
        browser.load_directory(start).await;
        browser
    }

    pub(crate) fn visible_entries(&self) -> Vec<DirectoryBrowserEntry> {
        let mut entries = Vec::new();
        if self.current_is_canonical {
            entries.push(DirectoryBrowserEntry::SelectCurrent);
        }
        if self.current_is_canonical
            && let Some(parent) = self.current.parent()
        {
            entries.push(DirectoryBrowserEntry::Navigate {
                label: format!("Parent · {}", display_path(parent)),
                path: parent.to_path_buf(),
            });
        }
        if let Some(home) = home_directory()
            && home != self.current
        {
            entries.push(DirectoryBrowserEntry::Navigate {
                label: format!("Home · {}", display_path(&home)),
                path: home,
            });
        }
        for root in &self.roots {
            if root != &self.current {
                entries.push(DirectoryBrowserEntry::Navigate {
                    label: format!("Root · {}", display_path(root)),
                    path: root.clone(),
                });
            }
        }

        let filter = self.filter.to_lowercase();
        entries.extend(
            self.directories
                .iter()
                .filter(|path| {
                    filter.is_empty() || file_name(path).to_lowercase().contains(filter.as_str())
                })
                .map(|path| DirectoryBrowserEntry::Navigate {
                    label: format!("{}/", file_name(path)),
                    path: path.clone(),
                }),
        );
        entries
    }

    pub(crate) fn selected_entry(&self) -> Option<DirectoryBrowserEntry> {
        self.visible_entries().get(self.selected).cloned()
    }

    pub(crate) fn can_select_current(&self) -> bool {
        self.current_is_canonical
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        let count = self.visible_entries().len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        self.selected = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected
                .saturating_add(delta.unsigned_abs())
                .min(count - 1)
        };
    }

    pub(crate) fn push_filter(&mut self, text: &str) {
        let remaining = MAX_FILTER_BYTES.saturating_sub(self.filter.len());
        if remaining == 0 {
            return;
        }
        let mut end = remaining.min(text.len());
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        self.filter.push_str(&text[..end]);
        self.selected = 0;
    }

    pub(crate) fn pop_filter(&mut self) {
        self.filter.pop();
        self.selected = 0;
    }

    pub(crate) async fn navigate(&mut self, path: PathBuf) {
        self.load_directory(path).await;
    }

    async fn load_directory(&mut self, path: PathBuf) {
        match time::timeout(DIRECTORY_IO_TIMEOUT, scan_directory(path.clone())).await {
            Ok(Ok((current, directories))) => {
                self.current = current;
                self.current_is_canonical = true;
                self.directories = directories;
                self.filter.clear();
                self.selected = 0;
                self.error = None;
            }
            Ok(Err(error)) => self.error = Some(crate::bounded_feedback(error)),
            Err(_) => {
                self.error = Some(format!("Timed out while reading {}.", display_path(&path)));
            }
        }
    }
}

async fn scan_directory(path: PathBuf) -> Result<(PathBuf, Vec<PathBuf>), String> {
    let current = fs::canonicalize(&path)
        .await
        .map_err(|error| format!("Cannot open {}: {error}", display_path(&path)))?;
    let metadata = fs::metadata(&current)
        .await
        .map_err(|error| format!("Cannot inspect {}: {error}", display_path(&current)))?;
    if !metadata.is_dir() {
        return Err("The selected path is not a directory.".into());
    }
    let mut entries = fs::read_dir(&current)
        .await
        .map_err(|error| format!("Cannot read {}: {error}", display_path(&current)))?;
    let mut directories = Vec::new();
    let mut scanned = 0_usize;
    loop {
        let entry = entries.next_entry().await.map_err(|error| {
            format!(
                "Cannot continue reading {}: {error}",
                display_path(&current)
            )
        })?;
        let Some(entry) = entry else {
            break;
        };
        scanned = scanned.saturating_add(1);
        if scanned > MAX_SCANNED_ENTRIES {
            return Err(format!(
                "This directory contains more than {MAX_SCANNED_ENTRIES} entries. Narrow it outside SylvOps before browsing here."
            ));
        }
        if entry
            .file_type()
            .await
            .is_ok_and(|file_type| file_type.is_dir())
        {
            let path = entry.path();
            directories.push(path);
            if directories.len() > MAX_DIRECTORY_ENTRIES {
                return Err(format!(
                    "This directory contains more than {MAX_DIRECTORY_ENTRIES} subdirectories. Narrow it outside SylvOps before browsing here."
                ));
            }
        }
    }
    directories.sort_by(|left, right| {
        let left_name = file_name_os(left);
        let right_name = file_name_os(right);
        left_name
            .to_string_lossy()
            .to_lowercase()
            .cmp(&right_name.to_string_lossy().to_lowercase())
            .then_with(|| left_name.cmp(right_name))
    });
    Ok((current, directories))
}

fn file_name(path: &Path) -> String {
    file_name_os(path).to_string_lossy().into_owned()
}

fn file_name_os(path: &Path) -> &OsStr {
    path.file_name().unwrap_or(path.as_os_str())
}

pub(crate) fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn home_directory() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .find_map(std::env::var_os)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

#[cfg(windows)]
fn platform_roots() -> Vec<PathBuf> {
    ('A'..='Z')
        .map(|drive| PathBuf::from(format!("{drive}:\\")))
        .collect()
}

#[cfg(not(windows))]
fn platform_roots() -> Vec<PathBuf> {
    vec![PathBuf::from("/")]
}
