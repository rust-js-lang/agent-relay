use crate::store::{Account, read_regular};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct Claude {
    pub binary: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthStatus {
    logged_in: bool,
    auth_method: String,
    config_directory: PathBuf,
    subscription_type: Option<String>,
    email: Option<String>,
    org_id: Option<String>,
}

pub fn protected_env(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    key.starts_with("ANTHROPIC_")
        || key.starts_with("CLAUDE_CODE_OAUTH_")
        || matches!(
            key.as_str(),
            "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR"
                | "CLAUDE_SECURESTORAGE_CONFIG_DIR"
                | "CLAUDE_CODE_USE_BEDROCK"
                | "CLAUDE_CODE_USE_VERTEX"
                | "CLAUDE_CODE_USE_FOUNDRY"
                | "CLAUDE_CODE_USE_MANTLE"
                | "CLAUDE_CODE_USE_ANTHROPIC"
                | "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST"
                | "CLAUDE_CODE_SESSION_ACCESS_TOKEN"
                | "CLAUDE_CODE_WEBSOCKET_AUTH_FILE_DESCRIPTOR"
                | "CLAUDE_CODE_SKIP_PROMPT_HISTORY"
                | "CLAUDE_CODE_SUBPROCESS_ENV_SCRUB"
        )
}

pub fn check_environment() -> Result<()> {
    for (key, value) in std::env::vars_os() {
        let key = key.to_string_lossy();
        ensure!(
            !protected_env(&key) || value.is_empty(),
            "Conflicting environment variable {key}; unset it before using a subscription account (value hidden)"
        );
    }
    Ok(())
}

pub fn check_args(args: &[OsString]) -> Result<()> {
    for arg in args {
        let text = arg.to_string_lossy();
        let flag = text.split('=').next().unwrap_or_default();
        ensure!(
            !matches!(
                flag,
                "--settings"
                    | "--setting-sources"
                    | "--bare"
                    | "--no-session-persistence"
                    | "--worktree"
                    | "-w"
                    | "--cwd"
                    | "--config-dir"
                    | "--bg"
                    | "--background"
                    | "--teleport"
                    | "--remote-control"
                    | "--rc"
                    | "--resume-session-at"
            ),
            "{flag} is unsupported: it can bypass account checks, change history discovery, or detach the session"
        );
    }
    if let Some(first) = args.first().and_then(|v| v.to_str()) {
        ensure!(
            !matches!(
                first,
                "auth" | "setup-token" | "gateway" | "install" | "update"
            ),
            "Use account add/login for authentication; run only conversation commands through Relay"
        );
    }
    Ok(())
}

fn inspect_settings(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
        Ok(_) => {}
    }
    // Settings links are valid, but unreadable or invalid targets must fail closed.
    let resolved = path
        .canonicalize()
        .with_context(|| format!("Cannot inspect settings {}", path.display()))?;
    let value: Value = serde_json::from_slice(&read_regular(&resolved)?)
        .with_context(|| format!("Invalid settings in {} (contents hidden)", path.display()))?;
    let object = value
        .as_object()
        .with_context(|| format!("Expected settings object in {}", path.display()))?;
    for key in [
        "apiKeyHelper",
        "forceLoginGatewayUrl",
        "awsAuthRefresh",
        "awsCredentialExport",
    ] {
        ensure!(
            object.get(key).is_none_or(|v| v.is_null() || v == ""),
            "Conflicting setting {key} in {} (value hidden)",
            path.display()
        );
    }
    if let Some(method) = object.get("forceLoginMethod") {
        ensure!(
            method == "claudeai",
            "Conflicting forceLoginMethod in {}",
            path.display()
        );
    }
    if let Some(env) = object.get("env") {
        let env = env
            .as_object()
            .with_context(|| format!("Invalid env block in {}", path.display()))?;
        for (key, value) in env {
            ensure!(
                value.is_string(),
                "Invalid environment setting {key} in {}",
                path.display()
            );
            ensure!(
                !(protected_env(key) || key.eq_ignore_ascii_case("CLAUDE_CONFIG_DIR"))
                    || value == "",
                "Conflicting setting env.{key} in {} (value hidden)",
                path.display()
            );
        }
    }
    Ok(())
}

pub fn check_settings(home: &Path, project: &Path) -> Result<()> {
    for name in [
        "settings.json",
        "settings.local.json",
        "managed-settings.json",
    ] {
        inspect_settings(&home.join(name))?;
    }
    for ancestor in project.ancestors() {
        for name in ["settings.json", "settings.local.json"] {
            inspect_settings(&ancestor.join(".claude").join(name))?;
        }
        let git = ancestor.join(".git");
        if git.is_file() {
            // Worktrees can load local settings from the main checkout, which is
            // outside the current directory's ancestors. Submodules have no commondir.
            let text = String::from_utf8(read_regular(&git.canonicalize()?)?)
                .context("Cannot inspect worktree .git pointer")?;
            let pointer = text
                .strip_prefix("gitdir: ")
                .context("Unrecognized worktree .git pointer")?
                .trim_end();
            let gitdir = ancestor
                .join(pointer)
                .canonicalize()
                .context("Cannot resolve worktree gitdir")?;
            let common_path = gitdir.join("commondir");
            if common_path.try_exists()? {
                let common = String::from_utf8(read_regular(&common_path)?)
                    .context("Cannot inspect worktree commondir")?;
                let common = gitdir.join(common.trim_end()).canonicalize()?;
                let main = common.parent().context("Cannot resolve main checkout")?;
                inspect_settings(&main.join(".claude/settings.local.json"))?;
            }
            break;
        } else if git.is_dir() {
            break;
        }
    }
    #[cfg(target_os = "macos")]
    let managed_root = Path::new("/Library/Application Support/ClaudeCode");
    #[cfg(not(target_os = "macos"))]
    let managed_root = Path::new("/etc/claude-code");
    inspect_settings(&managed_root.join("managed-settings.json"))?;
    let dropins = managed_root.join("managed-settings.d");
    match fs::read_dir(&dropins) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if entry.path().extension() == Some(OsStr::new("json")) {
                    inspect_settings(&entry.path())?;
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("Cannot inspect {}", dropins.display())),
    }
    Ok(())
}

