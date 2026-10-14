use std::{
    cmp::Ordering,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{fs, time};

use crate::forms::Form;

const MAX_SCANNED_ENTRIES: usize = 16_384;
const MAX_VISIBLE_DIRECTORIES: usize = 4_096;
const MAX_FILTER_BYTES: usize = 1_024;
const DIRECTORY_IO_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectoryKind {
    Directory,
    Link,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectoryLoad {
    Navigate,
    Refresh,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ScannedDirectory {
    name: OsString,
    path: PathBuf,
    hidden: bool,
    kind: DirectoryKind,
}

#[derive(Debug)]
struct DirectoryScan {
    current: PathBuf,
    directories: Vec<ScannedDirectory>,
    read_truncated: bool,
    skipped_entries: usize,
}

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
    pub show_hidden: bool,
    current_is_selectable: bool,
    directories: Vec<ScannedDirectory>,
    roots: Vec<PathBuf>,
    read_truncated: bool,
    skipped_entries: usize,
}

impl DirectoryBrowser {
    pub(crate) async fn new(form: Form, start: PathBuf) -> Self {
        let mut browser = Self {
            form,
            current: start.clone(),
            filter: String::new(),
            selected: 0,
            error: None,
            show_hidden: false,
            current_is_selectable: false,
            directories: Vec::new(),
            roots: platform_roots(),
            read_truncated: false,
            skipped_entries: 0,
        };
        browser.load_directory(start, DirectoryLoad::Navigate).await;
        browser
    }

    pub(crate) fn visible_entries(&self) -> Vec<DirectoryBrowserEntry> {
        let mut entries = Vec::new();
        if self.current_is_selectable {
            entries.push(DirectoryBrowserEntry::SelectCurrent);
        }
        if self.current.is_absolute()
            && let Some(parent) = self.current.parent()
            && parent != self.current
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

        entries.extend(
            self.filtered_directories()
                .take(MAX_VISIBLE_DIRECTORIES)
                .map(|directory| DirectoryBrowserEntry::Navigate {
                    label: match directory.kind {
                        DirectoryKind::Directory => {
                            format!("{}/", display_name(&directory.name))
                        }
                        DirectoryKind::Link => {
                            format!("[link] {}/", display_name(&directory.name))
                        }
                    },
                    path: directory.path.clone(),
                }),
        );
        entries
    }

    pub(crate) fn selected_entry(&self) -> Option<DirectoryBrowserEntry> {
        self.visible_entries().get(self.selected).cloned()
    }

    pub(crate) fn can_select_current(&self) -> bool {
        self.current_is_selectable
    }

    pub(crate) fn truncation_notice(&self) -> Option<String> {
        let matching_directories = self.filtered_directories().count();
        if self.read_truncated {
            Some(format!(
                "Directory read stopped at the {MAX_SCANNED_ENTRIES}-entry safety limit; refine outside SylvOps or open a narrower directory."
            ))
        } else if matching_directories > MAX_VISIBLE_DIRECTORIES {
            Some(format!(
                "Showing the first {MAX_VISIBLE_DIRECTORIES} matching directories in deterministic order."
            ))
        } else {
            None
        }
    }

    pub(crate) fn scan_notice(&self) -> Option<String> {
        (self.skipped_entries > 0).then(|| {
            format!(
                "Skipped {} entr{} that changed or could not be inspected.",
                self.skipped_entries,
                if self.skipped_entries == 1 {
                    "y"
                } else {
                    "ies"
                }
            )
        })
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

    pub(crate) fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.selected = 0;
    }

    pub(crate) async fn navigate(&mut self, path: PathBuf) {
        self.load_directory(path, DirectoryLoad::Navigate).await;
    }

    pub(crate) async fn refresh(&mut self) {
        self.load_directory(self.current.clone(), DirectoryLoad::Refresh)
            .await;
    }

    fn filtered_directories(&self) -> impl Iterator<Item = &ScannedDirectory> {
        let filter = self.filter.to_lowercase();
        self.directories.iter().filter(move |directory| {
            (self.show_hidden || !directory.hidden)
                && (filter.is_empty()
                    || display_name(&directory.name)
                        .to_lowercase()
                        .contains(filter.as_str()))
        })
    }

    async fn load_directory(&mut self, path: PathBuf, load: DirectoryLoad) {
        match time::timeout(
            DIRECTORY_IO_TIMEOUT,
            scan_directory(path.clone(), MAX_SCANNED_ENTRIES),
        )
        .await
        {
            Ok(Ok(scan)) => {
                self.current = scan.current;
                self.current_is_selectable = true;
                self.directories = scan.directories;
                self.read_truncated = scan.read_truncated;
                self.skipped_entries = scan.skipped_entries;
                if load == DirectoryLoad::Navigate {
                    self.filter.clear();
                }
                self.selected = 0;
                self.error = None;
            }
            Ok(Err(error)) => self.error = Some(crate::bounded_feedback(error)),
            Err(_) => {
                self.error = Some(crate::bounded_feedback(format!(
                    "Timed out while reading {}. The last directory remains available.",
                    display_path(&path)
                )));
            }
        }
    }
}

async fn scan_directory(
    path: PathBuf,
    max_scanned_entries: usize,
) -> Result<DirectoryScan, String> {
    let current = std::path::absolute(&path).map_err(|_| {
        format!(
            "Cannot make {} absolute. Choose a platform root and try again.",
            display_path(&path)
        )
    })?;
    validate_ipc_path(&current)?;
    let inspection_root = fs::canonicalize(&current)
        .await
        .map_err(|error| format!("Cannot inspect {}: {error}", display_path(&current)))?;
    let metadata = fs::metadata(&inspection_root)
        .await
        .map_err(|error| format!("Cannot inspect {}: {error}", display_path(&current)))?;
    if !metadata.is_dir() {
        return Err("The selected path is not a directory.".into());
    }
    let mut entries = fs::read_dir(&inspection_root)
        .await
        .map_err(|error| format!("Cannot read {}: {error}", display_path(&current)))?;
    let mut directories = Vec::new();
    let mut scanned = 0_usize;
    let mut skipped_entries = 0_usize;
    let mut read_truncated = false;
    loop {
        if scanned == max_scanned_entries {
            read_truncated = entries
                .next_entry()
                .await
                .map_err(|error| {
                    format!(
                        "Cannot continue reading {}: {error}",
                        display_path(&current)
                    )
                })?
                .is_some();
            if read_truncated {
                directories.clear();
            }
            break;
        }
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
        let inspected_path = entry.path();
        let name = entry.file_name();
        let path = current.join(&name);
        let Ok(link_metadata) = fs::symlink_metadata(&inspected_path).await else {
            skipped_entries = skipped_entries.saturating_add(1);
            continue;
        };
        let is_link = is_link_or_reparse_point(&link_metadata);
        let Ok(is_directory) = directory_target_is_directory(&link_metadata, &inspected_path).await
        else {
            skipped_entries = skipped_entries.saturating_add(1);
            continue;
        };
        if !is_directory {
            continue;
        }
        directories.push(ScannedDirectory {
            hidden: is_hidden(&name, &link_metadata),
            name,
            path,
            kind: if is_link {
                DirectoryKind::Link
            } else {
                DirectoryKind::Directory
            },
        });
    }
    directories.sort_by(compare_directories);
    Ok(DirectoryScan {
        current,
        directories,
        read_truncated,
        skipped_entries,
    })
}

fn compare_directories(left: &ScannedDirectory, right: &ScannedDirectory) -> Ordering {
    display_name(&left.name)
        .to_lowercase()
        .cmp(&display_name(&right.name).to_lowercase())
        .then_with(|| left.name.cmp(&right.name))
        .then_with(|| left.path.cmp(&right.path))
}

async fn directory_target_is_directory(
    link_metadata: &std::fs::Metadata,
    path: &Path,
) -> Result<bool, ()> {
    if !is_link_or_reparse_point(link_metadata) {
        return Ok(link_metadata.is_dir());
    }
    fs::metadata(path)
        .await
        .map(|target| target.is_dir())
        .map_err(|_| ())
}

fn validate_ipc_path(path: &Path) -> Result<(), String> {
    let Some(path) = path.to_str() else {
        return Err(
            "This directory path cannot be represented by the string-based IPC path format. Choose a UTF-8 path instead."
                .into(),
        );
    };
    if path.len() > crate::MAX_FORM_VALUE_BYTES {
        return Err(format!(
            "This directory path exceeds the {}-byte form and IPC safety limit. Choose a shorter path.",
            crate::MAX_FORM_VALUE_BYTES
        ));
    }
    Ok(())
}

fn display_name(name: &OsStr) -> String {
    name.to_string_lossy().into_owned()
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

#[cfg(unix)]
fn is_hidden(name: &OsStr, _metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::ffi::OsStrExt;

    name.as_bytes().starts_with(b".")
}

#[cfg(windows)]
fn is_hidden(name: &OsStr, metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    name.to_string_lossy().starts_with('.')
        || metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
}

#[cfg(not(any(unix, windows)))]
fn is_hidden(name: &OsStr, _metadata: &std::fs::Metadata) -> bool {
    name.to_string_lossy().starts_with('.')
}

#[cfg(windows)]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn platform_roots() -> Vec<PathBuf> {
    // SAFETY: GetLogicalDrives takes no pointers or caller-owned buffers and returns a bitmask.
    let logical_drives = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    (b'A'..=b'Z')
        .enumerate()
        .filter(|(index, _)| logical_drives & (1_u32 << index) != 0)
        .map(|(_, drive)| PathBuf::from(format!("{}:\\", char::from(drive))))
        .collect()
}

#[cfg(not(windows))]
fn platform_roots() -> Vec<PathBuf> {
    vec![PathBuf::from("/")]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as sync_fs;
    use sylvops_core::ids::WorkspaceId;

    fn form() -> Form {
        Form::repository(WorkspaceId::new())
    }

    fn child_labels(browser: &DirectoryBrowser) -> Vec<String> {
        browser
            .visible_entries()
            .into_iter()
            .map(|entry| entry.label())
            .filter(|label| !label.contains(" · ") && label.ends_with('/'))
            .collect()
    }

    #[tokio::test]
    async fn hidden_filtering_order_refresh_and_failure_recovery_are_stable() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        for name in ["zeta", ".hidden", "Alpha"] {
            sync_fs::create_dir(temporary.path().join(name)).expect("directory");
        }
        let mut browser = DirectoryBrowser::new(form(), temporary.path().into()).await;

        assert_eq!(child_labels(&browser), ["Alpha/", "zeta/"]);
        browser.toggle_hidden();
        assert_eq!(child_labels(&browser), [".hidden/", "Alpha/", "zeta/"]);
        browser.push_filter("a");
        assert_eq!(child_labels(&browser), ["Alpha/", "zeta/"]);
        browser.refresh().await;
        assert_eq!(browser.filter, "a");
        assert!(browser.show_hidden);
        assert_eq!(child_labels(&browser), ["Alpha/", "zeta/"]);

        let previous = browser.current.clone();
        browser.navigate(temporary.path().join("removed")).await;
        assert_eq!(browser.current, previous);
        assert!(browser.can_select_current());
        assert!(
            browser
                .error
                .as_deref()
                .is_some_and(|error| error.contains("Cannot inspect"))
        );
    }

    #[tokio::test]
    async fn directory_links_are_marked_and_preserve_the_absolute_link_path() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let target = temporary.path().join("target");
        let link = temporary.path().join("linked");
        sync_fs::create_dir(&target).expect("target directory");
        create_directory_link(&target, &link).expect("directory link");
        let mut browser = DirectoryBrowser::new(form(), temporary.path().into()).await;
        let entry = browser
            .visible_entries()
            .into_iter()
            .find(|entry| entry.label().contains("linked"))
            .expect("linked directory entry");
        assert!(entry.label().starts_with("[link]"));
        let DirectoryBrowserEntry::Navigate { path, .. } = entry else {
            panic!("link must navigate");
        };
        browser.navigate(path).await;
        assert_eq!(browser.current, std::path::absolute(&link).unwrap());
        assert_ne!(browser.current, sync_fs::canonicalize(&link).unwrap());
        assert!(browser.can_select_current());
        remove_directory_link(&link).expect("remove directory link");
    }

    #[test]
    fn truncation_is_deterministic_and_filter_input_stays_on_utf8_boundaries() {
        let mut directories = (0..=MAX_VISIBLE_DIRECTORIES)
            .rev()
            .map(|index| ScannedDirectory {
                name: format!("entry-{index:05}").into(),
                path: PathBuf::from(format!("/entry-{index:05}")),
                hidden: false,
                kind: DirectoryKind::Directory,
            })
            .collect::<Vec<_>>();
        directories.sort_by(compare_directories);
        assert_eq!(
            directories
                .iter()
                .take(3)
                .map(|entry| display_name(&entry.name))
                .collect::<Vec<_>>(),
            ["entry-00000", "entry-00001", "entry-00002"]
        );

        let mut browser = DirectoryBrowser {
            form: form(),
            current: PathBuf::from("/"),
            filter: String::new(),
            selected: 0,
            error: None,
            show_hidden: false,
            current_is_selectable: true,
            directories,
            roots: Vec::new(),
            read_truncated: false,
            skipped_entries: 0,
        };
        assert_eq!(child_labels(&browser).len(), MAX_VISIBLE_DIRECTORIES);
        assert!(
            browser
                .truncation_notice()
                .as_deref()
                .is_some_and(|notice| notice.contains("deterministic order"))
        );
        browser.filter = "a".repeat(MAX_FILTER_BYTES - 1);
        browser.push_filter("éx");
        assert_eq!(browser.filter.len(), MAX_FILTER_BYTES - 1);
        assert!(browser.filter.is_char_boundary(browser.filter.len()));
    }

    #[tokio::test]
    async fn filesystem_scan_bound_truncates_without_an_order_dependent_subset() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        for name in ["delta", "charlie", "bravo", "alpha"] {
            sync_fs::create_dir(temporary.path().join(name)).expect("directory");
        }

        let scan = scan_directory(temporary.path().into(), 3)
            .await
            .expect("bounded scan");
        assert!(scan.read_truncated);
        assert_eq!(scan.directories, []);
    }

    #[tokio::test]
    async fn target_removed_during_link_inspection_is_reported_inline() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let target = temporary.path().join("target");
        let link = temporary.path().join("linked");
        sync_fs::create_dir(&target).expect("target directory");
        create_directory_link(&target, &link).expect("directory link");
        let link_metadata = sync_fs::symlink_metadata(&link).expect("link metadata");
        sync_fs::remove_dir(&target).expect("remove target between metadata reads");
        let inspection = directory_target_is_directory(&link_metadata, &link).await;
        assert_eq!(inspection, Err(()));

        let browser = DirectoryBrowser::new(form(), temporary.path().into()).await;
        assert!(
            browser
                .scan_notice()
                .as_deref()
                .is_some_and(|notice| notice.contains("could not be inspected"))
        );
        assert!(
            !child_labels(&browser)
                .iter()
                .any(|label| label.contains("linked"))
        );
        remove_directory_link(&link).expect("remove directory link");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn non_utf8_directory_names_are_rejected_without_losing_the_parent() {
        use std::os::unix::ffi::OsStringExt;

        let temporary = tempfile::tempdir().expect("temporary directory");
        let name = OsString::from_vec(vec![b'n', b'o', b'n', 0xff]);
        let path = temporary.path().join(name);
        let mut browser = DirectoryBrowser::new(form(), temporary.path().into()).await;
        let parent = browser.current.clone();
        browser.navigate(path).await;
        assert_eq!(browser.current, parent);
        assert!(
            browser
                .error
                .as_deref()
                .is_some_and(|error| error.contains("string-based IPC"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_directory_failure_keeps_the_browser_usable_when_enforced() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().expect("temporary directory");
        let unreadable = temporary.path().join("unreadable");
        sync_fs::create_dir(&unreadable).expect("unreadable directory");
        sync_fs::set_permissions(&unreadable, sync_fs::Permissions::from_mode(0o000))
            .expect("remove permissions");
        let mut browser = DirectoryBrowser::new(form(), temporary.path().into()).await;
        let parent = browser.current.clone();
        browser.navigate(unreadable.clone()).await;
        sync_fs::set_permissions(&unreadable, sync_fs::Permissions::from_mode(0o700))
            .expect("restore permissions");
        if browser.error.is_some() {
            assert_eq!(browser.current, parent);
            assert!(browser.can_select_current());
        }
    }

    #[cfg(unix)]
    fn create_directory_link(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_directory_link(target: &Path, link: &Path) -> std::io::Result<()> {
        junction::create(target, link)
    }

    #[cfg(unix)]
    fn remove_directory_link(link: &Path) -> std::io::Result<()> {
        sync_fs::remove_file(link)
    }

    #[cfg(windows)]
    fn remove_directory_link(link: &Path) -> std::io::Result<()> {
        junction::delete(link)
    }

    #[cfg(unix)]
    #[test]
    fn unix_root_has_no_parent_and_uses_the_unix_separator() {
        assert_eq!(platform_roots(), [PathBuf::from("/")]);
        assert!(Path::new("/").parent().is_none());
        assert_eq!(Path::new("/").join("child"), PathBuf::from("/child"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_roots_include_the_current_volume_and_use_windows_separators() {
        let roots = platform_roots();
        let current = std::env::current_dir().unwrap();
        assert!(roots.iter().any(|root| current.starts_with(root)));
        assert!(roots.iter().all(|root| root.is_absolute()));
        assert!(Path::new(r"C:\").parent().is_none());
        assert_eq!(
            Path::new(r"D:\parent").join("child"),
            PathBuf::from(r"D:\parent\child")
        );
    }
}
