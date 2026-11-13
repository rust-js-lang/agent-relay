#![cfg(unix)]

use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    home: PathBuf,
    project: PathBuf,
    claude: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let base = temp.path().canonicalize()?;
        let home = base.join("home");
        let project = base.join("project with spaces");
        fs::create_dir(&home)?;
        fs::create_dir(&project)?;
        let claude = base.join("fake claude");
        fs::write(
            &claude,
            r##"#!/bin/sh
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
"##,
        )?;
        fs::set_permissions(&claude, fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            root: base.join("relay data"),
            home,
            project,
            claude,
            _temp: temp,
        })
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-relay"));
        cmd.env_clear()
            .env("HOME", &self.home)
            .env("PATH", "/usr/bin:/bin")
            .current_dir(&self.project)
            .arg("--root")
            .arg(&self.root)
            .arg("--claude")
            .arg(&self.claude)
            .args(args);
        cmd
    }

    fn add(&self, name: &str) -> TestResult {
        let output = self
            .command(&["account", "add", name])
            .env("FAKE_EMAIL", format!("{name}@example.test"))
            .output()?;
        success(&output);
        Ok(())
    }

    fn account(&self, name: &str) -> PathBuf {
        self.root.join("accounts").join(name).join("claude")
    }
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn failure(output: &Output, message: &str) {
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(message),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn subscription_switch_keeps_transcript_and_tool_results_for_continue_and_resume() -> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    f.add("work")?;
    success(
        &f.command(&["run", "personal", "--", "remember this"])
            .output()?,
    );
    for args in [
        vec!["--continue"],
        vec!["--resume", "session-fixture"],
        vec!["--resume"],
    ] {
        let mut command = f.command(&["run", "work", "--"]);
        let output = command.args(args).output()?;
        success(&output);
        let text = String::from_utf8(output.stdout)?;
        assert!(text.contains("account=work@example.test"));
        assert!(text.contains("remember this"));
        assert!(text.contains("fixture-result"));
    }
    let personal = f.account("personal");
    let work = f.account("work");
    assert_eq!(
        fs::read_link(personal.join("projects"))?,
        fs::read_link(work.join("projects"))?
    );
    assert_ne!(
        fs::read(personal.join(".credentials.json"))?,
        fs::read(work.join(".credentials.json"))?
    );
    assert_eq!(
        fs::read_link(work.join("history.jsonl"))?,
        fs::read_link(personal.join("history.jsonl"))?
    );
    assert_eq!(
        fs::read(work.join("history.jsonl"))?,
        fs::read(personal.join("history.jsonl"))?
    );
    assert_eq!(
        fs::read_to_string(work.join("history.jsonl"))?
            .lines()
            .count(),
        4
    );
    assert!(!f.home.join(".claude").exists());
    Ok(())
}

#[test]
fn default_launch_has_the_same_checks_and_shared_history() -> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    f.add("work")?;
    success(&f.command(&["run", "personal"]).output()?);
    success(&f.command(&["account", "default", "work"]).output()?);
    let output = f.command(&["run", "--", "--continue"]).output()?;
    success(&output);
    assert!(String::from_utf8(output.stdout)?.contains("account=work@example.test"));
    let output = f
        .command(&["run"])
        .env("ANTHROPIC_API_KEY", "sensitive-key")
        .output()?;
    failure(&output, "ANTHROPIC_API_KEY");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sensitive-key"));
    Ok(())
}

#[test]
fn rejects_every_known_environment_override_for_both_login_and_run() -> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    for key in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_PROFILE",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
        "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
        "CLAUDE_CODE_USE_MANTLE",
        "CLAUDE_SECURESTORAGE_CONFIG_DIR",
        "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
        "CLAUDE_CODE_SKIP_PROMPT_HISTORY",
        "CLAUDE_CODE_SUBPROCESS_ENV_SCRUB",
    ] {
        for args in [vec!["run", "personal"], vec!["account", "add", "work"]] {
            let output = f.command(&args).env(key, "sensitive-value").output()?;
            failure(&output, key);
            assert!(!String::from_utf8_lossy(&output.stderr).contains("sensitive-value"));
        }
    }
    Ok(())
}

