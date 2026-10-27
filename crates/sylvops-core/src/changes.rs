//! Bounded local-change review shared by disposable clients.

use crate::{diff::DiffLineKind, ids::WorktreeId};
use serde::{Deserialize, Serialize};

pub const MAX_CHANGE_FILES: usize = 2_000;
pub const MAX_FILE_BYTES: usize = 256 * 1024;
pub const MAX_DIFF_LINES: usize = 4_000;
pub const MAX_LINE_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum ChangeGroup {
    Unstaged,
    Staged,
    Untracked,
}

impl ChangeGroup {
    pub const ALL: [Self; 3] = [Self::Unstaged, Self::Staged, Self::Untracked];
    pub fn label(self) -> &'static str {
        match self {
            Self::Unstaged => "Unstaged",
            Self::Staged => "Staged",
            Self::Untracked => "Untracked",
        }
    }
}

/// Git's lossless repository-relative bytes, never a display string or a pathspec.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangeKey {
    pub group: ChangeGroup,
    pub path: Vec<u8>,
}

impl ChangeKey {
    pub fn valid(&self) -> bool {
        !self.path.is_empty()
            && self.path.len() <= 4096
            && !self.path.contains(&0)
            && !self.path.contains(&b'\\')
            && !self.path.contains(&b':')
            && self.path.split(|b| *b == b'/').all(|p| {
                !p.is_empty() && p != b"." && p != b".." && !p.eq_ignore_ascii_case(b".git")
            })
    }
    pub fn display_path(&self) -> String {
        display_path(&self.path)
    }
}

pub fn display_path(path: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut display = String::new();
    for chunk in path.utf8_chunks() {
        for character in chunk.valid().chars() {
            if character.is_control() {
                display.extend(character.escape_default());
            } else {
                display.push(character);
            }
        }
        for byte in chunk.invalid() {
            let _ = write!(display, "\\x{byte:02x}");
        }
    }
    display
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub key: ChangeKey,
    pub status: ChangeStatus,
    pub old_path: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckoutChanges {
    pub worktree_id: WorktreeId,
    pub files: Vec<ChangedFile>,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ChangeStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Conflicted,
    Untracked,
}

impl std::fmt::Display for ChangeStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Modified => "modified",
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Copied => "copied",
            Self::TypeChanged => "type changed",
            Self::Conflicted => "conflicted",
            Self::Untracked => "untracked",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangeLine {
    pub kind: DiffLineKind,
    pub text: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
}

/// Byte ranges with paired light/dark syntax colors; clients own final styling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyntaxToken {
    pub start: usize,
    pub end: usize,
    pub dark: [u8; 3],
    pub light: [u8; 3],
}

