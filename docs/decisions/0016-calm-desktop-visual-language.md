# ADR 0016: Calm desktop visual language and contextual session actions

## Context

The beta desktop made state unmistakable with saturated fills, repeated action colors, and hover tooltips. In practice this made repositories, checkouts, sessions, content tabs, and lifecycle actions compete for attention. `Open terminal` / `Leave terminal` and `Stop` also appeared beside Terminal/Changes/Details even though they act on the session, not on the selected view.

Comparable tools consistently separate subjective appearance preferences from workflow state:

- [Zed appearance](https://zed.dev/docs/appearance) separates UI and terminal typography, previews themes immediately, and persists the chosen appearance.
- [VS Code terminal appearance](https://code.visualstudio.com/docs/terminal/appearance) keeps terminal font family, size, spacing, weight, and cursor preferences distinct from workbench navigation.
- [Ghostty configuration](https://ghostty.org/docs/config) starts from a strong zero-configuration default while treating theme and font as appropriate user preferences.
- [Warp text, fonts, and cursor](https://docs.warp.dev/terminal/appearance/text-fonts-cursor) groups terminal typeface, size, weight, line height, contrast, ligatures, and cursor controls under Appearance.
- [Warp tab behavior](https://docs.warp.dev/terminal/appearance/tabs-behavior) uses tab indicators for exceptional state rather than coloring every tab as a status badge.

## Decision

- Use neutral surfaces by default. Selection is communicated by a soft theme tint, a one-pixel accent outline, and font weight. Hover is a temporary neutral tint. Saturated fills are reserved for terminal content and exceptional feedback, not ordinary navigation.
- Keep destructive actions semantically red, but render them as a soft tint plus outline until pressed.
- Place terminal attachment and session termination controls in the active session-tab header. Keep Terminal/Changes/Details exclusively for switching views.
- Do not use hover tooltips. Labels, status copy, confirmation dialogs, and the shortcuts panel must carry the interaction contract without transient overlays.
- Inline session rename is a direct-manipulation interaction: double-click to edit, Enter to save, Escape or click-away to cancel. Validation and daemon failures keep the editor and input intact.
- Preserve a useful zero-configuration default while offering ten curated themes and four terminal typeface choices. Preferences remain bounded, local desktop state and do not change daemon authority.

## Consequences

- Dense areas have less competing color, and theme palettes behave more consistently across light and dark variants.
- Session lifecycle actions remain visible but are visually and spatially associated with the session they affect.
- Removing tooltips means clipped labels do not reveal hidden text on hover; future search or detail surfaces should solve discovery explicitly if this becomes a problem.
- Named terminal fonts may not be installed on every machine, so the renderer falls back rather than making font availability a startup requirement.
- Older persisted desktop state remains readable through serde defaults; the protocol version does not change because named MessagePack fields remain backward compatible.
