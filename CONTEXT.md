# SylvOps

SylvOps is a local application for supervising coding-agent sessions that run in real Git worktrees. This glossary names the product concepts that must remain stable across the daemon, clients, packaging, and documentation.

## Language

### Supervision hierarchy

**Workspace**:
A saved group of Projects that a user supervises together.

**Project**:
An existing local Git repository registered in a Workspace.
_Avoid_: Repository when naming the persisted domain type

**Worktree**:
A Git checkout belonging to a Project, either its root checkout or an isolated managed checkout.
_Avoid_: Checkout when naming the persisted domain type

**Session**:
A supervised interactive agent or shell process that belongs to a Worktree.
_Avoid_: Terminal

**Session Lifecycle**:
The existence and terminal outcome of a Session's process, independent of Provider activity and user attention.
_Avoid_: Provider activity, Attention Event

**Conversation**:
The provider-owned interaction history that a resumable Provider may continue across a linear chain of Sessions.
_Avoid_: Session, terminal history, thread

**Resume Conversation**:
The explicit action of opening an eligible terminal Session to create and attach to its single successor while continuing the same provider Conversation.
_Avoid_: Restart Session, reopen process

**Session Lineage**:
The linear sequence of historical Sessions that continue one Conversation, where each Session has at most one successor.
_Avoid_: Restarted Session, duplicate Session

**Provider**:
The selected kind of interactive program that supplies a Session, such as a plain shell, Codex, or Claude Code.
_Avoid_: Agent when referring to the Session's launch kind

**Provider Activity**:
The current work posture within a live Provider Session, such as working, awaiting input, or waiting after a completed turn.
_Avoid_: Session Lifecycle, Attention Event

**Successful Completion**:
The end of a Provider turn with no known remaining work, or the clean non-requested exit of a plain-shell Session.
_Avoid_: Process exit, user interruption, Stop Session

**Attachment**:
A client's live connection to a Session's terminal.
_Avoid_: Session, process

**Attention Event**:
A new explicit Provider lifecycle transition that requires user awareness. Each Session has at most one pending Attention Event; a newer effective condition replaces an obsolete one, and the event remains pending until acknowledged or resolved according to its Attention Reason.
_Avoid_: Session status, notification, inactivity

**Attention Reason**:
The bounded cause of an Attention Event: input requested, successful completion, recoverable failure, or terminal failure. It is independent of the Session's lifecycle state.
_Avoid_: Session state, notification channel, raw provider error

**Needs Attention**:
The set of Sessions with pending Attention Events, including Sessions that are no longer active.
_Avoid_: Active Sessions, notification queue

**Attention Notification**:
The live, system-coordinated announcement of an Attention Event through native, sound, or in-client channels. It is presentation rather than durable Session state and is not replayed at client startup.
_Avoid_: Attention Event, Session status, notification queue

### GitHub work

**GitHub Repository**:
The remote GitHub repository explicitly associated with a Project as the source of its Issues and Pull Requests.
_Avoid_: Repository when the local Project or its checkout is meant

**Issue**:
A GitHub work item describing a problem or requested change in a GitHub Repository.
_Avoid_: Ticket, task

**Start Work**:
The explicit guided action that creates an isolated managed Worktree and starts a task-capable Provider Session from an Issue.
_Avoid_: Open agent, automatic fix

**Work Source**:
The single Issue whose identity and content initiated a Worktree and its Sessions. One Issue may be the Work Source for multiple Worktrees.
_Avoid_: Attachment, ticket, prompt

### Supervision removal

**Stop Session**:
End a Session's process while retaining its record and history.
_Avoid_: Delete Session

**Delete Session**:
Permanently remove an inactive Session and its retained history from SylvOps without affecting its Worktree.
_Avoid_: Stop Session

**Remove Project**:
Unregister a Project with no Sessions or active managed Worktrees while preserving its repository, root checkout, and branches on disk.
_Avoid_: Delete repository

**Remove Worktree**:
Remove a clean, active, managed non-root Worktree with no active Sessions from disk while preserving its branch.
_Avoid_: Delete branch

**Delete Workspace**:
Permanently remove an empty Workspace from SylvOps without affecting repositories, Worktrees, or branches on disk.
_Avoid_: Delete repository, delete Worktree

### Application delivery

**Application release**:
A deliberately promoted, versioned SylvOps build that users can install or upgrade to.
_Avoid_: Release-phase labels, release candidate, build artifact

**Installation**:
A copy of SylvOps integrated through the operating system's normal launch and removal surfaces. Privilege scope follows the platform's native package contract.
_Avoid_: Extracted archive, copied binary, setup script

**Upgrade**:
A user-approved replacement of an installation that preserves user data and never silently terminates active sessions.
_Avoid_: Reinstall, silent update, overwrite

**Portable build**:
A standalone executable archive that runs without OS application registration and remains an advanced fallback distribution.
_Avoid_: Installer, installed app

**User data**:
SylvOps-owned configuration, preferences, logs, and session metadata stored outside an installation. Repositories and worktrees are user content, never SylvOps user data.
_Avoid_: Application files, installation data
