# Agent Relay

Switch Claude subscription accounts and resume the same saved conversation.

Each account logs in independently through the official Claude CLI. Accounts share project transcripts, while credentials and settings stay separate. Relay never swaps global credentials, copies refresh tokens, or proxies model requests.

## Install

Requires macOS with the Command Line Tools (`xcode-select --install`, which provides `/usr/bin/python3`) and the official Claude CLI. Agent Relay is one Python file using only the standard library of Python 3.9; there is nothing else to install. Linux and Windows are not supported.

```sh
./install.sh            # installs ~/.local/bin/agent-relay (or ./install.sh /other/path)
agent-relay --help
```

The installed copy starts on the Python.app binary behind `/usr/bin/python3`, not on `/usr/bin/python3` itself: that stub adds `SDKROOT`, `CPATH`, `LIBRARY_PATH` and `MANPATH` to the environment, which Claude and every build it runs would inherit. Claude receives exactly the environment Relay was started with.

Agent Relay 0.1.0 was first written in Rust; this Python port keeps its commands, files, messages and exit codes, so data created by the Rust version keeps working. The Rust source remains in the git history.

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

Log in once per account, then reuse those saved logins. `run`, `doctor`, and default selection never invoke browser login or log an account out. Claude owns automatic access-token refresh; switching accounts does not invalidate another account's saved login. Launch preflight checks the saved subscription identity, not access-token freshness: `loggedIn: false` and status exit code 1 are accepted for a registered account when Claude still identifies its `claude.ai` method, config directory, paid plan, and matching identity. Claude can then refresh its saved login normally. Registration and `doctor` still require `loggedIn: true`. Missing/invalid status data, API authentication, provider conflicts, and changed identities remain blocking errors; they are never silently accepted as refreshable logins.

Browser sign-in can still be necessary when the saved refresh credential expires, is revoked, or becomes unavailable. Relay does not attempt refresh itself or interpret a generic probe failure as a reason to log in again. Synthetic integration tests cover the refresh handoff and rejected refresh without deleting credentials or history. Real aged-but-refreshable login behavior remains a live acceptance requirement for each supported Claude version.

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

Recovery verifies hashes and resumes from the saved manifest. Backups are retained. The source is required until snapshotting finishes; afterward recovery uses the snapshot. Imports reject symlinks and special files rather than following them into unrelated data.

History files are hashed and copied using a fixed 1 MiB buffer, with no per-transcript size cap. Large transcripts do not need to fit in memory. Allow time to read each file and sufficient free disk space: an initial import retains a snapshot and a destination copy, roughly twice the source size in additional storage. Conflicts and retained backups from repeated imports can require more. A changed or incomplete copy is never published as a verified file. The 64 MiB limit still applies to configuration/settings and the recovery manifest, which are parsed as JSON; it no longer applies to transcript content.

## Subscription checks

Relay refuses known authentication/routing environment overrides, including `ANTHROPIC_*`, OAuth-token overrides, cloud-provider switches, and credential file descriptors. It also rejects variables that disable transcript persistence or remove the selected account from subprocess environments. Errors name the conflicting setting without showing its value.

It inspects account settings, project/ancestor settings, and system managed settings files for provider credentials and helpers. The local `claude auth status` must then identify the selected config directory, a saved `claude.ai` login, and a recognized paid plan. A launch may hand a saved login to Claude for refresh even when status reports it logged out; this never bypasses method, plan, directory, or identity checks. API keys, credential helpers, and cloud-provider authentication are refused. No default-account launch bypasses these checks.

Flags that bypass verification or history persistence, change the project at launch, or detach the process are unsupported, including `--settings`, `--setting-sources`, `--bare`, `--no-session-persistence`, `--worktree`, and `--bg`. Use ordinary `--continue`, `--resume`, and prompt arguments.

This is preflight validation, not enforcement against configuration changes, in-session `/login` or `/cd`, arbitrary hooks, plugins, or a different executable supplied as Claude. Extra-usage billing policies remain controlled by your subscription provider. Relay does not guarantee zero additional charges.

## Storage

Default: `~/Library/Application Support/agent-relay`.

Use `--root /absolute/path` or `AGENT_RELAY_HOME` to select another root, and `--claude /absolute/path/to/claude` to select an executable. Application-owned directories and files are created privately. An existing application root must have mode `0700`.

```text
config.json
accounts/<name>/claude/        independent login and settings
shared/claude/projects/        shared transcripts and project memory
shared/claude/history.jsonl    shared prompt history
backups/import-<id>/           manifest, snapshot, preserved conflicts
locks/                        persistent OS lock files
```

## Local development and verification

```sh
/usr/bin/python3 -m unittest discover -s tests -v
AGENT_RELAY_BIN=/path/to/agent-relay /usr/bin/python3 -m unittest discover -s tests   # test another build
```

Validation runs locally; this repository does not use GitHub Actions.

Integration tests use a fake Claude executable and isolated temporary homes. They run an installed copy, started the way `install.sh` starts it. They cover cross-account resume plumbing, authentication conflicts, default selection, process exit and signals, concurrent-writer exclusion, corrupt metadata, and import recovery. Further tests pin what the port must keep from the Rust version: the exact environment Claude receives, default `SIGPIPE` in Claude, byte-identical `config.json` and manifest files, and error texts. The command-line tests pass against both the Rust 0.1.0 binary and this port. They do not prove compatibility with a live Claude service.

Before releasing a version, follow [the live acceptance procedure](docs/live-acceptance.md) with explicitly authorized accounts on each supported platform. See [DESIGN.md](DESIGN.md) for the design and its limits.
