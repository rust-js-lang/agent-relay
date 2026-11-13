use crate::store::{Store, atomic_write_with, ensure_dir, open_regular, read_regular, write_json};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: PathBuf,
    sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    source: PathBuf,
    phase: Phase,
    entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize, PartialEq)]
enum Phase {
    Snapshotting,
    Merging,
    Complete,
}

fn inventory(root: &Path, current: &Path, entries: &mut Vec<Entry>) -> Result<()> {
    for entry in fs::read_dir(current)? {
        let path = entry?.path();
        let meta = fs::symlink_metadata(&path)?;
        ensure!(
            !meta.is_symlink(),
            "History source contains a symlink: {}",
            path.display()
        );
        if meta.is_dir() {
            inventory(root, &path, entries)?;
        } else {
            ensure!(
                meta.is_file(),
                "History source contains a special file: {}",
                path.display()
            );
            entries.push(Entry {
                path: path.strip_prefix(root)?.to_path_buf(),
                sha256: hash_file(&path)?,
            });
        }
    }
    Ok(())
}

fn validate(manifest: &Manifest) -> Result<()> {
    ensure!(
        manifest.version == 1,
        "Unsupported history manifest version"
    );
    for entry in &manifest.entries {
        ensure!(
            !entry.path.as_os_str().is_empty()
                && entry
                    .path
                    .components()
                    .all(|c| matches!(c, Component::Normal(_)))
                && entry.sha256.len() == 64
                && entry.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid history manifest entry"
        );
    }
    Ok(())
}

fn verify(actual: &str, entry: &Entry) -> Result<()> {
    ensure!(
        actual == entry.sha256,
        "History changed or backup verification failed for {}; original files were not modified",
        entry.path.display()
    );
    Ok(())
}

// Only this fixed-size buffer is retained, regardless of transcript size.
// The same pass hashes copied bytes before a staged file can be published.
fn transfer(reader: &mut impl Read, writer: &mut impl Write) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let count = match reader.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            break;
        }
        writer.write_all(&buffer[..count])?;
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn hash_file(path: &Path) -> Result<String> {
    transfer(&mut open_regular(path)?, &mut io::sink())
}

fn copy_verified(source: &Path, target: &Path, entry: &Entry) -> Result<()> {
    let mut source = open_regular(source)?;
    atomic_write_with(target, |target| {
        let actual = transfer(&mut source, target)?;
        verify(&actual, entry)
    })
}

pub fn import(store: &Store, source: &Path) -> Result<String> {
    let _gate = store.gate(false)?;
    ensure!(
        !fs::symlink_metadata(source)?.is_symlink(),
        "Source must be a real projects directory, not a symlink"
    );
    let source = source.canonicalize()?;
    ensure!(source.is_dir(), "Source must be a projects directory");
    for name in [
        ".credentials.json",
        ".claude.json",
        "settings.json",
        "settings.local.json",
        "auth.json",
        "history.jsonl",
    ] {
        ensure!(
            !source.join(name).try_exists()?,
            "Source contains account configuration ({name}); import its projects/ directory, not the account home"
        );
    }
    ensure!(
        !source.starts_with(&store.root) && !store.root.starts_with(&source),
        "Source must not overlap Agent Relay storage"
    );
    let mut entries = Vec::new();
    eprintln!("[i] Indexing history; large files may take time");
    inventory(&source, &source, &mut entries)?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    ensure!(!entries.is_empty(), "Source projects directory is empty");
    let id = format!(
        "import-{}-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        std::process::id()
    );
    let operation = store.root.join("backups").join(&id);
    ensure_dir(&operation)?;
    let manifest = Manifest {
        version: 1,
        source,
        phase: Phase::Snapshotting,
        entries,
    };
    write_json(&operation.join("manifest.json"), &manifest)?;
    eprintln!("[i] Recovery ID: {id}");
    complete(store, &operation)?;
    Ok(id)
}

pub fn recover(store: &Store, id: &str) -> Result<()> {
    ensure!(
        id.starts_with("import-") && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "Invalid recovery ID"
    );
    let _gate = store.gate(false)?;
    let operation = store.root.join("backups").join(id);
    ensure!(
        fs::symlink_metadata(&operation)?.is_dir(),
        "Invalid backup directory"
    );
    complete(store, &operation)
}