#[test]
fn rejects_auth_method_plan_directory_and_identity_mismatches() -> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    for method in [
        "api_key",
        "api_key_helper",
        "third_party",
        "oauth_token",
        "none",
    ] {
        failure(
            &f.command(&["run"])
                .env("FAKE_AUTH_METHOD", method)
                .output()?,
            "subscription login",
        );
    }
    failure(
        &f.command(&["run"])
            .env("FAKE_SUBSCRIPTION", "free")
            .output()?,
        "recognized paid subscription",
    );
    failure(
        &f.command(&["run"])
            .env("FAKE_CONFIG_DIR", &f.home)
            .output()?,
        "different config directory",
    );
    fs::write(
        f.account("personal").join(".identity"),
        "someone-else@example.test",
    )?;
    failure(&f.command(&["run"]).output()?, "identity changed");
    Ok(())
}

#[test]
fn failed_and_duplicate_logins_never_register_an_account_or_print_secrets() -> TestResult {
    let f = Fixture::new()?;
    failure(
        &f.command(&["account", "add", "personal"])
            .env("FAKE_FAIL_LOGIN", "yes")
            .output()?,
        "login failed",
    );
    assert!(!f.root.join("config.json").exists());
    f.add("personal")?;
    failure(
        &f.command(&["account", "add", "work"]).output()?,
        "already registered",
    );
    let output = f.command(&["run"]).env("FAKE_FAIL_AUTH", "yes").output()?;
    failure(&output, "Cannot verify saved login");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sensitive-api-token"));
    let output = f
        .command(&["run"])
        .env("FAKE_INVALID_AUTH", "yes")
        .output()?;
    failure(&output, "Unrecognized Claude auth status");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sensitive-api-token"));
    Ok(())
}

#[test]
fn reauthentication_cannot_silently_change_registered_identity() -> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    failure(
        &f.command(&["account", "login", "personal"])
            .env("FAKE_EMAIL", "wrong@example.test")
            .output()?,
        "different identity",
    );
    failure(&f.command(&["run"]).output()?, "identity changed");
    success(
        &f.command(&["account", "login", "personal"])
            .env("FAKE_EMAIL", "personal@example.test")
            .output()?,
    );
    success(&f.command(&["doctor"]).output()?);
    Ok(())
}

#[test]
fn settings_conflicts_are_rejected_at_account_project_and_ancestor_levels() -> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    let locations = [
        f.account("personal").join("settings.json"),
        f.project.join(".claude/settings.local.json"),
        f.project
            .parent()
            .ok_or("missing parent")?
            .join(".claude/settings.json"),
    ];
    for path in locations {
        fs::create_dir_all(path.parent().ok_or("missing parent")?)?;
        for settings in [
            json!({"env":{"ANTHROPIC_API_KEY":"sensitive-value"}}),
            json!({"apiKeyHelper":"sensitive-value"}),
            json!({"env":{"CLAUDE_CONFIG_DIR":"elsewhere"}}),
            json!({"forceLoginMethod":"console"}),
            json!({"env":{"CLAUDE_CODE_USE_VERTEX":"1"}}),
        ] {
            fs::write(&path, serde_json::to_vec(&settings)?)?;
            let output = f.command(&["run"]).output()?;
            failure(&output, "Conflicting");
            assert!(!String::from_utf8_lossy(&output.stderr).contains("sensitive-value"));
        }
        fs::write(&path, "invalid JSON containing sensitive-value")?;
        let output = f.command(&["run"]).output()?;
        failure(&output, "Invalid settings");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("sensitive-value"));
        fs::remove_file(path)?;
    }
    Ok(())
}

