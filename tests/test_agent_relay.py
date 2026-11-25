"""Tests for scripts/agent_relay.py, ported from the Rust agent-relay's tests/cli.rs.

    /usr/bin/python3 -m unittest discover -s tests -v

The CLI tests run agent-relay as a black box: an installed copy, started through
the same `#!<Python.app> -I` line the installer writes. Set
AGENT_RELAY_BIN to run them against another build instead (e.g. the Rust binary,
to check both behave the same). A fake `claude` shell script stands in for Claude.
"""

import fcntl
import hashlib
import io
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.dont_write_bytecode = True
sys.path.insert(0, str(REPO))

import agent_relay  # noqa: E402

FAKE_CLAUDE = r"""#!/bin/sh
set -eu
if [ "${1:-}" = --version ]; then
  printf '%s (Claude Code)\n' "${FAKE_VERSION:-2.1.300}"
  exit 0
fi
if [ "${1:-}" = auth ] && [ "${2:-}" = login ]; then
  if [ -n "${FAKE_LOGIN_LOG:-}" ]; then printf 'login\n' >> "$FAKE_LOGIN_LOG"; fi
  if [ "${FAKE_FAIL_LOGIN:-}" = yes ]; then exit 1; fi
  printf '%s' "${FAKE_EMAIL:-personal@example.test}" > "$CLAUDE_CONFIG_DIR/.identity"
  printf '%s' "credential-${FAKE_EMAIL:-personal@example.test}" > "$CLAUDE_CONFIG_DIR/.credentials.json"
  exit 0
fi
if [ "${1:-}" = auth ] && [ "${2:-}" = status ]; then
  if [ "${FAKE_FAIL_AUTH:-}" = yes ]; then
    printf 'sensitive-api-token' >&2
    exit 1
  fi
  if [ "${FAKE_INVALID_AUTH:-}" = yes ]; then printf 'sensitive-api-token'; exit 0; fi
  if [ "${FAKE_INCOMPLETE_AUTH:-}" = yes ]; then printf '{"loggedIn":false,"authMethod":"none"}'; exit 1; fi
  email=$(cat "$CLAUDE_CONFIG_DIR/.identity")
  logged_in=true
  status_exit=0
  if [ "${FAKE_ACCESS_EXPIRED:-}" = yes ]; then logged_in=false; status_exit=${FAKE_STATUS_EXIT:-1}; fi
  printf '{"loggedIn":%s,"authMethod":"%s","configDirectory":"%s","subscriptionType":"%s","email":"%s","orgId":"org-fixture"}\n' \
    "$logged_in" "${FAKE_AUTH_METHOD:-claude.ai}" "${FAKE_CONFIG_DIR:-$CLAUDE_CONFIG_DIR}" "${FAKE_SUBSCRIPTION:-max}" "$email"
  exit "$status_exit"
fi
if [ "${FAKE_ACCESS_EXPIRED:-}" = yes ]; then
  if [ "${FAKE_REFRESH_REVOKED:-}" = yes ]; then printf 'refresh credential revoked' >&2; exit 42; fi
  printf 'refreshed-by-claude' > "$CLAUDE_CONFIG_DIR/.credentials.json"
  printf 'automatic-refresh\n'
fi
if [ -n "${FAKE_MARKER:-}" ]; then printf 'started' > "$FAKE_MARKER"; fi
if [ "${FAKE_WAIT:-}" = yes ]; then
  trap 'exit 143' TERM
  trap 'exit 130' INT
  while :; do sleep 0.05; done
fi
if [ -n "${FAKE_ARGS_FILE:-}" ]; then printf '%s\n' "$@" > "$FAKE_ARGS_FILE"; fi
if [ -n "${FAKE_ENV_FILE:-}" ]; then /usr/bin/env > "$FAKE_ENV_FILE"; fi
if [ "${FAKE_SIGPIPE:-}" = yes ]; then
  { yes || printf 'yes-exit=%s\n' "$?" >&2; } | head -n 1 > /dev/null
fi
printf 'account=%s\n' "$(cat "$CLAUDE_CONFIG_DIR/.identity")"
printf 'cwd=%s\n' "$PWD"
printf '{"display":"fixture prompt"}\n' >> "$CLAUDE_CONFIG_DIR/history.jsonl"
mkdir -p "$CLAUDE_CONFIG_DIR/projects/fixture-project"
transcript="$CLAUDE_CONFIG_DIR/projects/fixture-project/session-fixture.jsonl"
case "${1:-}" in
  --continue|-c|--resume|-r)
    if [ ! -f "$transcript" ]; then printf 'session not found' >&2; exit 9; fi
    cat "$transcript"
    ;;
  *) printf '{"sessionId":"session-fixture","message":"remember this","toolResult":"fixture-result"}\n' > "$transcript" ;;
esac
exit "${FAKE_EXIT_CODE:-0}"
"""


def python_app() -> str:
    """The interpreter the installed copy runs on, found the way the installer finds it."""
    return subprocess.run(
        ["/usr/bin/python3", "-I", "-c", 'import sys; print(sys.prefix + "/Resources/Python.app/Contents/MacOS/Python")'],
        capture_output=True, text=True, check=True,
    ).stdout.strip()


