use crate::store::{Store, atomic_write, digest, ensure_dir, read_regular, write_json};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::fs;
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
                sha256: digest(&read_regular(&path)?),
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

fn verify(bytes: &[u8], entry: &Entry) -> Result<()> {
    ensure!(
        digest(bytes) == entry.sha256,
        "History changed or backup verification failed for {}; original files were not modified",
        entry.path.display()
    );
    Ok(())
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
                verify(&read_regular(&target)?, entry)?;
            } else {
                let source = manifest.source.join(&entry.path);
                // Check intermediate components too; never follow substituted links.
                validate_tree_path(&manifest.source, &entry.path)?;
                let bytes = read_regular(&source)?;
                verify(&bytes, entry)?;
                atomic_write(&target, &bytes)?;
            }
        }
        manifest.phase = Phase::Merging;
        write_json(&manifest_path, &manifest)?;
    }
    let mut conflicts = 0;
    for entry in &manifest.entries {
        validate_tree_path(&operation.join("snapshot"), &entry.path)?;
        let bytes = read_regular(&operation.join("snapshot").join(&entry.path))?;
        verify(&bytes, entry)?;
        let destination = store.projects().join(&entry.path);
        validate_tree_path(&store.projects(), &entry.path)?;
        match fs::symlink_metadata(&destination) {
            Ok(meta) => {
                ensure!(
                    meta.is_file() && !meta.is_symlink(),
                    "Unsafe existing history entry: {}",
                    destination.display()
                );
                if digest(&read_regular(&destination)?) != entry.sha256 {
                    let conflict = operation.join("conflicts").join(&entry.path);
                    atomic_write(&conflict, &bytes)?;
                    conflicts += 1;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                atomic_write(&destination, &bytes)?
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
