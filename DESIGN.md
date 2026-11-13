# Agent Relay

Agent Relay switches the subscription account used by a coding agent while keeping conversation history available to resume. The first supported agent is Claude Code.

## Goals

- Resume a saved conversation under another subscription account.
- Keep each account's login and token refresh independent.
- Prevent known API credentials or provider routing from silently overriding the selected subscription.
- Preserve existing history and leave the official agent responsible for its transcript format.
- Keep the initial implementation a small CLI, without a proxy, background service, or automatic account rotation.

## Core design

Separate authentication; share conversation storage. Select an account when launching a process, rather than replacing credentials underneath running processes.

```text
agent-relay run personal -- --resume
    |
    +-- validate subscription launch configuration
    +-- select accounts/personal/claude/
    +-- set CLAUDE_CONFIG_DIR
    +-- execute official Claude CLI
              |
              +-- personal login and credential store
              +-- projects/ ------> shared/claude/projects/

agent-relay run work -- --resume
    |
    +-- work login and credential store
    +-- projects/ ---------------> same conversation transcripts
```

`--continue` resumes the latest conversation for the current project. `--resume` opens the agent's picker or selects a conversation. Agent Relay forwards these arguments; it does not implement its own transcript loader.

Changing accounts affects a new process. It does not change the identity of an existing process or transfer running tools, background tasks, or server-side prompt caches.

## Storage

Use the platform's application data directory, with an explicit override for testing and portable installations. Paths below are relative to that root.

```text
config.json                       account names and default selection
accounts/<id>/claude/              independent CLAUDE_CONFIG_DIR
shared/claude/projects/            shared project transcripts
shared/claude/history.jsonl        shared prompt history
backups/<operation-id>/            retained pre-migration data
locks/                            registry and migration locks
```

Registry metadata never contains tokens. Application-owned directories are private to the current user. Credential files remain under Claude's control; on macOS Claude may use a Keychain entry associated with the config directory. Account directory paths remain stable after login, including when an account's display name changes.

The shared `projects/` tree contains more than conversation messages, including project memory. Sharing it makes that data visible to all accounts in the same local history store. This is an intentional user-visible property, not account isolation.

## Account lifecycle

Proposed interface:

```sh
agent-relay account add personal
agent-relay account add work
agent-relay account list
agent-relay account default work
agent-relay run personal
agent-relay run work -- --continue
agent-relay run work -- --resume <session-id>
agent-relay doctor
```

Adding an account invokes the official login flow in a stable, private account directory. It does not copy another account's access or refresh token. Ask the user to select the intended browser identity, and reject duplicate account identities when reliably detectable.

Verify the resulting authentication method and subscription eligibility through a supported local agent interface where available. A successful login command alone is insufficient. If the installed agent cannot establish the required state, explain the limitation and refuse to claim successful subscription-only setup.

The official CLI owns token refresh. Agent Relay does not refresh tokens for quota polling or keep backup credential generations.

## Subscription safeguards

Before login or launch, inspect known authentication and routing overrides in the inherited environment and applicable settings sources. Include API keys, bearer tokens, alternate endpoints, cloud-provider switches, OAuth token overrides, credential file descriptors, and credential helpers.

Reject conflicts with an actionable message naming the variable or setting and its source, without printing secret values. Do not silently modify the user's shell, project settings, managed settings, or global Claude configuration.

Per-account settings remain independent by default. Do not symlink the entire settings file from an existing installation: it may contain API credentials or endpoint routing. Shared preferences can be considered later with an explicit allowlist.

Settings precedence and authentication sources change across Claude versions. Maintain a tested compatibility range and use an official effective-authentication check when one is available. Unknown or unreadable authentication configuration must not be presented as verified subscription-only usage. Preflight is a compatibility safeguard, not a security boundary against arbitrary executables, hooks, or subsequent configuration changes.

