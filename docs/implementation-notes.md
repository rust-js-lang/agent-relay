# Implementation evidence

This document records which upstream behavior informed the implementation and where its regression tests live. Source was read from local clones. These observations are not substitutes for live Claude acceptance.

## Independent accounts and shared transcripts

Omarchy (`ac6cb9c71`) selects `CLAUDE_CONFIG_DIR` in `bin/omarchy-agent-account-exec`, links `projects/` in `bin/omarchy-agent-account-state`, and keeps each account's credentials in its own home. `test/shell.d/agent-account-test.sh` asserts independent credential files, shared history links, private permissions, and stable home paths. `agent-account-exec-test.sh` exercises wrapper dispatch and account pinning with fake executables.

Relay uses the same directory-selection model and live links for transcripts and prompt history, testing them through a fake executable, including continuing and resuming under a different identity. Its single-writer policy serializes managed prompt-history writes. It keeps settings separate to avoid introducing provider credentials through shared settings.

## Authentication checks must not destroy a profile

claude-swap (`b042283`) uses `claude auth status --json` in `src/claude_swap/session.py`, checking login method, email, and organization. Its `TestIsSessionValid` tests distinguish invalid authentication from an inconclusive probe. A timeout must not be interpreted as evidence that credentials should be deleted.

Relay refuses a launch when it cannot verify authentication, but retains the account directory, credentials, and shared history. It additionally requires the selected config directory and a recognized subscription plan. Missing subscription metadata can therefore refuse a valid login; the error is a verification failure, not proof that the user lacks a subscription.

## History must remain live and conflicts must survive

claude-swap's `TestShareHistoryPosix` checks links, repeated synchronization, preservation of existing user files, merge collisions, and that no transcript is deleted. Its `TestHistoryMergeSharesTheDestination` tests serialization when separate account stores share a destination and ensures linking finishes before releasing the migration lock.

CCS's `tests/unit/shared-context-policy.test.ts` checks shared project links, concurrent context setup, and preserving files when changing context mode. Its merge helper retains conflicting source content rather than overwriting the destination.

Relay tests live links, identical transcript visibility across accounts, no destructive replacement of existing directories, non-overwriting imports, retained conflicts, interrupted snapshot and merge recovery, and hash verification before using backups. It leaves an imported source untouched and does not attempt to merge a source while an agent is still writing it.

## Locks have different jobs

The upstream tools use locks for credential/metadata changes and history migration. claude-swap explicitly releases its bootstrap lock before handing control to Claude; inheriting that lock would obstruct later account-management operations.

Relay likewise releases registry locks before conversation launch. It intentionally retains separate runtime history/account locks across `exec`, because its initial policy forbids overlapping writers and concurrent login renewal. These are Relay locks, not Claude credential-refresh locks; Claude remains responsible for refresh. Integration tests verify the locks survive execution, default selection can still change, imports/renewal are refused when appropriate, and process termination releases locks once inherited descendants exit.

The history-store-wide writer lock is a deliberate restriction beyond the reference implementations. A project-only lock cannot identify which conversation a user will choose in Claude's cross-project resume picker. Per-conversation concurrency remains future work.

## Validation boundary

Fake-agent tests establish Relay's filesystem, dispatch, argument, identity-checking, and recovery behavior. They do not establish that every Claude version preserves or discovers a linked transcript identically. The separate live acceptance procedure must validate that boundary with authorized accounts.
