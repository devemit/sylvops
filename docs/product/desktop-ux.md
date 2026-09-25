# Desktop UX language and interaction contract

- Status: accepted for the beta desktop
- Date: 2026-09-25

## User-facing vocabulary

The persisted domain model remains `Workspace → Project → Worktree → Session`. The desktop uses the following clearer labels without changing those domain types or IPC contracts:

| Desktop label | Domain type | Meaning |
|---|---|---|
| Workspace | Workspace | A saved group of repositories used together. |
| Repository | Project | An existing local Git repository registered with SylvOps. |
| Checkout | Worktree | The repository's root checkout or an isolated managed Git worktree. |
| Session | Session | One daemon-owned Shell or Codex process in a checkout. |
| Terminal | Attachment | The live, explicitly attached view of a session's PTY. |

First-run guidance names both the user-facing and domain terms where that distinction matters. Documentation and diagnostics may continue to use `Project` and `Worktree` when referring to the protocol or Git operation precisely.

## Navigation and typography

- Workspace and session tabs scroll horizontally instead of pushing fixed actions outside the window.
- Workspace tabs retain creation order when the active workspace changes; a newly created workspace is appended instead of moving the active tab to the left.
- Active tabs use a persistent, contrast-safe accent and stronger label weight. Neutral hover and pressed fills never replace the active cue or reduce text readability.
- A session label and its close action are one visual group.
- Session navigator rows use `Name · State`; open-session tabs show only the name and never prepend a status glyph.
- The selected session row exposes inline rename. The pencil control or `R` focuses the editor, Enter saves through the daemon, and Escape cancels before submission. Validation and IPC errors preserve the entered value.
- The native platform UI family remains the default for cross-platform availability. Body text is 14 px, secondary labels are 12 px, and the footer uses a dedicated 12 px status scale.
- The footer contains version, current workspace/checkout context, connection health, and essential shortcuts. Internal enum names and keyboard-panel debug state are not user-facing status.

## Embedded terminal baseline

The desktop terminal renders the daemon-owned VT screen; it does not print raw provider escape sequences into a host shell. The beta terminal supports:

- indexed, 256-color, and RGB ANSI foreground/background colors;
- bold, dim, italic, underline, and inverse cell attributes;
- a visible focused/unfocused cursor;
- terminal-owned bounded scrollback with wheel, keyboard, and a visible vertical history control;
- mouse drag selection and platform terminal copy/paste shortcuts;
- bounded paste with bracketed-paste framing when requested by the running application;
- dynamic PTY resizing from the measured rendered viewport.

The scrollbar maps the oldest retained history to the top and live output to the bottom. Typing or pasting while viewing history returns to live output before the input is forwarded, so the real provider-owned input remains visible. The scrollbar is not an outer application scroll view and does not alter PTY ownership.

`Ctrl+]` remains the explicit detach shortcut. Terminal mouse-protocol forwarding and scrollback persistence across daemon restart remain explicit beta limitations.
