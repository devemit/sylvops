# Review input without resolving it

The Codex hook-correlation implementation below is superseded by [ADR-0013](0013-structured-codex-input.md). The review/resolution distinction remains in effect.

Input attention can be acknowledged while the Provider still awaits input: review means the user has seen a request, while resolution means trustworthy Provider evidence has ended it. This supersedes ADR-0009's rule that input must remain pending until resolved; explicit successful opening or Mark reviewed acknowledges the exact event across clients, removes unread colour/counts, and retains a neutral waiting label. Foreground visibility alone still never acknowledges attention.

Codex tool hooks correlate permission requests in bounded daemon memory and resolve them only after every unambiguously matched outstanding tool returns; ambiguous or missing correlation remains conservative until a lifecycle transition. Input notifications wait one second and are cancelled by review, resolution, replacement, or foreground display, reducing transient automatic-approval alerts without guessing from terminal output. Hooks do not expose exact approval-resolution timing, so a long-running tool can retain a waiting indicator; app-server integration remains separate work.