def setUpModule():
    global BINARY, INSTALL
    if os.environ.get("AGENT_RELAY_BIN"):
        BINARY = os.environ["AGENT_RELAY_BIN"]
        return
    INSTALL = tempfile.TemporaryDirectory()
    BINARY = os.path.join(INSTALL.name, "agent-relay")
    source = (REPO / "agent_relay.py").read_text()
    Path(BINARY).write_text(f"#!{python_app()} -I\n{source}")
    os.chmod(BINARY, 0o755)


def tearDownModule():
    if "INSTALL" in globals():
        INSTALL.cleanup()


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def hash_file(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as file:
        for chunk in iter(lambda: file.read(MIB), b""):
            digest.update(chunk)
    return digest.hexdigest()


MIB = 1024 * 1024


class Fixture:
    def __init__(self, test: unittest.TestCase):
        temp = tempfile.TemporaryDirectory()
        test.addCleanup(temp.cleanup)
        self.test = test
        base = Path(temp.name).resolve()
        self.home = base / "home"
        self.project = base / "project with spaces"
        self.home.mkdir()
        self.project.mkdir()
        self.claude = base / "fake claude"
        self.claude.write_text(FAKE_CLAUDE)
        self.claude.chmod(0o700)
        self.root = base / "relay data"

    def argv(self, args):
        return [BINARY, "--root", str(self.root), "--claude", str(self.claude), *map(str, args)]

    def env(self, extra):
        return {"HOME": str(self.home), "PATH": "/usr/bin:/bin", **{k: str(v) for k, v in extra.items()}}

    def run(self, *args, cwd=None, **env) -> subprocess.CompletedProcess:
        return subprocess.run(
            self.argv(args), env=self.env(env), cwd=cwd or self.project,
            stdin=subprocess.DEVNULL, capture_output=True,
        )

    def start(self, *args, **env) -> subprocess.Popen:
        process = subprocess.Popen(
            self.argv(args), env=self.env(env), cwd=self.project,
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        self.test.addCleanup(lambda: (process.kill(), process.wait()))
        return process

    def add(self, name):
        self.test.success(self.run("account", "add", name, FAKE_EMAIL=f"{name}@example.test"))

    def account(self, name) -> Path:
        return self.root / "accounts" / name / "claude"

    def backup(self) -> Path:
        return next((self.root / "backups").iterdir())


class RelayTest(unittest.TestCase):
    def success(self, output):
        self.assertEqual(output.returncode, 0, f"stdout={output.stdout.decode()} stderr={output.stderr.decode()}")

    def failure(self, output, message):
        self.assertNotEqual(output.returncode, 0)
        self.assertIn(message, output.stderr.decode(), output.stderr.decode())


# --- ported from tests/cli.rs ------------------------------------------------------


class Cli(RelayTest):
    def test_subscription_switch_keeps_transcript_and_tool_results_for_continue_and_resume(self):
        f = Fixture(self)
        f.add("personal")
        f.add("work")
        self.success(f.run("run", "personal", "--", "remember this"))
        for args in (["--continue"], ["--resume", "session-fixture"], ["--resume"]):
            output = f.run("run", "work", "--", *args)
            self.success(output)
            text = output.stdout.decode()
            self.assertIn("account=work@example.test", text)
            self.assertIn("remember this", text)
            self.assertIn("fixture-result", text)
        personal, work = f.account("personal"), f.account("work")
        self.assertEqual(os.readlink(personal / "projects"), os.readlink(work / "projects"))
        self.assertNotEqual((personal / ".credentials.json").read_bytes(), (work / ".credentials.json").read_bytes())
        self.assertEqual(os.readlink(work / "history.jsonl"), os.readlink(personal / "history.jsonl"))
        self.assertEqual((work / "history.jsonl").read_bytes(), (personal / "history.jsonl").read_bytes())
        self.assertEqual(len((work / "history.jsonl").read_text().splitlines()), 4)
        self.assertFalse((f.home / ".claude").exists())

    def test_default_launch_has_the_same_checks_and_shared_history(self):
        f = Fixture(self)
        f.add("personal")
        f.add("work")
        self.success(f.run("run", "personal"))
        self.success(f.run("account", "default", "work"))
        output = f.run("run", "--", "--continue")
        self.success(output)
        self.assertIn("account=work@example.test", output.stdout.decode())
        output = f.run("run", ANTHROPIC_API_KEY="sensitive-key")
        self.failure(output, "ANTHROPIC_API_KEY")
        self.assertNotIn("sensitive-key", output.stderr.decode())

    def test_rejects_every_known_environment_override_for_both_login_and_run(self):
        f = Fixture(self)
        f.add("personal")
        for key in [
            "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL", "ANTHROPIC_PROFILE",
            "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
            "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR", "CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY", "CLAUDE_CODE_USE_MANTLE", "CLAUDE_SECURESTORAGE_CONFIG_DIR",
            "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "CLAUDE_CODE_SKIP_PROMPT_HISTORY",
            "CLAUDE_CODE_SUBPROCESS_ENV_SCRUB",
        ]:
            for args in (["run", "personal"], ["account", "add", "work"]):
                output = f.run(*args, **{key: "sensitive-value"})
                self.failure(output, key)
                self.assertNotIn("sensitive-value", output.stderr.decode())

    def test_rejects_auth_method_plan_directory_and_identity_mismatches(self):
        f = Fixture(self)
        f.add("personal")
        for method in ["api_key", "api_key_helper", "third_party", "oauth_token", "none"]:
            self.failure(f.run("run", FAKE_AUTH_METHOD=method), "subscription login")
        self.failure(f.run("run", FAKE_SUBSCRIPTION="free"), "recognized paid subscription")
        self.failure(f.run("run", FAKE_CONFIG_DIR=f.home), "different config directory")
        (f.account("personal") / ".identity").write_text("someone-else@example.test")
        self.failure(f.run("run"), "identity changed")

    def test_failed_and_duplicate_logins_never_register_an_account_or_print_secrets(self):
        f = Fixture(self)
        self.failure(f.run("account", "add", "personal", FAKE_FAIL_LOGIN="yes"), "login failed")
        self.assertFalse((f.root / "config.json").exists())
        f.add("personal")
        self.failure(f.run("account", "add", "work"), "already registered")
        output = f.run("run", FAKE_FAIL_AUTH="yes")
        self.failure(output, "Cannot verify saved login")
        self.assertNotIn("sensitive-api-token", output.stderr.decode())
        output = f.run("run", FAKE_INVALID_AUTH="yes")
        self.failure(output, "Unrecognized Claude auth status")
        self.assertNotIn("sensitive-api-token", output.stderr.decode())

    def test_reauthentication_cannot_silently_change_registered_identity(self):
        f = Fixture(self)
        f.add("personal")
        self.failure(f.run("account", "login", "personal", FAKE_EMAIL="wrong@example.test"), "different identity")
        self.failure(f.run("run"), "identity changed")
        self.success(f.run("account", "login", "personal", FAKE_EMAIL="personal@example.test"))
        self.success(f.run("doctor"))

    def test_settings_conflicts_are_rejected_at_account_project_and_ancestor_levels(self):
        f = Fixture(self)
        f.add("personal")
        for path in [
            f.account("personal") / "settings.json",
            f.project / ".claude/settings.local.json",
            f.project.parent / ".claude/settings.json",
        ]:
            path.parent.mkdir(parents=True, exist_ok=True)
            for settings in [
                {"env": {"ANTHROPIC_API_KEY": "sensitive-value"}},
                {"apiKeyHelper": "sensitive-value"},
                {"env": {"CLAUDE_CONFIG_DIR": "elsewhere"}},
                {"forceLoginMethod": "console"},
                {"env": {"CLAUDE_CODE_USE_VERTEX": "1"}},
            ]:
                path.write_text(json.dumps(settings))
                output = f.run("run")
                self.failure(output, "Conflicting")
                self.assertNotIn("sensitive-value", output.stderr.decode())
            path.write_text("invalid JSON containing sensitive-value")
            output = f.run("run")
            self.failure(output, "Invalid settings")
            self.assertNotIn("sensitive-value", output.stderr.decode())
            path.unlink()

    def test_unsafe_cli_overrides_are_rejected_but_arguments_and_exit_codes_are_preserved(self):
        f = Fixture(self)
        f.add("personal")
        for flag in ["--settings={}", "--setting-sources=user", "--bare", "--no-session-persistence", "--worktree", "--bg"]:
            self.failure(f.run("run", "--", flag), "unsupported")
        args_file = f.home / "arguments"
        args = ["a spaced prompt", "", "$(touch SHOULD_NOT_EXIST)", "--model", "sonnet"]
        output = f.run("run", "--", *args, FAKE_ARGS_FILE=args_file, FAKE_EXIT_CODE="37")
        self.assertEqual(output.returncode, 37)
        self.assertEqual(args_file.read_text(), "\n".join(args) + "\n")
        self.assertFalse((f.project / "SHOULD_NOT_EXIST").exists())
        self.assertIn(f"cwd={f.project}", output.stdout.decode())

    def start_waiting(self, f):
        marker = f.home / "started"
        process = f.start("run", "personal", FAKE_WAIT="yes", FAKE_MARKER=marker)
        deadline = time.monotonic() + 5
        while not marker.exists():
            self.assertIsNone(process.poll(), "fixture child exited before starting")
            self.assertLess(time.monotonic(), deadline, "fixture did not start")
            time.sleep(0.02)
        return process

    def test_history_writer_lock_survives_exec_and_blocks_cross_project_resume_import_and_relogin(self):
        f = Fixture(self)
        f.add("personal")
        f.add("work")
        running = self.start_waiting(f)
        self.failure(f.run("run", "work"), "Another Relay session")
        alias = f.home / "project-alias"
        alias.symlink_to(f.project)
        self.failure(f.run("run", "work", cwd=alias), "Another Relay session")
        self.failure(f.run("account", "login", "personal"), "login or account update")
        self.success(f.run("account", "default", "work"))
        self.assertIsNone(running.poll())
        source = f.home / "source"
        source.mkdir()
        self.failure(f.run("history", "import", source, "--agents-stopped"), "History is in use")
        different = f.home / "another-project"
        different.mkdir()
        self.failure(f.run("run", "work", cwd=different), "Another Relay session")
        # exec preserves the Claude process's actual signal status, with no wrapper translating it.
        os.kill(running.pid, signal.SIGKILL)
        self.assertEqual(running.wait(), -signal.SIGKILL)
        # Short-lived shell descendants can retain a lock briefly after their parent dies.
        time.sleep(0.15)
        self.success(f.run("run", "work"))

    def test_rejects_symlinked_storage_and_preserves_existing_unlinked_history(self):
        f = Fixture(self)
        f.add("personal")
        projects = f.account("personal") / "projects"
        projects.unlink()
        projects.mkdir()
        (projects / "precious.jsonl").write_text("precious")
        self.failure(f.run("run"), "existing data was left untouched")
        self.assertEqual((projects / "precious.jsonl").read_text(), "precious")
        f = Fixture(self)
        f.root.mkdir(mode=0o700)
        (f.root / "accounts").symlink_to(f.home)
        self.failure(f.run("account", "add", "personal"), "Unsafe directory")

    def test_corrupt_registry_is_not_reset_and_unsupported_versions_refuse_launch(self):
        f = Fixture(self)
        f.add("personal")
        self.failure(f.run("run", FAKE_VERSION="2.1.267"), "Unsupported Claude version")
        self.failure(f.run("run", FAKE_VERSION="2.2.0"), "Unsupported Claude version")
        (f.root / "config.json").write_text("corrupt")
        self.failure(f.run("account", "list"), "Invalid config.json")
        self.assertEqual((f.root / "config.json").read_text(), "corrupt")

    def test_imports_history_without_overwriting_conflicts_or_changing_the_source(self):
        f = Fixture(self)
        f.add("personal")
        source = f.home / "old projects"
        (source / "old-project").mkdir(parents=True)
        (source / "old-project/session.jsonl").write_text("original transcript")
        shared = f.root / "shared/claude/projects/old-project"
        shared.mkdir()
        (shared / "session.jsonl").write_text("existing transcript")
        (source / "old-project/another.jsonl").write_text("another transcript")
        self.success(f.run("history", "import", source, "--agents-stopped"))
        self.assertEqual((shared / "session.jsonl").read_text(), "existing transcript")
        self.assertEqual((shared / "another.jsonl").read_text(), "another transcript")
        self.assertEqual((source / "old-project/session.jsonl").read_text(), "original transcript")
        operation = f.backup()
        self.assertEqual((operation / "snapshot/old-project/session.jsonl").read_text(), "original transcript")
        self.assertEqual((operation / "conflicts/old-project/session.jsonl").read_text(), "original transcript")
        self.success(f.run("history", "recover", operation.name, "--agents-stopped"))

    def test_interrupted_import_resumes_from_verified_snapshot_and_detects_tampering(self):
        f = Fixture(self)
        f.add("personal")
        source = f.home / "source"
        source.mkdir()
        (source / "session.jsonl").write_text("recover me")
        self.success(f.run("history", "import", source, "--agents-stopped"))
        operation = f.backup()
        manifest_path = operation / "manifest.json"
        manifest = json.loads(manifest_path.read_text())
        manifest["phase"] = "Merging"
        manifest_path.write_text(json.dumps(manifest))
        (f.root / "shared/claude/projects/session.jsonl").unlink()
        (source / "session.jsonl").unlink()
        source.rmdir()
        self.success(f.run("history", "recover", operation.name, "--agents-stopped"))
        self.assertEqual((f.root / "shared/claude/projects/session.jsonl").read_text(), "recover me")
        manifest_path.write_text(json.dumps(manifest))
        (operation / "snapshot/session.jsonl").write_text("tampered")
        self.failure(f.run("history", "recover", operation.name, "--agents-stopped"), "backup verification failed")
        self.assertEqual((f.root / "shared/claude/projects/session.jsonl").read_text(), "recover me")

    def test_import_requires_confirmation_and_rejects_symlinks_and_path_traversal(self):
        f = Fixture(self)
        f.add("personal")
        source = f.home / "source"
        source.mkdir()
        self.failure(f.run("history", "import", source), "--agents-stopped")
        (source / "credential-link").symlink_to(f.account("personal") / ".credentials.json")
        self.failure(f.run("history", "import", source, "--agents-stopped"), "contains a symlink")
        self.failure(f.run("history", "recover", "import-../../escape", "--agents-stopped"), "Invalid recovery ID")

    def test_account_names_cannot_collide_by_case_or_escape_storage(self):
        f = Fixture(self)
        for name in ["Personal", "PERSONAL", "../personal", "a/b", ".hidden", "", "1account"]:
            self.failure(f.run("account", "add", name), "Account names must")
        f.add("personal")
        credentials = (f.account("personal") / ".credentials.json").read_bytes()
        self.failure(f.run("account", "add", "Personal"), "Account names must")
        self.assertEqual((f.account("personal") / ".credentials.json").read_bytes(), credentials)

    def test_real_failed_merge_keeps_snapshot_and_can_be_recovered_without_source(self):
        f = Fixture(self)
        f.add("personal")
        source = f.home / "source"
        source.mkdir()
        (source / "session.jsonl").write_text("recover after merge failure")
        destination = f.root / "shared/claude/projects/session.jsonl"
        destination.symlink_to(f.home / "unrelated")
        self.failure(f.run("history", "import", source, "--agents-stopped"), "Unsafe history symlink")
        operation = f.backup()
        destination.unlink()
        (source / "session.jsonl").unlink()
        source.rmdir()
        self.success(f.run("history", "recover", operation.name, "--agents-stopped"))
        self.assertEqual(destination.read_text(), "recover after merge failure")

    def test_incomplete_snapshot_recovery_rejects_changed_sources_before_merging_anything(self):
        f = Fixture(self)
        f.add("personal")
        source = f.home / "source"
        source.mkdir()
        (source / "a.jsonl").write_text("first")
        (source / "b.jsonl").write_text("changed")
        operation = f.root / "backups/import-fixture"
        (operation / "snapshot").mkdir(parents=True)
        (operation / "snapshot/a.jsonl").write_text("first")
        manifest = {"version": 1, "source": str(source), "phase": "Snapshotting", "entries": [
            {"path": "a.jsonl", "sha256": sha256(b"first")},
            {"path": "b.jsonl", "sha256": sha256(b"second")},
        ]}
        (operation / "manifest.json").write_text(json.dumps(manifest))
        self.failure(f.run("history", "recover", "import-fixture", "--agents-stopped"), "History changed")
        self.assertFalse((f.root / "shared/claude/projects/a.jsonl").exists())
        (source / "b.jsonl").write_text("second")
        self.success(f.run("history", "recover", "import-fixture", "--agents-stopped"))
        self.assertEqual((f.root / "shared/claude/projects/b.jsonl").read_text(), "second")

    def test_worktree_launch_checks_local_settings_in_the_main_checkout(self):
        f = Fixture(self)
        f.add("personal")
        main = f.home / "main checkout"
        gitdir = main / ".git/worktrees/feature"
        gitdir.mkdir(parents=True)
        (main / ".claude").mkdir()
        (gitdir / "commondir").write_text("../..\n")
        (f.project / ".git").write_text(f"gitdir: {gitdir}\n")
        (main / ".claude/settings.local.json").write_text('{"apiKeyHelper":"sensitive-value"}')
        output = f.run("run")
        self.failure(output, "apiKeyHelper")
        self.assertNotIn("sensitive-value", output.stderr.decode())

    def test_inconclusive_auth_verification_never_deletes_credentials_or_transcripts(self):
        f = Fixture(self)
        f.add("personal")
        self.success(f.run("run"))
        home = f.account("personal")
        credential = (home / ".credentials.json").read_bytes()
        transcript_path = home / "projects/fixture-project/session-fixture.jsonl"
        transcript = transcript_path.read_bytes()
        self.failure(f.run("run", FAKE_INVALID_AUTH="yes"), "Unrecognized Claude auth status")
        self.assertEqual((home / ".credentials.json").read_bytes(), credential)
        self.assertEqual(transcript_path.read_bytes(), transcript)
        self.success(f.run("run", "--", "--continue"))

    def test_importing_an_account_home_is_refused_before_credentials_can_be_copied(self):
        f = Fixture(self)
        f.add("personal")
        source = f.home / "old-account"
        (source / "projects").mkdir(parents=True)
        (source / ".credentials.json").write_text("sensitive-credential")
        (source / "projects/session.jsonl").write_text("conversation")
        self.failure(f.run("history", "import", source, "--agents-stopped"), "import its projects/ directory")
        self.assertEqual(list((f.root / "backups").iterdir()), [])
        self.assertFalse((f.root / "shared/claude/projects/.credentials.json").exists())

    def test_switching_and_failed_status_checks_never_invoke_login(self):
        f = Fixture(self)
        log = f.home / "login-invocations"
        for name in ["personal", "work"]:
            self.success(f.run("account", "add", name, FAKE_EMAIL=f"{name}@example.test", FAKE_LOGIN_LOG=log))
        self.assertEqual(len(log.read_text().splitlines()), 2)
        self.success(f.run("run", "personal", FAKE_LOGIN_LOG=log))
        for name in ["work", "personal", "work"]:
            self.success(f.run("run", name, "--", "--continue", FAKE_LOGIN_LOG=log))
        output = f.run("run", "personal", FAKE_FAIL_AUTH="yes", FAKE_LOGIN_LOG=log)
        self.failure(output, "status-check failure does not prove")
        self.assertNotIn("account login", output.stderr.decode())
        self.assertEqual(len(log.read_text().splitlines()), 2)

    def test_expired_access_token_reaches_claude_refresh_and_resumes_without_login(self):
        f = Fixture(self)
        log = f.home / "login-invocations"
        for name in ["personal", "work"]:
            self.success(f.run("account", "add", name, FAKE_EMAIL=f"{name}@example.test", FAKE_LOGIN_LOG=log))
        self.success(f.run("run", "work"))
        work_credential = (f.account("work") / ".credentials.json").read_bytes()
        for status_exit in ["0", "1"]:
            output = f.run(
                "run", "personal", "--", "--continue",
                FAKE_ACCESS_EXPIRED="yes", FAKE_STATUS_EXIT=status_exit, FAKE_LOGIN_LOG=log,
            )
            self.success(output)
            text = output.stdout.decode()
            self.assertIn("automatic-refresh", text)
            self.assertIn("account=personal@example.test", text)
            self.assertIn("remember this", text)
            self.assertIn("fixture-result", text)
            self.assertEqual((f.account("personal") / ".credentials.json").read_text(), "refreshed-by-claude")
        self.success(f.run("account", "default", "personal"))
        self.success(f.run("run", "--", "--resume", "session-fixture", FAKE_ACCESS_EXPIRED="yes", FAKE_LOGIN_LOG=log))
        self.assertEqual(len(log.read_text().splitlines()), 2)
        self.assertEqual((f.account("work") / ".credentials.json").read_bytes(), work_credential)

    def test_refreshable_status_does_not_bypass_provider_identity_or_plan_checks(self):
        f = Fixture(self)
        f.add("personal")
        for method in ["api_key", "apiKey", "api_key_helper", "third_party", "oauth_token", "none"]:
            self.failure(f.run("run", FAKE_ACCESS_EXPIRED="yes", FAKE_AUTH_METHOD=method), "claude.ai subscription login")
        self.failure(f.run("run", FAKE_ACCESS_EXPIRED="yes", FAKE_SUBSCRIPTION="free"), "recognized paid subscription")
        self.failure(f.run("run", FAKE_ACCESS_EXPIRED="yes", FAKE_CONFIG_DIR=f.home), "different config directory")
        self.failure(f.run("run", FAKE_ACCESS_EXPIRED="yes", ANTHROPIC_API_KEY="sensitive-value"), "ANTHROPIC_API_KEY")
        self.failure(f.run("run", FAKE_INCOMPLETE_AUTH="yes"), "Unrecognized Claude auth status")
        (f.account("personal") / ".identity").write_text("other@example.test")
        self.failure(f.run("run", FAKE_ACCESS_EXPIRED="yes"), "identity changed")

    def test_account_registration_and_doctor_still_require_an_active_verified_login(self):
        f = Fixture(self)
        output = f.run("account", "add", "personal", FAKE_ACCESS_EXPIRED="yes", FAKE_STATUS_EXIT="0")
        self.failure(output, "active claude.ai subscription login")
        self.assertFalse((f.root / "config.json").exists())
        f.add("personal")
        self.failure(f.run("doctor", FAKE_ACCESS_EXPIRED="yes", FAKE_STATUS_EXIT="0"), "active claude.ai subscription login")

    def test_rejected_refresh_is_left_to_claude_without_relogin_or_credential_deletion(self):
        f = Fixture(self)
        f.add("personal")
        self.success(f.run("run"))
        credential_path = f.account("personal") / ".credentials.json"
        credential = credential_path.read_bytes()
        transcript = f.root / "shared/claude/projects/fixture-project/session-fixture.jsonl"
        saved = transcript.read_bytes()
        login_log = f.home / "unexpected-login"
        output = f.run(
            "run", "--", "--continue",
            FAKE_ACCESS_EXPIRED="yes", FAKE_REFRESH_REVOKED="yes", FAKE_LOGIN_LOG=login_log,
        )
        self.assertEqual(output.returncode, 42)
        self.assertIn("refresh credential revoked", output.stderr.decode())
        self.assertFalse(login_log.exists())
        self.assertEqual(credential_path.read_bytes(), credential)
        self.assertEqual(transcript.read_bytes(), saved)

    def test_history_larger_than_64_mib_imports_recovers_and_preserves_conflicts(self):
        f = Fixture(self)
        f.add("personal")
        source = f.home / "old-projects"
        (source / "project").mkdir(parents=True)
        source_file = source / "project/large.jsonl"
        size = 65 * MIB + 17
        with open(source_file, "wb") as file:
            file.truncate(size)
            file.write(b"history start\n")
            file.seek(size - 13)
            file.write(b"history end\n\n")
        expected = hash_file(source_file)
        self.success(f.run("history", "import", source, "--agents-stopped"))
        operation = f.backup()
        snapshot = operation / "snapshot/project/large.jsonl"
        destination = f.root / "shared/claude/projects/project/large.jsonl"
        self.assertEqual(snapshot.stat().st_size, size)
        self.assertEqual(destination.stat().st_size, size)
        self.assertEqual(hash_file(snapshot), expected)
        self.assertEqual(hash_file(destination), expected)

        # Recheck an already-copied large snapshot without the original source.
        manifest_path = operation / "manifest.json"
        manifest = json.loads(manifest_path.read_text())
        manifest["phase"] = "Snapshotting"
        manifest_path.write_text(json.dumps(manifest))
        source_file.unlink()
        (source / "project").rmdir()
        source.rmdir()
        self.success(f.run("history", "recover", operation.name, "--agents-stopped"))

        # A changed destination remains untouched while the large incoming copy is retained.
        destination.write_bytes(b"keep current history")
        manifest["phase"] = "Merging"
        manifest_path.write_text(json.dumps(manifest))
        self.success(f.run("history", "recover", operation.name, "--agents-stopped"))
        conflict = operation / "conflicts/project/large.jsonl"
        self.assertEqual(destination.read_bytes(), b"keep current history")
        self.assertEqual(hash_file(conflict), expected)

        # Corruption beyond the old size limit must not publish or overwrite a conflict copy.
        with open(snapshot, "r+b") as file:
            file.seek(64 * MIB + 1)
            file.write(b"changed")
        manifest_path.write_text(json.dumps(manifest))
        self.failure(f.run("history", "recover", operation.name, "--agents-stopped"), "backup verification failed")
        self.assertEqual(destination.read_bytes(), b"keep current history")
        self.assertEqual(hash_file(conflict), expected)
        self.assertEqual(len(list((operation / "conflicts/project").iterdir())), 1)


# --- new: what a Python port could get wrong ---------------------------------------


class PortFidelity(RelayTest):
    def test_claude_gets_exactly_the_callers_environment(self):
        # Python sets LC_CTYPE and __CF_USER_TEXT_ENCODING in its own environment, and the
        # /usr/bin/python3 stub adds SDKROOT, CPATH, ...; none of that may reach Claude.
        f = Fixture(self)
        f.add("personal")
        env_file = f.home / "environment"
        output = f.run(
            "run",
            FAKE_ENV_FILE=env_file, CLAUDECODE="1", CLAUDE_CODE_SESSION_ID="outer",
            CLAUDE_CODE_PARENT_SESSION_ID="outer-parent", EMPTY="", SPACED="a b",
        )
        self.success(output)
        seen = dict(line.split("=", 1) for line in env_file.read_text().splitlines())
        for shell_variable in ["PWD", "OLDPWD", "SHLVL", "_"]:  # set by the fake's /bin/sh
            seen.pop(shell_variable, None)
        self.assertEqual(seen, {
            "HOME": str(f.home), "PATH": "/usr/bin:/bin", "EMPTY": "", "SPACED": "a b",
            "FAKE_ENV_FILE": str(env_file), "CLAUDE_CONFIG_DIR": str(f.account("personal")),
        })

    def test_claude_starts_with_default_sigpipe(self):
        # Rust resets SIGPIPE before exec; Python ignores it unless told otherwise. With
        # SIGPIPE ignored, `yes` would see EPIPE and exit 1 instead of dying by signal (141).
        f = Fixture(self)
        f.add("personal")
        output = f.run("run", FAKE_SIGPIPE="yes")
        self.success(output)
        self.assertIn("yes-exit=141", output.stderr.decode())

    def test_registry_file_matches_the_rust_format(self):
        # Byte for byte what the Rust binary writes, so the existing data on the Macs keeps working.
        f = Fixture(self)
        f.add("personal")
        f.add("work")
        self.success(f.run("account", "default", "work"))
        self.assertEqual((f.root / "config.json").read_text(), (
            '{\n  "version": 1,\n  "default": "work",\n  "accounts": {\n'
            '    "personal": {\n      "email": "personal@example.test",\n      "org_id": "org-fixture",\n'
            '      "subscription": "max"\n    },\n'
            '    "work": {\n      "email": "work@example.test",\n      "org_id": "org-fixture",\n'
            '      "subscription": "max"\n    }\n  }\n}\n'
        ))
        self.assertEqual(
            f.run("account", "list").stdout.decode(),
            "  personal\tpersonal@example.test\tmax\n* work\twork@example.test\tmax\n",
        )

    def test_reads_a_registry_written_by_rust(self):
        f = Fixture(self)
        f.add("personal")
        (f.root / "config.json").write_text(
            '{\n  "version": 1,\n  "default": "personal",\n  "accounts": {\n    "personal": {\n'
            '      "email": "PERSONAL@example.test",\n      "org_id": "org-fixture",\n      "subscription": "max"\n'
            "    }\n  }\n}\n"
        )
        self.success(f.run("doctor"))  # stored email differs only in ASCII case

    def test_registry_is_strict_like_serde(self):
        f = Fixture(self)
        f.add("personal")
        for registry in [
            '{"version":1,"default":null,"accounts":{},"extra":1}',  # unknown field
            '{"version":1,"version":1,"accounts":{}}',  # repeated field
            '{"version":1.0,"accounts":{}}',  # u32, not float
            '{"version":true,"accounts":{}}',
            '{"version":1}',  # missing field
            '{"version":1,"accounts":{"a":{"email":"x","subscription":"max","more":1}}}',
        ]:
            (f.root / "config.json").write_text(registry)
            self.failure(f.run("account", "list"), "Invalid config.json")
        (f.root / "config.json").write_text('{"version":2,"accounts":{}}')
        self.failure(f.run("account", "list"), "Unsupported registry version 2")

    def test_manifest_file_matches_the_rust_format(self):
        f = Fixture(self)
        f.add("personal")
        source = f.home / "source"
        (source / "a").mkdir(parents=True)
        (source / "a/x.jsonl").write_text("x")
        (source / "a-b.jsonl").write_text("y")
        self.success(f.run("history", "import", source, "--agents-stopped"))
        # Entries are ordered by path component, as Rust's PathBuf: "a/x" before "a-b".
        self.assertEqual((f.backup() / "manifest.json").read_text(), (
            '{\n  "version": 1,\n  "source": "%s",\n  "phase": "Complete",\n  "entries": [\n'
            '    {\n      "path": "a/x.jsonl",\n      "sha256": "%s"\n    },\n'
            '    {\n      "path": "a-b.jsonl",\n      "sha256": "%s"\n    }\n  ]\n}\n'
        ) % (source, sha256(b"x"), sha256(b"y")))

    def test_command_line_matches_clap(self):
        f = Fixture(self)
        f.add("personal")
        # Global options also work after the subcommand.
        self.success(subprocess.run(
            [BINARY, "account", "list", "--root", str(f.root)], env=f.env({}), capture_output=True,
        ))
        # Claude's arguments need `--`; later `--` are passed on untouched.
        args_file = f.home / "arguments"
        self.success(f.run("run", "personal", "--", "hello", "--", "-x", FAKE_ARGS_FILE=args_file))
        self.assertEqual(args_file.read_text(), "hello\n--\n-x\n")
        self.assertEqual(f.run("run", "personal", "--continue").returncode, 2)
        self.assertEqual(f.run("history", "import", f.home, "--agents").returncode, 2)  # no abbreviations
        self.assertEqual(f.run("account").returncode, 2)
        output = subprocess.run([BINARY, "--version"], capture_output=True)
        self.assertEqual(output.stdout.decode(), "agent-relay 0.1.0\n")

    def test_errors_print_like_rust(self):
        f = Fixture(self)
        output = f.run("history", "recover", "import-missing", "--agents-stopped")
        self.assertEqual(output.returncode, 1)
        self.assertEqual(output.stderr.decode(), (
            "[i] Relay locks cannot detect direct Claude processes. Import only after stopping them.\n"
            "[X] No such file or directory (os error 2)\n"
        ))

    def test_lock_contention_message_matches_rust(self):
        f = Fixture(self)
        f.add("personal")
        fd = os.open(f.root / "locks/registry.lock", os.O_RDWR)
        self.addCleanup(os.close, fd)
        fcntl.flock(fd, fcntl.LOCK_EX)
        self.assertEqual(
            f.run("account", "default", "personal").stderr.decode(),
            "[X] Another account update is in progress; retry: lock acquisition failed because the operation would block\n",
        )


# --- ported from the Rust unit tests -------------------------------------------------


class Units(unittest.TestCase):
    def test_failed_staged_write_leaves_original_and_cleans_up_partial_output(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp).resolve()
            target = root / "history.jsonl"
            target.write_bytes(b"original history")

            def write(output):
                output.write(b"partial copy")
                raise OSError("simulated write failure")

            with self.assertRaises(OSError):
                agent_relay.atomic_write_with(target, write)
            self.assertEqual(target.read_bytes(), b"original history")
            self.assertEqual(len(list(root.iterdir())), 1)

    def test_transfer_handles_interrupted_and_short_reads_with_a_bounded_buffer(self):
        class ShortReader:
            def __init__(self, data):
                self.input = io.BytesIO(data)
                self.interrupted = False
                self.largest_request = 0

            def read(self, size):
                self.largest_request = max(self.largest_request, size)
                if not self.interrupted:
                    self.interrupted = True
                    raise InterruptedError()
                return self.input.read(min(size, 3))

        reader = ShortReader(b"hello world")
        output = bytearray()
        digest = agent_relay.transfer(reader, output.extend)
        self.assertEqual(bytes(output), b"hello world")
        self.assertEqual(digest, "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9")
        self.assertEqual(reader.largest_request, MIB)

    def test_hash_mismatch_does_not_replace_existing_destination_or_leave_a_partial_file(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp).resolve()
            source, target = root / "source", root / "target"
            source.write_bytes(b"changed source")
            target.write_bytes(b"existing history")
            entry = agent_relay.Entry(path="source", sha256="0" * 64)
            with self.assertRaises(agent_relay.Fail):
                agent_relay.copy_verified(source, target, entry)
            self.assertEqual(target.read_bytes(), b"existing history")
            self.assertEqual(len(list(root.iterdir())), 2)

    def test_components_follow_rust(self):
        c = agent_relay.components
        self.assertEqual(c("a/./b//c/"), [("normal", "a"), ("normal", "b"), ("normal", "c")])
        self.assertEqual(c("./a"), [("cur", "."), ("normal", "a")])
        self.assertEqual(c("/../a"), [("root", "/"), ("parent", ".."), ("normal", "a")])
        self.assertEqual(c(""), [])

    def test_version_numbers_parse_like_rust(self):
        self.assertEqual(agent_relay.parse_u32("+268"), 268)
        for text in ["", "２", "1_0", " 1", "-1", "4294967296"]:
            with self.assertRaises(agent_relay.Fail):
                agent_relay.parse_u32(text)


if __name__ == "__main__":
    unittest.main()