fn syntax_tokens(file: &FileChange) -> Vec<Vec<SyntaxToken>> {
    use std::sync::LazyLock;
    use syntect::{easy::HighlightLines, highlighting::ThemeSet, parsing::SyntaxSet};
    static SYNTAX: LazyLock<SyntaxSet> = LazyLock::new(|| {
        let mut builder = SyntaxSet::load_defaults_newlines().into_builder();
        for source in [
            include_str!("../assets/TOML.sublime-syntax"),
            include_str!("../assets/TypeScript.sublime-syntax"),
        ] {
            if let Ok(syntax) =
                syntect::parsing::SyntaxDefinition::load_from_str(source, true, None)
            {
                builder.add(syntax);
            }
        }
        builder.build()
    });
    static THEMES: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);
    let path = file.key.display_path();
    let extension = path.rsplit('.').next().unwrap_or("");
    let extension = match extension {
        "jsx" => "js",
        value => value,
    };
    let Some(syntax) = SYNTAX.find_syntax_by_extension(extension) else {
        return vec![Vec::new(); file.lines.len()];
    };
    let mut dark: [_; 2] =
        std::array::from_fn(|_| HighlightLines::new(syntax, &THEMES.themes["base16-ocean.dark"]));
    let mut light: [_; 2] =
        std::array::from_fn(|_| HighlightLines::new(syntax, &THEMES.themes["InspiredGitHub"]));
    let mut budget = 16_000_usize;
    file.lines
        .iter()
        .map(|line| {
            if line.kind == DiffLineKind::HunkHeader {
                dark = std::array::from_fn(|_| {
                    HighlightLines::new(syntax, &THEMES.themes["base16-ocean.dark"])
                });
                light = std::array::from_fn(|_| {
                    HighlightLines::new(syntax, &THEMES.themes["InspiredGitHub"])
                });
            }
            if !matches!(
                line.kind,
                DiffLineKind::Addition | DiffLineKind::Deletion | DiffLineKind::Context
            ) || budget == 0
                || line.text.len() > 2048
            {
                return Vec::new();
            }
            let prefix = usize::from(line.text.starts_with(['+', '-', ' ']));
            let content = format!("{}\n", &line.text[prefix..]);
            let side = usize::from(line.kind != DiffLineKind::Deletion);
            if line.kind == DiffLineKind::Context {
                let _ = dark[0].highlight_line(&content, &SYNTAX);
                let _ = light[0].highlight_line(&content, &SYNTAX);
            }
            let Ok(dark_spans) = dark[side].highlight_line(&content, &SYNTAX) else {
                return Vec::new();
            };
            let Ok(light_spans) = light[side].highlight_line(&content, &SYNTAX) else {
                return Vec::new();
            };
            let mut offset = prefix;
            let mut tokens = Vec::new();
            for ((dark, text), (light, _)) in dark_spans
                .into_iter()
                .zip(light_spans)
                .take(256.min(budget))
            {
                let end = (offset + text.len()).min(line.text.len());
                if end <= offset {
                    break;
                }
                tokens.push(SyntaxToken {
                    start: offset,
                    end,
                    dark: [dark.foreground.r, dark.foreground.g, dark.foreground.b],
                    light: [light.foreground.r, light.foreground.g, light.foreground.b],
                });
                offset = end;
            }
            budget -= tokens.len();
            tokens
        })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileChange {
    pub worktree_id: WorktreeId,
    pub key: ChangeKey,
    pub lines: Vec<ChangeLine>,
    pub additions: usize,
    pub deletions: usize,
    pub truncated: bool,
    pub notice: Option<String>,
}

/// Derived presentation data prepared on a client worker, never sent over IPC or persisted.
#[derive(Clone, Debug)]
pub struct PreparedFile {
    pub file: FileChange,
    pub syntax: Vec<Vec<SyntaxToken>>,
}

impl PreparedFile {
    pub fn new(file: FileChange) -> Self {
        let syntax = syntax_tokens(&file);
        Self { file, syntax }
    }
}

impl FileChange {
    pub fn from_patch(
        worktree_id: WorktreeId,
        key: ChangeKey,
        patch: &str,
        truncated: bool,
    ) -> Self {
        let mut result = Self {
            worktree_id,
            key,
            lines: Vec::new(),
            additions: 0,
            deletions: 0,
            truncated,
            notice: None,
        };
        let (mut old, mut new) = (0, 0);
        let mut in_hunk = false;
        let mut text_budget = MAX_FILE_BYTES;
        for raw in patch.split_terminator('\n') {
            if result.lines.len() == MAX_DIFF_LINES {
                result.truncated = true;
                break;
            }
            let raw = raw.strip_suffix('\r').unwrap_or(raw);
            if raw.starts_with("diff --git ") {
                in_hunk = false;
            }
            let mut kind = crate::diff::lines(raw)
                .next()
                .map_or(DiffLineKind::Context, |line| line.kind);
            if raw.starts_with("@@ ") {
                let mut fields = raw.split_whitespace();
                old = fields.nth(1).and_then(parse_range).unwrap_or(0);
                new = fields.next().and_then(parse_range).unwrap_or(0);
                in_hunk = true;
            } else if in_hunk {
                kind = match raw.as_bytes().first() {
                    Some(b'+') => DiffLineKind::Addition,
                    Some(b'-') => DiffLineKind::Deletion,
                    Some(b' ') => DiffLineKind::Context,
                    _ => DiffLineKind::Notice,
                };
            }
            let (old_line, new_line) = if in_hunk {
                match kind {
                    DiffLineKind::Addition => {
                        result.additions += 1;
                        new += 1;
                        (None, Some(new - 1))
                    }
                    DiffLineKind::Deletion => {
                        result.deletions += 1;
                        old += 1;
                        (Some(old - 1), None)
                    }
                    DiffLineKind::Context => {
                        old += 1;
                        new += 1;
                        (Some(old - 1), Some(new - 1))
                    }
                    _ => (None, None),
                }
            } else {
                (None, None)
            };
            let mut end = raw.len().min(MAX_LINE_BYTES);
            while !raw.is_char_boundary(end) {
                end -= 1;
            }
            if end < raw.len() {
                result.truncated = true;
            }
            let mut text = safe_text(&raw[..end]);
            let mut retained = text.len().min(MAX_LINE_BYTES).min(text_budget);
            while !text.is_char_boundary(retained) {
                retained -= 1;
            }
            if retained < text.len() {
                result.truncated = true;
                text.truncate(retained);
            }
            if text_budget == 0 {
                result.truncated = true;
                break;
            }
            text_budget -= text.len();
            result.lines.push(ChangeLine {
                kind,
                text,
                old_line,
                new_line,
            });
        }
        result
    }

