# ADR 0017: Bounded TUI appearance settings

## Status

Accepted.

## Context

The TUI previously had one hard-coded chrome palette and one spacing model. Its daemon-backed state persisted navigation only, while desktop appearance preferences lived in an independent client scope. Adding appearance controls must preserve daemon authority, backward compatibility, readable status labels, and provider-owned terminal presentation.

## Decision

Protocol 1.14 extends `TuiState` with two bounded preferences: `theme` and `density`. Themes are Terminal Default, Nord, Tokyo Night, Catppuccin, Gruvbox Dark, and Solarized Light. Density is Comfortable or Compact. Missing fields use Terminal Default and Comfortable; unknown serialized appearance values normalize to those defaults. The existing `tui/navigation.v1` record remains separate from `desktop/navigation.v1`, and preference writes neither advance the entity revision nor emit lifecycle events.

Settings is a distinct TUI interaction mode opened from the header, `s`, or the command palette. Selection previews immediately in the client. Apply writes the complete bounded TUI state through authenticated IPC; Escape and Cancel restore the theme and density snapshot captured when Settings opened. Forms, searches, terminal data, attachment state, and credentials remain excluded from persistence.

Application chrome is rendered from semantic roles for ordinary foreground/background, borders, focus, selection, success, attention, danger, and muted text. Written labels, radio symbols, focus markers, and status symbols remain authoritative. Attached PTY cells retain their VT foreground, background, and text modifiers, including explicit terminal-default colors; application themes do not rewrite provider-owned terminal colors.

## Consequences

- TUI appearance survives restart without coupling it to native desktop preferences.
- Comfortable and Compact density share one bounded metric choice across chrome, Explorer rows, forms, and modal spacing.
- Exact protocol matching means 1.13 clients and 1.14 daemons must be upgraded together.
