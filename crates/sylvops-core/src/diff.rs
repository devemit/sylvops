//! Platform-neutral classification of bounded unified-diff text for replaceable clients.

/// The semantic role of one line in a unified diff.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffLineKind {
    /// A `diff --git`, `---`, or `+++` file header.
    FileHeader,
    /// Git metadata such as modes, indexes, renames, or binary-file notices.
    Metadata,
    /// A unified-diff hunk header beginning with `@@`.
    HunkHeader,
    /// Added content beginning with `+`.
    Addition,
    /// Removed content beginning with `-`.
    Deletion,
    /// Unchanged content or an empty line.
    Context,
    /// A bounded client notice or Git's no-newline marker.
    Notice,
}

/// A borrowed unified-diff line and its semantic role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiffLine<'a> {
    /// The semantic role clients should use when styling the line.
    pub kind: DiffLineKind,
    /// The original line without its line ending.
    pub text: &'a str,
}

/// Classifies every line in bounded unified-diff text without allocating.
pub fn lines(diff: &str) -> impl Iterator<Item = DiffLine<'_>> {
    diff.split('\n').map(|line| {
        let line = line.strip_suffix('\r').unwrap_or(line);
        DiffLine {
            kind: classify(line),
            text: line,
        }
    })
}

fn classify(line: &str) -> DiffLineKind {
    if line.starts_with("diff --git ") || line.starts_with("--- ") || line.starts_with("+++ ") {
        DiffLineKind::FileHeader
    } else if line.starts_with("@@") {
        DiffLineKind::HunkHeader
    } else if line.starts_with('+') {
        DiffLineKind::Addition
    } else if line.starts_with('-') {
        DiffLineKind::Deletion
    } else if line.starts_with(' ') || line.is_empty() {
        DiffLineKind::Context
    } else if line.starts_with("\\ No newline at end of file")
        || (line.starts_with('[') && line.ends_with(']'))
    {
        DiffLineKind::Notice
    } else {
        DiffLineKind::Metadata
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_a_representative_unified_diff() {
        let diff = concat!(
            "diff --git a/src/lib.rs b/src/lib.rs\n",
            "index 1234567..89abcde 100644\n",
            "--- a/src/lib.rs\n",
            "+++ b/src/lib.rs\n",
            "@@ -1,3 +1,3 @@ fn example() {\n",
            " context\n",
            "-removed\n",
            "+added\n",
            "\\ No newline at end of file\n",
            "[Diff truncated by daemon limits]"
        );

        let kinds = lines(diff).map(|line| line.kind).collect::<Vec<_>>();

        assert_eq!(
            kinds,
            vec![
                DiffLineKind::FileHeader,
                DiffLineKind::Metadata,
                DiffLineKind::FileHeader,
                DiffLineKind::FileHeader,
                DiffLineKind::HunkHeader,
                DiffLineKind::Context,
                DiffLineKind::Deletion,
                DiffLineKind::Addition,
                DiffLineKind::Notice,
                DiffLineKind::Notice,
            ]
        );
    }

    #[test]
    fn file_markers_win_over_addition_and_deletion_prefixes() {
        let diff = "--- /dev/null\n+++ b/new.rs\n-old\n+new";
        let kinds = lines(diff).map(|line| line.kind).collect::<Vec<_>>();

        assert_eq!(
            kinds,
            vec![
                DiffLineKind::FileHeader,
                DiffLineKind::FileHeader,
                DiffLineKind::Deletion,
                DiffLineKind::Addition,
            ]
        );
    }

    #[test]
    fn preserves_blank_lines_and_strips_carriage_returns() {
        let parsed = lines(" context\r\n\r\n+added\r\n").collect::<Vec<_>>();

        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[0].text, " context");
        assert_eq!(parsed[1].text, "");
        assert_eq!(parsed[2].text, "+added");
        assert_eq!(parsed[3].text, "");
        assert!(parsed.iter().all(|line| !line.text.ends_with('\r')));
    }

    #[test]
    fn recognizes_common_binary_and_rename_metadata() {
        for line in [
            "new file mode 100644",
            "deleted file mode 100644",
            "similarity index 100%",
            "rename from old.rs",
            "rename to new.rs",
            "Binary files a/image.png and b/image.png differ",
            "GIT binary patch",
        ] {
            assert_eq!(classify(line), DiffLineKind::Metadata, "{line}");
        }
    }
}
