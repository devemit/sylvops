# Codex provider

The daemon-side adapter resolves a canonical native `codex` executable from an explicit absolute override, `PATH`, official npm or standalone layouts, or supported desktop-app locations. Candidate searches are bounded and validate the platform's native file signature before and after canonicalization. Package-manager `.cmd`, PowerShell, JavaScript, and shell shims are never parsed or executed. The adapter then runs bounded `codex --version` and `codex login status` probes and launches the interactive CLI using an executable plus argument vector. Optional prompt, model, effort, working directory, profile, and resume ID are separate arguments; no user text is interpolated into a shell command.

The environment starts from a platform allowlist, retains `CODEX_HOME` so an existing login remains usable, injects only SylvOps session/worktree/hook identifiers, and excludes GitHub and provider API-key variables. SylvOps never initiates login, copies credentials, persists auth state, or logs environment values.

For each daemon lifetime, SylvOps writes a uniquely named `$CODEX_HOME/<name>.config.toml` layer and launches Codex with `--profile <name>`. The base user configuration and repository files are untouched. The layer contains only marked, observational lifecycle command hooks and is removed only if it was created by this process. The relay reads one bounded JSON object, sends it to the authenticated loopback receiver, emits no approving/denying output, and applies strict time and body limits.

The receiver captures documented session IDs for guarded `codex resume <id> --cd <verified-worktree>`. Unknown, duplicate, stale, identity-mismatched, or malformed events cannot silently change state; relevant refusals are audited without persisting raw payloads.

Real-account tests are opt-in. Default tests use the fake agent.

Interface assumptions are limited to the documented [Codex CLI commands](https://developers.openai.com/codex/cli/reference), [authentication behavior](https://developers.openai.com/codex/auth), and [hook contract](https://learn.chatgpt.com/docs/hooks).
