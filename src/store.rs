use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub version: u32,
    pub default: Option<String>,
    pub accounts: std::collections::BTreeMap<String, Account>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub email: String,
    pub org_id: Option<String>,
    pub subscription: String,
}

pub struct Store {
    pub root: PathBuf,
}

pub fn valid_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name.as_bytes()[0].is_ascii_lowercase()
            && name
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_'),
        "Account names must start with a lowercase letter and contain at most 64 lowercase ASCII letters, digits, '-' or '_'"
    );
    Ok(())
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        ensure_dir(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(meta) => ensure!(
            meta.is_dir() && !meta.is_symlink(),
            "Unsafe directory: {}",
            path.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let result = fs::DirBuilder::new().mode(0o700).create(path);
            if let Err(error) = result {
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error.into());
                }
                let meta = fs::symlink_metadata(path)?;
                ensure!(
                    meta.is_dir() && !meta.is_symlink(),
                    "Unsafe directory: {}",
                    path.display()
                );
            }
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

pub fn read_regular(path: &Path) -> Result<Vec<u8>> {
    let file = open_regular(path)?;
    let mut data = Vec::new();
    file.take(64 * 1024 * 1024 + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() <= 64 * 1024 * 1024,
        "File exceeds 64 MiB: {}",
        path.display()
    );
    Ok(data)
}

