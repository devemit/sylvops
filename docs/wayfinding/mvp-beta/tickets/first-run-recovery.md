---
title: Choose the first-run and provider-recovery experience
label: wayfinder:prototype
state: closed
assignee: null
blocked_by: []
---

> **Historical release-planning record:** this ticket belongs to the retired beta-phase plan and is not an active roadmap item or release gate.

## Question

What is the smallest desktop flow that lets a first-time user understand Workspace versus Project versus Worktree, recover from unavailable or unauthenticated providers, and successfully launch a session without consulting documentation?

## Resolution

An empty daemon snapshot opens one cancellable three-step desktop flow. Step 1 creates a Workspace and explains that it groups repositories. Step 2 registers an existing Git repository as a Project and explains that its current checkout becomes the root Worktree. Step 3 launches a Session in that selected root Worktree and explains the complete hierarchy. Successful steps advance directly to the next form; the final confirmation points to the existing explicit Attach action for opening the terminal. Outside first run, the existing individual creation forms remain unchanged.

Shell remains the ready default and immediate fallback. The session form shows the selected provider's current readiness. An unavailable Codex selection tells the user to install the Codex CLI; an unauthenticated selection tells the user to run `codex login` in a terminal. Both states offer **Check again**, which sends the existing typed `ProbeProvider` request through authenticated IPC and updates the open form from the daemon's authoritative result. Switching providers clears stale submission errors.

SylvOps does not install software, initiate login, capture credentials, open a shell, or execute a provider from this recovery UI. Users can cancel the first-run sequence after any completed step and resume with the normal Workspace, Project, Worktree, and Session controls.

## Acceptance evidence

- Desktop unit tests cover the ordered hierarchy explanations and the unavailable/unauthenticated Codex recovery instructions, including the Shell fallback.
- Focused desktop tests prove the provider re-probe integration compiles with the existing daemon-owned provider contract.
- The repository formatting, Clippy, and full workspace test gates pass after the implementation.