#[test]
fn unsafe_cli_overrides_are_rejected_but_arguments_and_exit_codes_are_preserved() -> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    for flag in [
        "--settings={}",
        "--setting-sources=user",
        "--bare",
        "--no-session-persistence",
        "--worktree",
        "--bg",
    ] {
        failure(&f.command(&["run", "--", flag]).output()?, "unsupported");
    }
    let args_file = f.home.join("arguments");
    let args = [
        "a spaced prompt",
        "",
        "$(touch SHOULD_NOT_EXIST)",
        "--model",
        "sonnet",
    ];
    let output = f
        .command(&["run", "--"])
        .args(args)
        .env("FAKE_ARGS_FILE", &args_file)
        .env("FAKE_EXIT_CODE", "37")
        .output()?;
    assert_eq!(output.status.code(), Some(37));
    assert_eq!(
        fs::read_to_string(args_file)?,
        format!("{}\n", args.join("\n"))
    );
    assert!(!f.project.join("SHOULD_NOT_EXIST").exists());
    assert!(String::from_utf8(output.stdout)?.contains(&format!("cwd={}", f.project.display())));
    Ok(())
}

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_waiting(f: &Fixture) -> Result<Running, Box<dyn std::error::Error>> {
    let marker = f.home.join("started");
    let child = f
        .command(&["run", "personal"])
        .env("FAKE_WAIT", "yes")
        .env("FAKE_MARKER", &marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut running = Running(child);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        assert!(
            running.0.try_wait()?.is_none(),
            "fixture child exited before starting"
        );
        assert!(Instant::now() < deadline, "fixture did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(running)
}

#[test]
fn history_writer_lock_survives_exec_and_blocks_cross_project_resume_import_and_relogin()
-> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    f.add("work")?;
    let mut running = start_waiting(&f)?;
    failure(
        &f.command(&["run", "work"]).output()?,
        "Another Relay session",
    );
    let alias = f.home.join("project-alias");
    symlink(&f.project, &alias)?;
    failure(
        &f.command(&["run", "work"]).current_dir(alias).output()?,
        "Another Relay session",
    );
    failure(
        &f.command(&["account", "login", "personal"]).output()?,
        "login or account update",
    );
    success(&f.command(&["account", "default", "work"]).output()?);
    assert!(running.0.try_wait()?.is_none());
    let source = f.home.join("source");
    fs::create_dir(&source)?;
    failure(
        &f.command(&["history", "import"])
            .arg(&source)
            .arg("--agents-stopped")
            .output()?,
        "History is in use",
    );
    let different = f.home.join("another-project");
    fs::create_dir(&different)?;
    failure(
        &f.command(&["run", "work"])
            .current_dir(different)
            .output()?,
        "Another Relay session",
    );
    // exec preserves the Claude process's actual signal status, with no wrapper translating it.
    unsafe {
        libc::kill(running.0.id() as i32, libc::SIGKILL);
    }
    assert_eq!(running.0.wait()?.signal(), Some(libc::SIGKILL));
    // Short-lived shell descendants can retain a lock briefly after their parent dies.
    std::thread::sleep(Duration::from_millis(150));
    success(&f.command(&["run", "work"]).output()?);
    Ok(())
}

#[test]
fn rejects_symlinked_storage_and_preserves_existing_unlinked_history() -> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    let projects = f.account("personal").join("projects");
    fs::remove_file(&projects)?;
    fs::create_dir(&projects)?;
    fs::write(projects.join("precious.jsonl"), "precious")?;
    failure(
        &f.command(&["run"]).output()?,
        "existing data was left untouched",
    );
    assert_eq!(
        fs::read_to_string(projects.join("precious.jsonl"))?,
        "precious"
    );
    let f = Fixture::new()?;
    fs::create_dir(&f.root)?;
    fs::set_permissions(&f.root, fs::Permissions::from_mode(0o700))?;
    symlink(&f.home, f.root.join("accounts"))?;
    failure(
        &f.command(&["account", "add", "personal"]).output()?,
        "Unsafe directory",
    );
    Ok(())
}

#[test]
fn corrupt_registry_is_not_reset_and_unsupported_versions_refuse_launch() -> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    failure(
        &f.command(&["run"])
            .env("FAKE_VERSION", "2.1.267")
            .output()?,
        "Unsupported Claude version",
    );
    failure(
        &f.command(&["run"]).env("FAKE_VERSION", "2.2.0").output()?,
        "Unsupported Claude version",
    );
    fs::write(f.root.join("config.json"), "corrupt")?;
    failure(
        &f.command(&["account", "list"]).output()?,
        "Invalid config.json",
    );
    assert_eq!(fs::read_to_string(f.root.join("config.json"))?, "corrupt");
    Ok(())
}

