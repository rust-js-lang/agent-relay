# Agent Relay

Switch Claude subscription accounts and resume the same saved conversation.

Each account logs in independently through the official Claude CLI. Accounts share project transcripts, while credentials and settings stay separate. Relay never swaps global credentials, copies refresh tokens, or proxies model requests.

## Install

Requires macOS or Linux, Rust 1.100, and the official Claude CLI. The toolchain file and CI pin Rust 1.100.0. Windows is not supported yet.

```sh
cargo install --path . --locked
agent-relay --help
```

The initial compatibility gate accepts Claude Code `>=2.1.268` and `<2.2.0`, requiring the documented `configDirectory` field in `claude auth status`. This is a required API contract, not a claim that every version in that range has passed live acceptance. Live subscription testing is still required before a production release.

## Add subscriptions and resume

```sh
agent-relay account add personal
agent-relay account add work
agent-relay account list

cd /path/to/project
agent-relay run personal
# Work with Claude, then exit it.

agent-relay run work -- --continue
# Or choose a conversation:
agent-relay run work -- --resume
# Or resume a particular session:
agent-relay run work -- --resume <session-id>
```

Select the intended subscription in the browser during login. Relay checks the reported login method, config directory, subscription plan, and identity; it refuses unverified or conflicting authentication. A failed add retains its account directory for a retry and does not register the account.

Account names start with a lowercase letter and use lowercase ASCII letters, digits, `-`, or `_` (maximum 64 characters). This avoids names colliding on case-insensitive filesystems.

`--continue` opens the latest conversation for the current project directory. `--resume` uses Claude's own session picker or session selection. Relay preserves the working directory and forwards arguments after `--` without running a shell.

```sh
agent-relay account default work
agent-relay run -- --continue
agent-relay account login work  # renew this account's login
agent-relay doctor work
```

Changing the default does not change a running process's account. There is no automatic switching inside a live session. Exit Claude and launch the next account to resume.

Log in once per account, then reuse those saved logins. `run`, `doctor`, and default selection never invoke browser login or log an account out. Claude owns automatic access-token refresh; switching accounts does not invalidate another account's saved login. Browser sign-in can still be necessary when the saved refresh credential expires, is revoked, or becomes unavailable. A failed local status probe is not proof of expiry: Relay refuses the unverified launch, preserves the login, and suggests retrying rather than automatically reauthenticating. Aged-but-refreshable login behavior is a live acceptance requirement.

## History guarantees and limits

- Each account's `projects/` is a link to the same transcript tree. Conversations and project memory are visible across these accounts. Prompt history (`history.jsonl`) is also a live link to a shared file.
- Credentials, settings, checkpoints, shell snapshots, and other runtime files remain account-local. Conversation resume does not promise cross-account file rewind or restoration of running tasks.
- Only one Relay conversation process may run at a time per history store, even across different projects. A resume picker can select another project's conversation, so a project-only lock would be insufficient.
- OS locks survive the launch into Claude. Descendants that inherit lock descriptors can keep a store locked after the main process exits. Close those descendants before retrying; do not delete lock files.
- Direct `claude` launches bypass Relay's locks and checks. Do not open a Relay transcript for writing through another process at the same time.
- Keep the same application root after adding accounts. On macOS a login's Keychain entry depends on its config directory.
- Claude owns transcript formats, compaction, retention, and resume behavior. A stored conversation is not a saved running process or a portable copy of every tool's state.

## Bring existing conversations

Stop Claude processes that might write the source or destination before importing. Relay cannot detect every direct Claude process.

```sh
agent-relay history import "$HOME/.claude/projects" --agents-stopped
```

Import creates a verified snapshot, then merges it into shared transcript storage. Pass the `projects/` directory, not an account home; recognized account configuration at the source root is rejected to avoid importing credentials. Import never modifies the source or overwrites different destination content. Conflicting incoming files remain under the printed backup directory's `conflicts/`; inspect and resolve them manually. The original installation remains independent, so use Relay to continue the imported conversations afterward.

If interrupted, use the recovery ID printed before copying begins:

```sh
agent-relay history recover import-<id> --agents-stopped
```

Recovery verifies hashes and resumes from the saved manifest. Backups are retained. The source is required until snapshotting finishes; afterward recovery uses the snapshot. Imports reject symlinks and special files rather than following them into unrelated data. Individual imported files are limited to 64 MiB.

## Subscription checks

Relay refuses known authentication/routing environment overrides, including `ANTHROPIC_*`, OAuth-token overrides, cloud-provider switches, and credential file descriptors. It also rejects variables that disable transcript persistence or remove the selected account from subprocess environments. Errors name the conflicting setting without showing its value.

It inspects account settings, project/ancestor settings, and system managed settings files for provider credentials and helpers. The effective local `claude auth status` must then report the selected config directory, a `claude.ai` login, and a recognized paid plan. API keys, credential helpers, and cloud-provider authentication are refused. No default-account launch bypasses these checks.

Flags that bypass verification or history persistence, change the project at launch, or detach the process are unsupported, including `--settings`, `--setting-sources`, `--bare`, `--no-session-persistence`, `--worktree`, and `--bg`. Use ordinary `--continue`, `--resume`, and prompt arguments.

This is preflight validation, not enforcement against configuration changes, in-session `/login` or `/cd`, arbitrary hooks, plugins, or a different executable supplied as Claude. Extra-usage billing policies remain controlled by your subscription provider. Relay does not guarantee zero additional charges.

## Storage

Defaults:

- macOS: `~/Library/Application Support/agent-relay`
- Linux: `$XDG_DATA_HOME/agent-relay`, or `~/.local/share/agent-relay`

Use `--root /absolute/path` or `AGENT_RELAY_HOME` to select another root, and `--claude /absolute/path/to/claude` to select an executable. Application-owned directories and files are created privately. An existing application root must have mode `0700`.

```text
config.json
accounts/<name>/claude/        independent login and settings
shared/claude/projects/        shared transcripts and project memory
shared/claude/history.jsonl    shared prompt history
backups/import-<id>/           manifest, snapshot, preserved conflicts
locks/                        persistent OS lock files
```

## Development and verification

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Integration tests use a fake Claude executable and isolated temporary homes. They cover cross-account resume plumbing, authentication conflicts, default selection, process exit and signals, concurrent-writer exclusion, corrupt metadata, and import recovery. They do not prove compatibility with a live Claude service.

Before releasing a version, follow [the live acceptance procedure](docs/live-acceptance.md) with explicitly authorized accounts on each supported platform. See [DESIGN.md](DESIGN.md) for the design and its limits.
