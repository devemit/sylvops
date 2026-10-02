# Desktop UX language and interaction contract

- Status: accepted for the current desktop
- Date: 2026-10-02

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
- Workspace tabs are full-height segments separated by vertical rules. Selected tabs and rows use a restrained theme tint, a contrast-safe accent outline, and stronger type instead of saturated text backgrounds; inactive controls remain transparent until hover or press.
- A session label and its close action are one visual group.
- Session navigator rows use `Name · State`; open-session tabs show only the name and never prepend a status glyph.
- Wide windows present three independently resizable columns: **Repositories — Choose a codebase**, **Checkouts — Choose a branch checkout**, and **Sessions — Choose or start work**. Each column shows only the records that depend on the selection to its left.
- Compact windows use one tabbed Repository/Checkout/Session navigator beside the main workspace. Narrow windows retain the Navigator/Main switch.
- Repository rows show rolled-up attention. Checkout rows show branch plus Root/Managed status and rolled-up attention. Session rows show their written state and semantic status icon; one click selects and opens a session.
- Double-clicking any Repository, Checkout, or Session row—or pressing `R` in the active column—focuses its inline rename editor. Enter saves through the daemon; Escape or clicking away cancels before submission. The editor has no redundant Save, Cancel, or Rename buttons. Validation and IPC errors preserve the entered value.
- Navigator interaction clears terminal keyboard focus without detaching the session. Long single-line navigation labels stay clipped inside their controls. SylvOps does not show hover tooltips; important meaning must be visible in the interface or available through the shortcuts panel.
- An active managed checkout exposes `Delete checkout` on its selected row. The existing state-bound confirmation still refuses root, external, dirty, missing, invalid, or live-session removal and preserves the Git branch.
- Desktop copy says `Open terminal` and `Leave terminal`; the protocol continues to model the operation as explicit attachment and detachment.
- `Open terminal` / `Leave terminal` and `Stop session` are session-lifecycle actions, so they live at the trailing edge of the active session-tab header rather than beside Terminal/Changes/Details view navigation.
- The native platform UI family remains the default for cross-platform availability. Body text is 14 px, secondary labels are 12 px, and the footer uses a dedicated 12 px status scale.
- Settings is a centered modal with 16 px minimum viewport gutters and a 640 px maximum width at every breakpoint. Its body scrolls internally. **Essentials** contains signature themes, density, and terminal controls; **Updates** contains status, one context-sensitive action, and the periodic-check toggle; collapsed **Advanced** contains Classic themes, layout reset, and protected safety/data-removal controls.
- Signature theme previews are keyboard reachable and persist immediately without Apply or Cancel semantics. Nord, Tokyo Night, Catppuccin, Dracula, Gruvbox Dark, Solarized Light, and Solarized Dark remain under Advanced. Missing named fonts fall back through the renderer.
- One application-owned single-stroke icon family supplies add, refresh, settings, full-screen, terminal, collapse, delete, close, brand, and status marks. Every add icon is borderless; the workspace add control has an additional 10 px leading gap. Named and destructive actions retain visible text.
- JetBrains Mono Regular, Medium, and SemiBold are bundled under OFL-1.1 and form the desktop UI family on every platform. The embedded terminal continues to honor its separate font preference.
- The desktop and TUI new-session forms expose the provider picker and optional display name only. Model, effort, and initial prompt remain supported by the CLI and protocol but are not interactive-form settings.
- The footer presents the actual build identity (including an unsigned-preview or release tag when supplied at build time), local-daemon health beside the version, responsive Workspace/Repository/Checkout/Session context, the active content view, and essential shortcuts. Internal enum names and keyboard-panel debug state are not user-facing status.

## Embedded terminal baseline

The desktop terminal renders the daemon-owned VT screen; it does not print raw provider escape sequences into a host shell. The embedded terminal supports:

- indexed, 256-color, and RGB ANSI foreground/background colors;
- bold, dim, italic, underline, and inverse cell attributes;
- a visible focused/unfocused Block or Line cursor selected in Settings;
- terminal-owned bounded scrollback with wheel, keyboard, and a visible vertical history control;
- alternate-screen wheel forwarding when the running TUI explicitly requests a supported terminal mouse encoding;
- mouse drag selection and platform terminal copy/paste shortcuts;
- bounded paste with bracketed-paste framing when requested by the running application;
- dynamic PTY resizing from the measured rendered viewport.

The scrollbar maps the oldest retained history to the top and live output to the bottom. Typing or pasting while viewing history returns to live output before the input is forwarded, so the real provider-owned input remains visible. The scrollbar is not an outer application scroll view and does not alter PTY ownership.

`Ctrl+]` remains the explicit leave-terminal shortcut. Click and drag mouse-protocol forwarding remains unavailable so desktop text selection stays native; only requested wheel events are forwarded. Scrollback persistence across daemon restart remains an explicit current limitation.

## Guidance, details, and feedback

- First run presents Create workspace, Add repository, and Start session as one in-context checklist. Completed steps remain visibly collapsed; the current step expands and presents the focused required form.
- Empty states offer one contextual next action and do not advertise planned capabilities.
- Details uses human-readable dates formatted with the operating-system locale and timezone, and the label `Path`. IDs, raw timestamps, exact quantities, process IDs, and IPC/PTY terminology are under expandable Technical details with bounded copy actions.
- A Session record remains until user data is removed. Terminal output is separately described as daemon-memory state that is not persisted across daemon restart.
- Successful actions use brief non-modal feedback. Operation-labelled errors appear in the action-status region directly below the toolbar and remain visible until dismissed or corrected; diagnostics are control-character stripped, fail-closed when credential markers appear, and bounded to 1,024 characters.
- Action copy is standardized on `Start shell`, `Start Codex`, `Check again`, `Open terminal`, `Leave terminal`, `Stop session`, and `Delete checkout`.

## Accessibility and visual acceptance

Theme previews expose a visible keyboard focus ring and support Tab/arrow traversal plus Enter activation. In the navigator, Tab/Shift+Tab changes the active column, arrows move selection, Enter opens a session, and N/R/Delete invoke the contextual create, rename, and confirmed destructive actions. Focus does not depend on hover, and state always has a written label in addition to color or icon treatment. The client currently performs state changes immediately: Iced 0.14 exposes no operating-system reduced-motion signal or stable transition primitive, so the immediate path is also the reduced-motion fallback rather than introducing ungoverned animation.

The headless Iced acceptance set covers Canopy at 1440 x 900 with attention state, Grove first run, compact 900 x 700, narrow Navigator and Settings at 680 x 480, the signature/Advanced theme gallery, and destructive confirmation with contextual error. Tests use the bundled default font, fix each layout-tree shape, require a non-trivial color set, and require every scenario to remain visually distinct. Hosted CI additionally pins the approved rendered pixel checksums independently for Windows, Linux, and macOS so platform rasterization differences do not break developer machines.