#[test]
fn imports_history_without_overwriting_conflicts_or_changing_the_source() -> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    let source = f.home.join("old projects");
    fs::create_dir_all(source.join("old-project"))?;
    fs::write(
        source.join("old-project/session.jsonl"),
        "original transcript",
    )?;
    let shared = f.root.join("shared/claude/projects/old-project");
    fs::create_dir(&shared)?;
    fs::write(shared.join("session.jsonl"), "existing transcript")?;
    fs::write(
        source.join("old-project/another.jsonl"),
        "another transcript",
    )?;
    let output = f
        .command(&["history", "import"])
        .arg(&source)
        .arg("--agents-stopped")
        .output()?;
    success(&output);
    assert_eq!(
        fs::read_to_string(shared.join("session.jsonl"))?,
        "existing transcript"
    );
    assert_eq!(
        fs::read_to_string(shared.join("another.jsonl"))?,
        "another transcript"
    );
    assert_eq!(
        fs::read_to_string(source.join("old-project/session.jsonl"))?,
        "original transcript"
    );
    let operation = fs::read_dir(f.root.join("backups"))?
        .next()
        .ok_or("missing backup")??
        .path();
    assert_eq!(
        fs::read_to_string(operation.join("snapshot/old-project/session.jsonl"))?,
        "original transcript"
    );
    assert_eq!(
        fs::read_to_string(operation.join("conflicts/old-project/session.jsonl"))?,
        "original transcript"
    );
    success(
        &f.command(&["history", "recover"])
            .arg(operation.file_name().ok_or("missing id")?)
            .arg("--agents-stopped")
            .output()?,
    );
    Ok(())
}

#[test]
fn interrupted_import_resumes_from_verified_snapshot_and_detects_tampering() -> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    let source = f.home.join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("session.jsonl"), "recover me")?;
    let output = f
        .command(&["history", "import"])
        .arg(&source)
        .arg("--agents-stopped")
        .output()?;
    success(&output);
    let operation = fs::read_dir(f.root.join("backups"))?
        .next()
        .ok_or("missing backup")??
        .path();
    let id = operation.file_name().ok_or("missing id")?;
    let manifest_path = operation.join("manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    manifest["phase"] = json!("Merging");
    fs::write(&manifest_path, serde_json::to_vec(&manifest)?)?;
    fs::remove_file(f.root.join("shared/claude/projects/session.jsonl"))?;
    fs::remove_dir_all(source)?;
    success(
        &f.command(&["history", "recover"])
            .arg(id)
            .arg("--agents-stopped")
            .output()?,
    );
    assert_eq!(
        fs::read_to_string(f.root.join("shared/claude/projects/session.jsonl"))?,
        "recover me"
    );
    manifest["phase"] = json!("Merging");
    fs::write(&manifest_path, serde_json::to_vec(&manifest)?)?;
    fs::write(operation.join("snapshot/session.jsonl"), "tampered")?;
    failure(
        &f.command(&["history", "recover"])
            .arg(id)
            .arg("--agents-stopped")
            .output()?,
        "backup verification failed",
    );
    assert_eq!(
        fs::read_to_string(f.root.join("shared/claude/projects/session.jsonl"))?,
        "recover me"
    );
    Ok(())
}

#[test]
fn import_requires_confirmation_and_rejects_symlinks_and_path_traversal() -> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    let source = f.home.join("source");
    fs::create_dir(&source)?;
    failure(
        &f.command(&["history", "import"]).arg(&source).output()?,
        "--agents-stopped",
    );
    symlink(
        f.account("personal").join(".credentials.json"),
        source.join("credential-link"),
    )?;
    failure(
        &f.command(&["history", "import"])
            .arg(&source)
            .arg("--agents-stopped")
            .output()?,
        "contains a symlink",
    );
    failure(
        &f.command(&[
            "history",
            "recover",
            "import-../../escape",
            "--agents-stopped",
        ])
        .output()?,
        "Invalid recovery ID",
    );
    Ok(())
}

