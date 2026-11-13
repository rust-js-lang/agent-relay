#![cfg(unix)]

mod claude;
mod history;
mod store;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use claude::{Claude, check_args, check_environment, check_settings};
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use store::{Account, Store, valid_name};

#[derive(Parser)]
#[command(
    version,
    about = "Switch Claude subscriptions and resume shared conversations"
)]
struct Cli {
    /// Application data directory (or AGENT_RELAY_HOME)
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    /// Official Claude CLI executable; defaults to claude on PATH
    #[arg(long, global = true)]
    claude: Option<PathBuf>,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Add, log in to, list, or select a subscription account
    Account {
        #[command(subcommand)]
        command: AccountAction,
    },
    /// Start the selected account; pass Claude arguments after --
    Run {
        /// Account name, or omit to use the default
        account: Option<String>,
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    /// Inspect account identity, subscription authentication, and history links
    Doctor { account: Option<String> },
    /// Explicitly import existing project transcripts without modifying the source
    History {
        #[command(subcommand)]
        command: HistoryAction,
    },
}

#[derive(Subcommand)]
enum AccountAction {
    /// Log in to a new subscription in its own stable configuration directory
    Add { name: String },
    /// Renew an existing account's login; refuses while Relay sessions use it
    Login { name: String },
    /// List registered accounts (does not contact Claude or read credentials)
    List,
    /// Choose the account used when run has no explicit account
    Default { name: String },
}

#[derive(Subcommand)]
enum HistoryAction {
    /// Import a projects/ directory; retain verified snapshots and conflicting files
    Import {
        source: PathBuf,
        /// Confirm all direct Claude processes writing the source/destination are stopped
        #[arg(long, required = true)]
        agents_stopped: bool,
    },
    /// Resume an interrupted import by its printed recovery ID
    Recover {
        id: String,
        #[arg(long, required = true)]
        agents_stopped: bool,
    },
}

fn main() {
    if let Err(error) = execute(Cli::parse()) {
        eprintln!("[X] {error:#}");
        std::process::exit(1);
    }
}

fn execute(cli: Cli) -> Result<()> {
    let store = Store::open(cli.root.map(Ok).unwrap_or_else(default_root)?)?;
    match cli.command {
        Action::Account {
            command: AccountAction::List,
        } => {
            let registry = store.registry()?;
            for (name, account) in registry.accounts {
                let marker = if registry.default.as_deref() == Some(&name) {
                    "*"
                } else {
                    " "
                };
                println!(
                    "{marker} {name}\t{}\t{}",
                    account.email, account.subscription
                );
            }
            Ok(())
        }
        Action::Account {
            command: AccountAction::Default { name },
        } => {
            let _lock = store.registry_lock()?;
            let mut registry = store.registry()?;
            ensure!(
                registry.accounts.contains_key(&name),
                "Unknown account {name}; use account add first"
            );
            registry.default = Some(name.clone());
            store.save(&registry)?;
            println!("[OK] Default account: {name}. Running sessions are unchanged.");
            Ok(())
        }
        Action::Account { command } => {
            let (name, adding) = match command {
                AccountAction::Add { name } => (name, true),
                AccountAction::Login { name } => (name, false),
                _ => unreachable!(),
            };
            login(&store, &Claude::resolve(cli.claude)?, &name, adding)
        }
        Action::Run { account, args } => {
            check_args(&args)?;
            let name = select_account(&store, account)?;
            let _gate = store.gate(true)?;
            let _account = store.account_lock(&name, true)?;
            let project = std::env::current_dir()?.canonicalize()?;
            let _session = store.session_lock()?;
            let home = store.prepare_account(&name)?;
            let claude = Claude::resolve(cli.claude)?;
            preflight(&claude, &home, &project)?;
            let identity = claude.auth(&home, &project)?;
            verify_identity(&store, &name, &identity)?;
            eprintln!("[i] Account: {name} ({})", identity.email);
            eprintln!("[i] Shared transcripts: {}", store.projects().display());
            for lock in [&_gate, &_account, &_session] {
                lock.inherit()?;
            }
            let error = claude.command(&home, &project).args(args).exec();
            Err(error).context("Cannot execute Claude")
        }
        Action::Doctor { account } => {
            let name = select_account(&store, account)?;
            let _gate = store.gate(true)?;
            let _account = store.account_lock(&name, true)?;
            let home = store.prepare_account(&name)?;
            let project = std::env::current_dir()?.canonicalize()?;
            let claude = Claude::resolve(cli.claude)?;
            let version = preflight(&claude, &home, &project)?;
            let identity = claude.auth(&home, &project)?;
            verify_identity(&store, &name, &identity)?;
            println!(
                "[OK] Claude {version}: {} ({})",
                identity.email, identity.subscription
            );
            println!(
                "[i] Account: {name}\n[i] Config: {}\n[i] Shared transcripts: {}",
                home.display(),
                store.projects().display()
            );
            println!(
                "[i] Transcripts and prompt history are shared; checkpoints remain account-local"
            );
            Ok(())
        }
        Action::History { command } => {
            eprintln!(
                "[i] Relay locks cannot detect direct Claude processes. Import only after stopping them."
            );
            match command {
                HistoryAction::Import { source, .. } => {
                    history::import(&store, &source)?;
                }
                HistoryAction::Recover { id, .. } => history::recover(&store, &id)?,
            }
            Ok(())
        }
    }
}

fn default_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("AGENT_RELAY_HOME") {
        return Ok(root.into());
    }
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is unset; provide --root")?);
    #[cfg(target_os = "macos")]
    let base = home.join("Library/Application Support");
    #[cfg(not(target_os = "macos"))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    ensure!(
        base.is_absolute(),
        "Application data directory must be absolute; provide --root"
    );
    Ok(base.join("agent-relay"))
}