    pub fn metadata(worktree_id: WorktreeId, key: ChangeKey, notice: &str) -> Self {
        Self {
            worktree_id,
            key,
            lines: Vec::new(),
            additions: 0,
            deletions: 0,
            truncated: false,
            notice: Some(notice.into()),
        }
    }

    /// Align adjacent removal/addition blocks without inventing correspondence between lines.
    pub fn split_rows(&self) -> Vec<(Option<usize>, Option<usize>)> {
        let mut rows = Vec::new();
        let mut i = 0;
        while i < self.lines.len() {
            if matches!(
                self.lines[i].kind,
                DiffLineKind::Deletion | DiffLineKind::Addition
            ) {
                let mut left = Vec::new();
                let mut right = Vec::new();
                while i < self.lines.len()
                    && matches!(
                        self.lines[i].kind,
                        DiffLineKind::Deletion | DiffLineKind::Addition
                    )
                {
                    if self.lines[i].kind == DiffLineKind::Deletion {
                        left.push(i);
                    } else {
                        right.push(i);
                    }
                    i += 1;
                }
                for n in 0..left.len().max(right.len()) {
                    rows.push((left.get(n).copied(), right.get(n).copied()));
                }
            } else {
                rows.push((Some(i), Some(i)));
                i += 1;
            }
        }
        rows
    }
}

fn parse_range(range: &str) -> Option<u32> {
    range.get(1..)?.split(',').next()?.parse().ok()
}