#[test]
fn account_names_cannot_collide_by_case_or_escape_storage() -> TestResult {
    let f = Fixture::new()?;
    for name in [
        "Personal",
        "PERSONAL",
        "../personal",
        "a/b",
        ".hidden",
        "",
        "1account",
    ] {
        failure(
            &f.command(&["account", "add", name]).output()?,
            "Account names must",
        );
    }
    f.add("personal")?;
    let credentials = fs::read(f.account("personal").join(".credentials.json"))?;
    failure(
        &f.command(&["account", "add", "Personal"]).output()?,
        "Account names must",
    );
    assert_eq!(
        fs::read(f.account("personal").join(".credentials.json"))?,
        credentials
    );
    Ok(())
}

#[test]
fn real_failed_merge_keeps_snapshot_and_can_be_recovered_without_source() -> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    let source = f.home.join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("session.jsonl"), "recover after merge failure")?;
    let destination = f.root.join("shared/claude/projects/session.jsonl");
    symlink(f.home.join("unrelated"), &destination)?;
    failure(
        &f.command(&["history", "import"])
            .arg(&source)
            .arg("--agents-stopped")
            .output()?,
        "Unsafe history symlink",
    );
    let operation = fs::read_dir(f.root.join("backups"))?
        .next()
        .ok_or("missing backup")??
        .path();
    fs::remove_file(destination)?;
    fs::remove_dir_all(source)?;
    success(
        &f.command(&["history", "recover"])
            .arg(operation.file_name().ok_or("missing id")?)
            .arg("--agents-stopped")
            .output()?,
    );
    assert_eq!(
        fs::read_to_string(f.root.join("shared/claude/projects/session.jsonl"))?,
        "recover after merge failure"
    );
    Ok(())
}

#[test]
fn incomplete_snapshot_recovery_rejects_changed_sources_before_merging_anything() -> TestResult {
    use sha2::{Digest, Sha256};
    let f = Fixture::new()?;
    f.add("personal")?;
    let source = f.home.join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("a.jsonl"), "first")?;
    fs::write(source.join("b.jsonl"), "changed")?;
    let operation = f.root.join("backups/import-fixture");
    fs::create_dir_all(operation.join("snapshot"))?;
    fs::write(operation.join("snapshot/a.jsonl"), "first")?;
    let manifest = json!({"version":1,"source":source,"phase":"Snapshotting","entries":[
        {"path":"a.jsonl","sha256":format!("{:x}", Sha256::digest(b"first"))},
        {"path":"b.jsonl","sha256":format!("{:x}", Sha256::digest(b"second"))}
    ]});
    fs::write(
        operation.join("manifest.json"),
        serde_json::to_vec(&manifest)?,
    )?;
    failure(
        &f.command(&["history", "recover", "import-fixture", "--agents-stopped"])
            .output()?,
        "History changed",
    );
    assert!(!f.root.join("shared/claude/projects/a.jsonl").exists());
    fs::write(source.join("b.jsonl"), "second")?;
    success(
        &f.command(&["history", "recover", "import-fixture", "--agents-stopped"])
            .output()?,
    );
    assert_eq!(
        fs::read_to_string(f.root.join("shared/claude/projects/b.jsonl"))?,
        "second"
    );
    Ok(())
}

#[test]
fn worktree_launch_checks_local_settings_in_the_main_checkout() -> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    let main = f.home.join("main checkout");
    let gitdir = main.join(".git/worktrees/feature");
    fs::create_dir_all(&gitdir)?;
    fs::create_dir_all(main.join(".claude"))?;
    fs::write(gitdir.join("commondir"), "../..\n")?;
    fs::write(
        f.project.join(".git"),
        format!("gitdir: {}\n", gitdir.display()),
    )?;
    fs::write(
        main.join(".claude/settings.local.json"),
        r#"{"apiKeyHelper":"sensitive-value"}"#,
    )?;
    let output = f.command(&["run"]).output()?;
    failure(&output, "apiKeyHelper");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sensitive-value"));
    Ok(())
}