fn complete(store: &Store, operation: &Path) -> Result<()> {
    let manifest_path = operation.join("manifest.json");
    let mut manifest: Manifest = serde_json::from_slice(&read_regular(&manifest_path)?)
        .context("Invalid recovery manifest; keep the backup and restore its manifest")?;
    validate(&manifest)?;
    if manifest.phase == Phase::Complete {
        println!("[OK] History import is already complete");
        return Ok(());
    }
    if manifest.phase == Phase::Snapshotting {
        ensure_dir(&operation.join("snapshot"))?;
        for entry in &manifest.entries {
            let target = operation.join("snapshot").join(&entry.path);
            validate_tree_path(&operation.join("snapshot"), &entry.path)?;
            if target.try_exists()? {
                verify(&hash_file(&target)?, entry)?;
            } else {
                let source = manifest.source.join(&entry.path);
                // Check intermediate components too; never follow substituted links.
                validate_tree_path(&manifest.source, &entry.path)?;
                copy_verified(&source, &target, entry)?;
            }
        }
        manifest.phase = Phase::Merging;
        write_json(&manifest_path, &manifest)?;
    }
    let mut conflicts = 0;
    for entry in &manifest.entries {
        validate_tree_path(&operation.join("snapshot"), &entry.path)?;
        let snapshot = operation.join("snapshot").join(&entry.path);
        let destination = store.projects().join(&entry.path);
        validate_tree_path(&store.projects(), &entry.path)?;
        match fs::symlink_metadata(&destination) {
            Ok(meta) => {
                ensure!(
                    meta.is_file() && !meta.is_symlink(),
                    "Unsafe existing history entry: {}",
                    destination.display()
                );
                if hash_file(&destination)? != entry.sha256 {
                    let conflict = operation.join("conflicts").join(&entry.path);
                    copy_verified(&snapshot, &conflict, entry)?;
                    conflicts += 1;
                } else {
                    verify(&hash_file(&snapshot)?, entry)?;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                copy_verified(&snapshot, &destination, entry)?
            }
            Err(e) => return Err(e.into()),
        }
    }
    manifest.phase = Phase::Complete;
    write_json(&manifest_path, &manifest)?;
    println!(
        "[OK] Imported {} files; {conflicts} conflicting files retained separately",
        manifest.entries.len()
    );
    println!("[i] Verified backup: {}", operation.display());
    println!("[i] Source history remains unchanged; use Relay to continue in the imported history");
    Ok(())
}

fn validate_tree_path(root: &Path, relative: &Path) -> Result<()> {
    let mut path = root.to_path_buf();
    ensure!(
        !fs::symlink_metadata(root)?.is_symlink(),
        "Unsafe history root: {}",
        root.display()
    );
    for component in relative.components() {
        ensure!(
            matches!(component, Component::Normal(_)),
            "Invalid history path"
        );
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(meta) => ensure!(
                !meta.is_symlink(),
                "Unsafe history symlink: {}",
                path.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_handles_interrupted_and_short_reads_with_a_bounded_buffer() -> Result<()> {
        struct ShortReader<'a> {
            input: &'a [u8],
            interrupted: bool,
            largest_request: usize,
        }
        impl Read for ShortReader<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.largest_request = self.largest_request.max(buffer.len());
                if !self.interrupted {
                    self.interrupted = true;
                    Err(io::ErrorKind::Interrupted.into())
                } else {
                    let count = buffer.len().min(self.input.len()).min(3);
                    buffer[..count].copy_from_slice(&self.input[..count]);
                    self.input = &self.input[count..];
                    Ok(count)
                }
            }
        }
        let mut reader = ShortReader {
            input: b"hello world",
            interrupted: false,
            largest_request: 0,
        };
        let mut output = Vec::new();
        let hash = transfer(&mut reader, &mut output)?;
        assert_eq!(output, b"hello world");
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        assert_eq!(reader.largest_request, 1024 * 1024);
        Ok(())
    }

    #[test]
    fn hash_mismatch_does_not_replace_existing_destination_or_leave_a_partial_file() -> Result<()> {
        let temporary = tempfile::TempDir::new()?;
        let root = temporary.path().canonicalize()?;
        let source = root.join("source");
        let target = root.join("target");
        fs::write(&source, b"changed source")?;
        fs::write(&target, b"existing history")?;
        let entry = Entry {
            path: "source".into(),
            sha256: "0".repeat(64),
        };
        assert!(copy_verified(&source, &target, &entry).is_err());
        assert_eq!(fs::read(&target)?, b"existing history");
        assert_eq!(fs::read_dir(root)?.count(), 2);
        Ok(())
    }
}