fn safe_text(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() && c != '\t' {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

/// Client-only review state; no source contents or preferences are persisted.
#[derive(Clone, Debug, Default)]
#[allow(clippy::struct_excessive_bools)] // Independent presentation controls, not lifecycle states.
pub struct ReviewState {
    pub checkout: Option<WorktreeId>,
    pub files: Vec<ChangedFile>,
    pub selected: Option<ChangeKey>,
    pub file: Option<FileChange>,
    pub syntax: Vec<Vec<SyntaxToken>>,
    pub filter: String,
    pub search: String,
    pub collapsed: [bool; 3],
    pub paused: bool,
    pub split: bool,
    pub detail: bool,
    pub file_focus: bool,
    pub scroll: usize,
    pub horizontal: usize,
    pub truncated: bool,
    pub error: Option<String>,
    pub generation: u64,
    pub updated: bool,
}

impl ReviewState {
    pub fn enter(&mut self, checkout: WorktreeId) {
        if self.checkout != Some(checkout) {
            let generation = self.generation.wrapping_add(1);
            *self = Self {
                checkout: Some(checkout),
                generation,
                file_focus: true,
                ..Self::default()
            };
        }
    }
    pub fn visible(&self) -> Vec<&ChangedFile> {
        let query = self.filter.to_lowercase();
        self.files
            .iter()
            .filter(|f| {
                !self.collapsed[f.key.group as usize]
                    && f.key.display_path().to_lowercase().contains(&query)
            })
            .collect()
    }
    pub fn apply_list(&mut self, list: CheckoutChanges) {
        if Some(list.worktree_id) != self.checkout {
            return;
        }
        let position = self
            .visible()
            .iter()
            .position(|f| Some(&f.key) == self.selected.as_ref())
            .unwrap_or(0);
        self.files = list.files;
        self.truncated = list.truncated;
        if !self
            .visible()
            .iter()
            .any(|f| Some(&f.key) == self.selected.as_ref())
        {
            let files = self.visible();
            let key = files
                .get(position.min(files.len().saturating_sub(1)))
                .map(|f| f.key.clone());
            self.select(key);
        }
        self.error = None;
    }
    pub fn select(&mut self, key: Option<ChangeKey>) {
        if self.selected != key {
            self.selected = key;
            self.file = None;
            self.syntax.clear();
            self.scroll = 0;
            self.horizontal = 0;
            self.generation = self.generation.wrapping_add(1);
        }
    }
    pub fn move_file(&mut self, delta: isize) {
        let files = self.visible();
        let index = files
            .iter()
            .position(|f| Some(&f.key) == self.selected.as_ref())
            .unwrap_or(0);
        let next = index
            .saturating_add_signed(delta)
            .min(files.len().saturating_sub(1));
        let key = files.get(next).map(|f| f.key.clone());
        self.select(key);
    }
    pub fn apply_file(&mut self, file: FileChange) {
        self.apply_prepared(PreparedFile::new(file));
    }

    pub fn apply_prepared(&mut self, prepared: PreparedFile) {
        let PreparedFile { file, syntax } = prepared;
        if Some(file.worktree_id) != self.checkout || Some(&file.key) != self.selected.as_ref() {
            return;
        }
        let anchor = self
            .file
            .as_ref()
            .and_then(|f| f.lines.get(self.scroll))
            .map(|l| (l.old_line, l.new_line));
        self.updated = self.file.as_ref().is_some_and(|old| old != &file);
        if self.file.as_ref() != Some(&file) {
            self.syntax = syntax;
        }
        if let Some((old, new)) = anchor {
            self.scroll = file
                .lines
                .iter()
                .enumerate()
                .filter_map(|(i, line)| {
                    let distance = new
                        .zip(line.new_line)
                        .or_else(|| old.zip(line.old_line))
                        .map(|(a, b)| a.abs_diff(b));
                    distance.map(|d| (d, i))
                })
                .min()
                .map_or(self.scroll, |(_, i)| i);
        }
        self.scroll = self.scroll.min(file.lines.len().saturating_sub(1));
        self.file = Some(file);
        self.error = None;
    }
    pub fn matches(&self) -> Vec<usize> {
        if self.search.is_empty() {
            return Vec::new();
        }
        let needle = self.search.to_lowercase();
        self.file.as_ref().map_or_else(Vec::new, |f| {
            f.lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.text.to_lowercase().contains(&needle))
                .map(|(i, _)| i)
                .collect()
        })
    }
    pub fn jump(&mut self, forward: bool, search: bool) {
        let positions = if search {
            self.matches()
        } else {
            self.file.as_ref().map_or_else(Vec::new, |f| {
                f.lines
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| l.kind == DiffLineKind::HunkHeader)
                    .map(|(i, _)| i)
                    .collect()
            })
        };
        let target = if forward {
            positions
                .iter()
                .find(|i| **i > self.scroll)
                .or_else(|| positions.first())
        } else {
            positions
                .iter()
                .rev()
                .find(|i| **i < self.scroll)
                .or_else(|| positions.last())
        };
        if let Some(index) = target {
            self.scroll = *index;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_review_protocol_round_trips_groups_paths_and_structured_lines() {
        use crate::protocol::{ClientRequest, DaemonResponse, Frame, MessageClass};
        let id = WorktreeId::new();
        let list = ClientRequest::ListCheckoutChanges { worktree_id: id };
        let frame = Frame::message(MessageClass::Request, 10, &list).unwrap();
        assert_eq!(frame.payload_as::<ClientRequest>().unwrap(), list);
        for group in ChangeGroup::ALL {
            let key = ChangeKey {
                group,
                path: b"-file with spaces\n.rs".to_vec(),
            };
            let request = ClientRequest::LoadFileChange {
                worktree_id: id,
                key: key.clone(),
            };
            let frame = Frame::message(MessageClass::Request, 10, &request).unwrap();
            let decoded = Frame::decode(&frame.encode().unwrap()).unwrap();
            assert_eq!(decoded.payload_as::<ClientRequest>().unwrap(), request);
            let response = DaemonResponse::FileChange(FileChange::from_patch(
                id,
                key,
                "@@ -1 +1 @@\n-old\n+new\n",
                false,
            ));
            let frame = Frame::message(MessageClass::Response, 10, &response).unwrap();
            assert_eq!(frame.payload_as::<DaemonResponse>().unwrap(), response);
        }
    }
    #[test]
    fn syntax_tokens_cover_supported_languages_and_keep_byte_ranges_valid() {
        for extension in [
            "rs", "js", "ts", "json", "py", "sh", "toml", "yaml", "md", "html", "css", "c", "cpp",
        ] {
            let file = FileChange::from_patch(
                WorktreeId::new(),
                ChangeKey {
                    group: ChangeGroup::Untracked,
                    path: format!("file.{extension}").into_bytes(),
                },
                "@@ -0,0 +1 @@\n+value = \"sample\"\n",
                false,
            );
            let syntax = syntax_tokens(&file);
            assert!(!syntax[1].is_empty(), "missing syntax for {extension}");
            for token in &syntax[1] {
                assert!(file.lines[1].text.get(token.start..token.end).is_some());
            }
        }
    }

    #[test]
    fn hostile_text_and_line_counts_fit_the_protocol_frame() {
        let patch = format!("@@ -0,0 +1 @@\n+{}", "\u{1b}".repeat(MAX_FILE_BYTES));
        let file = FileChange::from_patch(WorktreeId::new(), key(), &patch, false);
        assert!(file.truncated);
        assert!(
            file.lines
                .iter()
                .all(|line| line.text.len() <= MAX_LINE_BYTES && !line.text.contains('\u{1b}'))
        );
        let frame = crate::protocol::Frame::message(
            crate::protocol::MessageClass::Response,
            10,
            &crate::protocol::DaemonResponse::FileChange(file),
        )
        .unwrap();
        assert!(frame.encode().unwrap().len() < crate::protocol::MAX_FRAME_SIZE);
    }

    #[test]
    fn refresh_reanchors_lines_and_search_and_never_accepts_another_file() {
        let id = WorktreeId::new();
        let mut review = ReviewState::default();
        review.enter(id);
        review.select(Some(key()));
        review.apply_file(FileChange::from_patch(
            id,
            key(),
            "@@ -5 +5 @@\n-old\n+new\n",
            false,
        ));
        review.scroll = 2;
        review.search = "NEW".into();
        assert_eq!(review.matches(), vec![2]);
        review.apply_file(FileChange::from_patch(
            id,
            key(),
            "header\n@@ -5 +5 @@\n-old\n+new\n",
            false,
        ));
        assert_eq!(review.scroll, 3);
        assert!(review.updated);
        let previous = review.file.clone();
        review.apply_file(FileChange::from_patch(WorktreeId::new(), key(), "", false));
        assert_eq!(review.file, previous);
        review.jump(false, false);
        assert_eq!(review.scroll, 1);
    }
    fn key() -> ChangeKey {
        ChangeKey {
            group: ChangeGroup::Unstaged,
            path: b"src/file.rs".to_vec(),
        }
    }
    #[test]
    fn numbered_hunks_preserve_header_like_content_and_align_blocks() {
        let file = FileChange::from_patch(
            WorktreeId::new(),
            key(),
            "--- a/f\n+++ b/f\n@@ -2,2 +2,3 @@\n--- old\n+++ new\n+extra\n context\n",
            false,
        );
        assert_eq!((file.additions, file.deletions), (2, 1));
        assert_eq!(file.lines[3].old_line, Some(2));
        assert_eq!(file.lines[4].new_line, Some(2));
        assert!(file.split_rows().contains(&(None, Some(5))));
    }
    #[test]
    fn path_identity_rejects_traversal_and_preserves_display_safety() {
        assert_eq!(display_path(b"invalid-\xff"), "invalid-\\xff");
        for path in [
            b"../x".as_slice(),
            b"/root",
            b".git/config",
            b"C:/x",
            b"x\\y",
            b"a//b",
        ] {
            assert!(
                !ChangeKey {
                    path: path.to_vec(),
                    ..key()
                }
                .valid()
            );
        }
        let path = ChangeKey {
            path: b"-file\nname".to_vec(),
            ..key()
        };
        assert!(path.valid());
        assert_eq!(path.display_path(), "-file\\nname");
    }
    #[test]
    fn list_refresh_preserves_group_selection_and_reports_untracked() {
        let id = WorktreeId::new();
        let mut review = ReviewState::default();
        review.enter(id);
        let entry = ChangedFile {
            key: ChangeKey {
                group: ChangeGroup::Untracked,
                ..key()
            },
            status: ChangeStatus::Untracked,
            old_path: None,
        };
        review.apply_list(CheckoutChanges {
            worktree_id: id,
            files: vec![entry.clone()],
            truncated: false,
        });
        assert_eq!(review.selected, Some(entry.key.clone()));
        review.apply_list(CheckoutChanges {
            worktree_id: id,
            files: vec![entry],
            truncated: true,
        });
        assert!(review.truncated);
        assert_eq!(review.visible().len(), 1);
    }
}