#[test]
fn inconclusive_auth_verification_never_deletes_credentials_or_transcripts() -> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    success(&f.command(&["run"]).output()?);
    let home = f.account("personal");
    let credential = fs::read(home.join(".credentials.json"))?;
    let transcript_path = home.join("projects/fixture-project/session-fixture.jsonl");
    let transcript = fs::read(&transcript_path)?;
    failure(
        &f.command(&["run"])
            .env("FAKE_INVALID_AUTH", "yes")
            .output()?,
        "Unrecognized Claude auth status",
    );
    assert_eq!(fs::read(home.join(".credentials.json"))?, credential);
    assert_eq!(fs::read(&transcript_path)?, transcript);
    success(&f.command(&["run", "--", "--continue"]).output()?);
    Ok(())
}

#[test]
fn importing_an_account_home_is_refused_before_credentials_can_be_copied() -> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    let source = f.home.join("old-account");
    fs::create_dir_all(source.join("projects"))?;
    fs::write(source.join(".credentials.json"), "sensitive-credential")?;
    fs::write(source.join("projects/session.jsonl"), "conversation")?;
    let output = f
        .command(&["history", "import"])
        .arg(&source)
        .arg("--agents-stopped")
        .output()?;
    failure(&output, "import its projects/ directory");
    assert_eq!(fs::read_dir(f.root.join("backups"))?.count(), 0);
    assert!(
        !f.root
            .join("shared/claude/projects/.credentials.json")
            .exists()
    );
    Ok(())
}

#[test]
fn switching_and_failed_status_checks_never_invoke_login() -> TestResult {
    let f = Fixture::new()?;
    let log = f.home.join("login-invocations");
    for name in ["personal", "work"] {
        success(
            &f.command(&["account", "add", name])
                .env("FAKE_EMAIL", format!("{name}@example.test"))
                .env("FAKE_LOGIN_LOG", &log)
                .output()?,
        );
    }
    assert_eq!(fs::read_to_string(&log)?.lines().count(), 2);
    success(
        &f.command(&["run", "personal"])
            .env("FAKE_LOGIN_LOG", &log)
            .output()?,
    );
    for name in ["work", "personal", "work"] {
        success(
            &f.command(&["run", name, "--", "--continue"])
                .env("FAKE_LOGIN_LOG", &log)
                .output()?,
        );
    }
    let output = f
        .command(&["run", "personal"])
        .env("FAKE_FAIL_AUTH", "yes")
        .env("FAKE_LOGIN_LOG", &log)
        .output()?;
    failure(&output, "status-check failure does not prove");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("account login"));
    assert_eq!(fs::read_to_string(&log)?.lines().count(), 2);
    Ok(())
}

#[test]
fn expired_access_token_reaches_claude_refresh_and_resumes_without_login() -> TestResult {
    let f = Fixture::new()?;
    let log = f.home.join("login-invocations");
    for name in ["personal", "work"] {
        success(
            &f.command(&["account", "add", name])
                .env("FAKE_EMAIL", format!("{name}@example.test"))
                .env("FAKE_LOGIN_LOG", &log)
                .output()?,
        );
    }
    success(&f.command(&["run", "work"]).output()?);
    let work_credential = fs::read(f.account("work").join(".credentials.json"))?;
    for status_exit in ["0", "1"] {
        let output = f
            .command(&["run", "personal", "--", "--continue"])
            .env("FAKE_ACCESS_EXPIRED", "yes")
            .env("FAKE_STATUS_EXIT", status_exit)
            .env("FAKE_LOGIN_LOG", &log)
            .output()?;
        success(&output);
        let text = String::from_utf8(output.stdout)?;
        assert!(text.contains("automatic-refresh"));
        assert!(text.contains("account=personal@example.test"));
        assert!(text.contains("remember this"));
        assert!(text.contains("fixture-result"));
        assert_eq!(
            fs::read_to_string(f.account("personal").join(".credentials.json"))?,
            "refreshed-by-claude"
        );
    }
    success(&f.command(&["account", "default", "personal"]).output()?);
    success(
        &f.command(&["run", "--", "--resume", "session-fixture"])
            .env("FAKE_ACCESS_EXPIRED", "yes")
            .env("FAKE_LOGIN_LOG", &log)
            .output()?,
    );
    assert_eq!(fs::read_to_string(log)?.lines().count(), 2);
    assert_eq!(
        fs::read(f.account("work").join(".credentials.json"))?,
        work_credential
    );
    Ok(())
}