impl Claude {
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self> {
        let binary = match explicit {
            Some(path) => path
                .canonicalize()
                .context("Cannot resolve --claude executable")?,
            None => {
                let path = std::env::var_os("PATH").context("PATH is unset; provide --claude")?;
                std::env::split_paths(&path)
                    .map(|dir| dir.join("claude"))
                    .find(|path| path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
                    .context("Claude CLI not found on PATH; install it or provide --claude /absolute/path")?
                    .canonicalize()?
            }
        };
        ensure!(
            binary.metadata()?.is_file() && binary.metadata()?.permissions().mode() & 0o111 != 0,
            "Claude executable is not an executable file"
        );
        ensure!(
            std::env::current_exe()?.canonicalize()? != binary,
            "Claude executable points to Agent Relay itself"
        );
        Ok(Self { binary })
    }

    pub fn command(&self, home: &Path, project: &Path) -> Command {
        let mut command = Command::new(&self.binary);
        command.current_dir(project).env("CLAUDE_CONFIG_DIR", home);
        // Avoid inheriting another agent's session identity or nested-session guard.
        for key in [
            "CLAUDECODE",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_PARENT_SESSION_ID",
        ] {
            command.env_remove(key);
        }
        command
    }

    pub fn check_version(&self, home: &Path, project: &Path) -> Result<String> {
        let output = probe(self.command(home, project).arg("--version"))?;
        let version = output
            .split_whitespace()
            .next()
            .context("Missing Claude version")?;
        let parts: Vec<_> = version
            .split('.')
            .map(str::parse::<u32>)
            .collect::<std::result::Result<_, _>>()
            .context("Unrecognized Claude version; expected 2.1.x")?;
        ensure!(
            parts.len() == 3 && parts[0] == 2 && parts[1] == 1 && parts[2] >= 268,
            "Unsupported Claude version {version}; Relay currently requires >=2.1.268 and <2.2.0 for configDirectory auth verification"
        );
        Ok(version.to_owned())
    }

    pub fn auth(&self, home: &Path, project: &Path) -> Result<Account> {
        let output = probe(
            self.command(home, project)
                .args(["auth", "status", "--json"]),
        )
        .context("Cannot verify login; use agent-relay account login <name>")?;
        let auth: AuthStatus = serde_json::from_str(&output).map_err(|_| {
            anyhow::anyhow!(
                "Unrecognized Claude auth status JSON; no subscription launch was authorized"
            )
        })?;
        ensure!(
            auth.logged_in && auth.auth_method == "claude.ai",
            "Claude did not report an active claude.ai subscription login; refusing to launch"
        );
        ensure!(
            auth.config_directory.canonicalize()? == home.canonicalize()?,
            "Claude reported a different config directory; refusing to launch the wrong account"
        );
        let subscription = auth
            .subscription_type
            .context("Claude did not report subscriptionType")?;
        ensure!(
            matches!(
                subscription.to_ascii_lowercase().as_str(),
                "pro" | "max" | "ultra" | "team" | "enterprise"
            ),
            "Claude did not report a recognized paid subscription; refusing to launch"
        );
        let email = auth
            .email
            .filter(|s| !s.is_empty())
            .context("Claude did not report an account email")?;
        Ok(Account {
            email,
            org_id: auth.org_id,
            subscription,
        })
    }
}

fn probe(command: &mut Command) -> Result<String> {
    // Write output to anonymous temporary files: bounded pipe reads can deadlock if
    // an unexpected wrapper fills stdout/stderr before it exits. Never echo stderr,
    // which may include credentials. Temporary files are unlinked before spawning.
    let mut output = temporary_file()?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Claude local status check timed out after 10 seconds");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    ensure!(
        status.success(),
        "Claude local status check failed (exit {:?}; output hidden)",
        status.code()
    );
    use std::io::{Seek, SeekFrom};
    output.seek(SeekFrom::Start(0))?;
    let mut data = String::new();
    output.take(1024 * 1024 + 1).read_to_string(&mut data)?;
    ensure!(
        data.len() <= 1024 * 1024,
        "Claude status response exceeded 1 MiB"
    );
    Ok(data)
}

fn temporary_file() -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "agent-relay-probe-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    fs::remove_file(path)?;
    Ok(file)
}