Subscription authentication also does not guarantee zero additional charges: account-level extra-usage policies are controlled by the provider. Agent Relay must not claim to enforce those billing policies.

## History and resume

Link each account's `projects/` to one shared directory. Link `history.jsonl` separately for a common prompt history. On platforms without the necessary link support, report an unsupported configuration instead of copying live history into divergent stores.

Leave account identity files and credentials independent. Share no additional state merely because it lives beside transcripts. Checkpoint files, file history, plans, shell snapshots, and other resume dependencies require investigation against supported agent versions before promising full restoration. Initial support promises conversation resume, not restoration of every runtime artifact.

For the initial release, prevent overlapping Agent Relay conversation launches in the same history store with a process-lifetime OS lock. A project-only lock is insufficient: the resume picker and explicit session selection can open another project's transcript. Different-project concurrency is deferred until per-conversation ownership can be enforced. This also serializes managed prompt-history writers. Direct agent launches bypass this protection and must be described as such. Cross-account transcript and prompt-history links follow the reference implementations and are covered by integration tests; live Claude acceptance remains required.

Project paths matter to history discovery. Keep the working directory unchanged, forward arguments faithfully, and provide diagnostics showing the selected account, effective config directory, and shared transcript directory without secrets.

## Existing history migration

Migration is explicit and separate from adding accounts. Never rewrite an existing installation's history automatically.

1. Require relevant agent processes to be stopped; explain when external activity cannot be ruled out.
2. Take the migration lock and validate source and destination paths.
3. Back up original directories and record a recovery manifest before mutation.
4. Merge transcripts without overwriting different content at the same path. Preserve conflicts separately and report them.
5. Verify copied content, then install links using recoverable staged operations.
6. Keep backups until the user explicitly removes them.

A failure must leave original data available and a clear recovery path. Do not assume several filesystem changes form one atomic transaction. Migration should be restartable from its manifest.

## Process behavior

Resolve and launch the official agent executable directly with an argument array. Preserve the working directory, terminal interaction, signals, and exit status. Avoid shell command construction. Account selection is captured once for each process and inherited by its children.

Write registry changes through a temporary file and atomic replacement under an OS lock. Use a stable lock file; do not unlink a held lock. Detect invalid metadata and unsafe links before modifying account or shared directories.

## Validation and release criteria

All automated tests use temporary application roots and synthetic credentials. Never touch a developer's real agent configuration or Keychain.

- Verify independent account directories and stable selection with a fake agent executable.
- Verify rejection of conflicting environment variables and settings, including account-default launches.
- Verify argument forwarding, signal delivery, exit codes, and paths containing spaces.
- Verify account changes do not alter the identity of running processes.
- Verify migration interruption, conflicting files, recovery, and repeated execution preserve data.
- Verify overlapping launches in one project are refused and different projects remain usable.
- With explicitly authorized live accounts, create a conversation under A, exit, resume it under B, and verify the active identity plus restored conversation and tool results.
- Exercise the resume workflow on each supported operating system and agent version. Record limitations around checkpoints, compaction, plugins, and MCP authentication.

Do not promise automatic handoff of a running session. Do not declare cross-account resume reliable solely because the files are linked; the live acceptance workflow is a release requirement.

## Reference approaches

- [Claude Code author's recommendation](https://github.com/anthropics/claude-code/issues/20131#issuecomment-5311357342): independent config directories and logins.
- [Claude credential management](https://code.claude.com/docs/en/authentication#credential-management) and [session behavior](https://code.claude.com/docs/en/sessions).
- [Omarchy's multiple subscriptions workflow](https://omarchy.org/manual/ai/#several-subscriptions): independent logins, shared history, and account selection for new processes.
- [claude-swap](https://github.com/realiti4/claude-swap): global credential switching and an isolated session mode. Agent Relay adopts independent logins rather than global credential replacement.

Reference behavior was reviewed on November 13, 2026. Upstream implementation details are compatibility inputs, not permanent contracts.