fn select_account(store: &Store, requested: Option<String>) -> Result<String> {
    let registry = store.registry()?;
    let name = requested
        .or(registry.default)
        .context("No default account; use account default <name> or run <name>")?;
    valid_name(&name)?;
    ensure!(
        registry.accounts.contains_key(&name),
        "Unknown account {name}; use account add first"
    );
    Ok(name)
}

fn preflight(claude: &Claude, home: &Path, project: &Path) -> Result<String> {
    check_environment()?;
    check_settings(home, project)?;
    claude.check_version(home, project)
}

fn verify_identity(store: &Store, name: &str, identity: &Account) -> Result<()> {
    let registry = store.registry()?;
    let stored = registry
        .accounts
        .get(name)
        .context("Account was removed during launch")?;
    ensure!(
        stored.email.eq_ignore_ascii_case(&identity.email) && stored.org_id == identity.org_id,
        "Account identity changed outside Relay; verify the intended browser account with account login {name}"
    );
    Ok(())
}

fn login(store: &Store, claude: &Claude, name: &str, adding: bool) -> Result<()> {
    valid_name(name)?;
    let _gate = store.gate(true)?;
    let _account = store.account_lock(name, false)?;
    let exists = store.registry()?.accounts.contains_key(name);
    ensure!(
        exists != adding,
        if adding {
            "Account exists; use account login to renew it"
        } else {
            "Unknown account; use account add first"
        }
    );
    let project = std::env::current_dir()?.canonicalize()?;
    let home = store.prepare_account(name)?;
    preflight(claude, &home, &project)?;
    eprintln!(
        "[i] Sign in to the intended Claude subscription for '{name}'. Check the browser account carefully."
    );
    // Inherit locks during login too: interrupting Relay must not unlock an account
    // while a surviving login subprocess is still changing its credential store.
    _gate.inherit()?;
    _account.inherit()?;
    let status = claude
        .command(&home, &project)
        .args(["auth", "login"])
        .status()?;
    ensure!(
        status.success(),
        "Claude login failed or was cancelled; account directory retained for retry"
    );
    check_environment()?;
    check_settings(&home, &project)?;
    let identity = claude.auth(&home, &project)?;
    let _registry = store.registry_lock()?;
    let mut registry = store.registry()?;
    for (other_name, other) in &registry.accounts {
        ensure!(
            other_name == name
                || !other.email.eq_ignore_ascii_case(&identity.email)
                || other.org_id != identity.org_id,
            "This subscription is already registered as '{other_name}'; no tokens were copied or deleted"
        );
    }
    if !adding {
        let old = registry
            .accounts
            .get(name)
            .context("Account disappeared during login")?;
        ensure!(
            old.email.eq_ignore_ascii_case(&identity.email) && old.org_id == identity.org_id,
            "Login selected a different identity; registry unchanged. Log in again with the original account, or add a new name"
        );
    }
    registry.accounts.insert(name.to_owned(), identity);
    if registry.default.is_none() {
        registry.default = Some(name.to_owned());
    }
    store.save(&registry)?;
    println!(
        "[OK] Subscription account '{name}' is ready. Resume with: agent-relay run {name} -- --resume"
    );
    Ok(())
}
