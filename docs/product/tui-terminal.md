# TUI terminal selection and copying

In an attached terminal, drag with the left mouse button to select visible text, then click **Copy** in the terminal border or press **Ctrl+Shift+C**. The Copy action works even if the host terminal intercepts the shortcut. Read-only observers can select and copy too. Selecting alone never writes to the clipboard; Ctrl+C remains the provider interrupt.

The selected view stays still while the Session continues running and receiving output. Copy retains the selection. Press Escape to clear it and return to live output; with no selection, Escape reaches the provider normally. Typing, pasting, scrolling, resizing, resynchronizing, detaching, or changing Sessions also clears selection. Scroll to the desired output before selecting: selection is limited to the visible viewport and does not automatically scroll across pages. Width and wrapping behavior are unchanged.

Copy extracts plain Unicode text without terminal escape sequences, colors, or UI borders. It retains indentation and intentional line breaks and joins soft-wrapped rows. The visible snapshot and copied text are limited to 1 MiB; oversized selections are refused explicitly rather than truncated. Copied text is not logged or persisted by SylvOps.

Native copying requires an accessible system clipboard on the machine running the TUI. Windows and macOS use their native clipboard; Linux supports X11 and Wayland data-control environments, with XWayland available where supported. An unsupported/headless/SSH environment may have no accessible clipboard: the TUI reports that and retains selection. There is no shell-command or OSC 52 clipboard fallback. On Linux the TUI retains clipboard ownership while running; availability after exit depends on the desktop clipboard manager.

## Verification

Default TUI tests use a fake clipboard and headless rendering. They cover selected output remaining stable, local copy routing, native-operation failures/backpressure, Unicode and soft wrapping, observers, viewport boundaries, size bounds, and selection cleanup. Run the normal repository formatting, Clippy, and workspace test gates on Windows, Linux, and macOS.

An explicit Windows clipboard round-trip test is available:

```text
cargo test -p sylvops-tui --lib clipboard::tests::windows_native_clipboard_copy_round_trip_restores_previous_text -- --ignored --exact
```

It requires an existing text clipboard, writes a fixed test string, checks it, and restores the previous text without printing clipboard contents. It is excluded from default tests.

For interactive acceptance on each supported desktop, attach a fake-provider Session, select output while more arrives, copy and paste into another application, repeat as an observer and from scrollback, then verify Escape, Ctrl+C, wheel scrolling, and existing navigation. Check narrow layouts, Copy-button readability, and selection contrast in every theme. Clipboard unavailability must remain an inline error and must not stop the Session or TUI.