pub fn open_regular(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options
        .open(path)
        .with_context(|| format!("Cannot read {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "Expected a regular file: {}",
        path.display()
    );
    Ok(file)
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_write_with(path, |file| {
        file.write_all(bytes)?;
        Ok(())
    })
}

pub fn atomic_write_with(path: &Path, write: impl FnOnce(&mut File) -> Result<()>) -> Result<()> {
    let parent = path.parent().context("File has no parent")?;
    ensure_dir(parent)?;
    if let Ok(meta) = fs::symlink_metadata(path) {
        ensure!(
            meta.is_file() && !meta.is_symlink(),
            "Unsafe file: {}",
            path.display()
        );
    }
    let temp = parent.join(format!(
        ".relay-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| {
        write(&mut file)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    result
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut data = serde_json::to_vec_pretty(value)?;
    data.push(b'\n');
    atomic_write(path, &data)
}

pub struct Lock(File);

impl Lock {
    pub fn acquire(path: &Path, shared: bool, message: &str) -> Result<Self> {
        ensure_dir(path.parent().context("Lock has no parent")?)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        ensure!(
            file.metadata()?.is_file(),
            "Unsafe lock file: {}",
            path.display()
        );
        let result = if shared {
            file.try_lock_shared()
        } else {
            file.try_lock()
        };
        result.with_context(|| message.to_owned())?;
        Ok(Self(file))
    }

    pub fn inherit(&self) -> Result<()> {
        // exec replaces Relay, retaining the same PID, terminal, signals, and exit status.
        // Keep locks alive in that process and any descendants that inherit this descriptor.
        let fd = self.0.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        ensure!(flags >= 0, "Cannot inspect lock descriptor");
        let result = unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
        ensure!(result >= 0, "Cannot preserve lock across exec");
        Ok(())
    }
}

impl Store {
    pub fn open(root: PathBuf) -> Result<Self> {
        // Canonicalize the existing parent (e.g. /tmp on macOS) before creating owned paths.
        let root = absolute_root(&root)?;
        ensure_dir(&root)?;
        ensure!(
            fs::metadata(&root)?.permissions().mode() & 0o077 == 0,
            "{} must be private; run chmod 700 on this directory",
            root.display()
        );
        let store = Self {
            root: root.canonicalize()?,
        };
        for name in [
            "accounts",
            "shared",
            "shared/claude",
            "shared/claude/projects",
            "locks",
            "backups",
        ] {
            ensure_dir(&store.root.join(name))?;
        }
        let history = store.root.join("shared/claude/history.jsonl");
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&history)
        {
            Ok(file) => file.sync_all()?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let meta = fs::symlink_metadata(&history)?;
                ensure!(
                    meta.is_file() && !meta.is_symlink(),
                    "Unsafe shared prompt history: {}",
                    history.display()
                );
            }
            Err(e) => return Err(e.into()),
        }
        Ok(store)
    }

    pub fn account_home(&self, name: &str) -> Result<PathBuf> {
        valid_name(name)?;
        Ok(self.root.join("accounts").join(name).join("claude"))
    }

    pub fn prepare_account(&self, name: &str) -> Result<PathBuf> {
        let home = self.account_home(name)?;
        ensure_dir(&home)?;
        for (name, target) in [
            ("projects", self.projects()),
            (
                "history.jsonl",
                self.root.join("shared/claude/history.jsonl"),
            ),
        ] {
            let link = home.join(name);
            match fs::symlink_metadata(&link) {
                Ok(meta) => {
                    ensure!(
                        meta.is_symlink() && fs::read_link(&link)? == target,
                        "{} must link to {}; existing data was left untouched",
                        link.display(),
                        target.display()
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => symlink(&target, &link)?,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(home)
    }

    pub fn projects(&self) -> PathBuf {
        self.root.join("shared/claude/projects")
    }

    pub fn gate(&self, shared: bool) -> Result<Lock> {
        Lock::acquire(
            &self.root.join("locks/history.lock"),
            shared,
            "History is in use by another Relay launch or import; stop it and retry",
        )
    }

    pub fn session_lock(&self) -> Result<Lock> {
        Lock::acquire(
            &self.root.join("locks/session.lock"),
            false,
            "Another Relay session is using this history store; exit it before resuming or switching accounts",
        )
    }

    pub fn account_lock(&self, name: &str, shared: bool) -> Result<Lock> {
        valid_name(name)?;
        Lock::acquire(
            &self.root.join("locks").join(format!("account-{name}.lock")),
            shared,
            "Another login or account update is in progress; retry after it finishes",
        )
    }

    pub fn registry_lock(&self) -> Result<Lock> {
        Lock::acquire(
            &self.root.join("locks/registry.lock"),
            false,
            "Another account update is in progress; retry",
        )
    }

    pub fn registry(&self) -> Result<Registry> {
        let path = self.root.join("config.json");
        if !path.try_exists()? {
            // try_exists follows symlinks; a dangling one is still an invalid registry.
            if fs::symlink_metadata(&path).is_ok() {
                bail!("Unsafe registry: {}", path.display());
            }
            return Ok(Registry {
                version: 1,
                ..Registry::default()
            });
        }
        let registry: Registry = serde_json::from_slice(&read_regular(&path)?)
            .context("Invalid config.json; restore a known-good copy rather than resetting it")?;
        ensure!(
            registry.version == 1,
            "Unsupported registry version {}",
            registry.version
        );
        for name in registry.accounts.keys() {
            valid_name(name)?;
        }
        if let Some(name) = &registry.default {
            ensure!(
                registry.accounts.contains_key(name),
                "Default account is missing from registry"
            );
        }
        Ok(registry)
    }

    pub fn save(&self, registry: &Registry) -> Result<()> {
        write_json(&self.root.join("config.json"), registry)
    }
}

fn absolute_root(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    ensure!(
        !path.components().any(|c| matches!(c, Component::ParentDir)),
        "Root cannot contain '..'"
    );
    if fs::symlink_metadata(&path).is_ok() {
        ensure!(
            !fs::symlink_metadata(&path)?.is_symlink(),
            "Application root cannot be a symlink"
        );
        return Ok(path.canonicalize()?);
    }
    let parent = path.parent().context("Invalid application root")?;
    let parent = if parent.exists() {
        parent.canonicalize()?
    } else {
        absolute_root(parent)?
    };
    Ok(parent.join(path.file_name().context("Invalid application root")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_staged_write_leaves_original_and_cleans_up_partial_output() -> Result<()> {
        let temporary = tempfile::TempDir::new()?;
        let root = temporary.path().canonicalize()?;
        let target = root.join("history.jsonl");
        fs::write(&target, b"original history")?;
        let result = atomic_write_with(&target, |output| {
            output.write_all(b"partial copy")?;
            Err(std::io::Error::other("simulated write failure").into())
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&target)?, b"original history");
        assert_eq!(fs::read_dir(root)?.count(), 1);
        Ok(())
    }
}