#[test]
fn refreshable_status_does_not_bypass_provider_identity_or_plan_checks() -> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    for method in [
        "api_key",
        "apiKey",
        "api_key_helper",
        "third_party",
        "oauth_token",
        "none",
    ] {
        failure(
            &f.command(&["run"])
                .env("FAKE_ACCESS_EXPIRED", "yes")
                .env("FAKE_AUTH_METHOD", method)
                .output()?,
            "claude.ai subscription login",
        );
    }
    failure(
        &f.command(&["run"])
            .env("FAKE_ACCESS_EXPIRED", "yes")
            .env("FAKE_SUBSCRIPTION", "free")
            .output()?,
        "recognized paid subscription",
    );
    failure(
        &f.command(&["run"])
            .env("FAKE_ACCESS_EXPIRED", "yes")
            .env("FAKE_CONFIG_DIR", &f.home)
            .output()?,
        "different config directory",
    );
    failure(
        &f.command(&["run"])
            .env("FAKE_ACCESS_EXPIRED", "yes")
            .env("ANTHROPIC_API_KEY", "sensitive-value")
            .output()?,
        "ANTHROPIC_API_KEY",
    );
    failure(
        &f.command(&["run"])
            .env("FAKE_INCOMPLETE_AUTH", "yes")
            .output()?,
        "Unrecognized Claude auth status",
    );
    fs::write(
        f.account("personal").join(".identity"),
        "other@example.test",
    )?;
    failure(
        &f.command(&["run"])
            .env("FAKE_ACCESS_EXPIRED", "yes")
            .output()?,
        "identity changed",
    );
    Ok(())
}

#[test]
fn account_registration_and_doctor_still_require_an_active_verified_login() -> TestResult {
    let f = Fixture::new()?;
    let output = f
        .command(&["account", "add", "personal"])
        .env("FAKE_ACCESS_EXPIRED", "yes")
        .env("FAKE_STATUS_EXIT", "0")
        .output()?;
    failure(&output, "active claude.ai subscription login");
    assert!(!f.root.join("config.json").exists());
    f.add("personal")?;
    failure(
        &f.command(&["doctor"])
            .env("FAKE_ACCESS_EXPIRED", "yes")
            .env("FAKE_STATUS_EXIT", "0")
            .output()?,
        "active claude.ai subscription login",
    );
    Ok(())
}

#[test]
fn rejected_refresh_is_left_to_claude_without_relogin_or_credential_deletion() -> TestResult {
    let f = Fixture::new()?;
    f.add("personal")?;
    success(&f.command(&["run"]).output()?);
    let credential_path = f.account("personal").join(".credentials.json");
    let credential = fs::read(&credential_path)?;
    let transcript = f
        .root
        .join("shared/claude/projects/fixture-project/session-fixture.jsonl");
    let saved = fs::read(&transcript)?;
    let login_log = f.home.join("unexpected-login");
    let output = f
        .command(&["run", "--", "--continue"])
        .env("FAKE_ACCESS_EXPIRED", "yes")
        .env("FAKE_REFRESH_REVOKED", "yes")
        .env("FAKE_LOGIN_LOG", &login_log)
        .output()?;
    assert_eq!(output.status.code(), Some(42));
    assert!(String::from_utf8_lossy(&output.stderr).contains("refresh credential revoked"));
    assert!(!login_log.exists());
    assert_eq!(fs::read(credential_path)?, credential);
    assert_eq!(fs::read(transcript)?, saved);
    Ok(())
}
