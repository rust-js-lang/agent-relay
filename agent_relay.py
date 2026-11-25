"""Agent Relay: switch Claude subscription accounts and resume the same conversation.

A Python port of agent-relay 0.1.0, first written in Rust (see the history of
github.com/rust-js-lang/agent-relay): the same commands, files, messages and
exit codes. macOS only; Python 3.9 standard library only.

    agent-relay account add <name>       log in to a new subscription
    agent-relay run [<name>] -- <args>   start Claude on that account

How it works, in one picture:

    accounts/<name>/claude/            one CLAUDE_CONFIG_DIR per account
        .credentials.json, settings    stay per account
        projects      -> shared/claude/projects        transcripts: shared
        history.jsonl -> shared/claude/history.jsonl   prompt history: shared

`run` checks the account, takes its locks, then *becomes* Claude (exec): same
PID, same terminal, and the locks stay held until Claude and every process
that inherited them exit.

The installed copy starts with `#!<the Python.app binary> -I`, which the installer
writes, not /usr/bin/python3: that stub would add SDKROOT, CPATH, ... to the
environment Claude gets. Python itself still sets LC_CTYPE and
__CF_USER_TEXT_ENCODING in its own environment, so every environment read and
every child process here uses `environment()`, the environment the process was
started with, never os.environ.
"""

import argparse
import contextlib
import ctypes
import errno
import fcntl
import functools
import hashlib
import itertools
import json
import os
import re
import signal
import stat
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, List, Optional, Tuple

VERSION = "0.1.0"
MIB = 1024 * 1024
TEMP_SEQUENCE = itertools.count()


# --- errors ---------------------------------------------------------------


class Fail(Exception):
    """An error reported as `[X] <message>` with exit code 1."""


def ensure(condition, message):
    if not condition:
        raise Fail(message)


def describe(error):
    """Rust's wording for an error: OS errors without the path, as 'text (os error N)'."""
    if isinstance(error, OSError) and error.errno:
        return f"{os.strerror(error.errno)} (os error {error.errno})"
    return str(error) or type(error).__name__


@contextlib.contextmanager
def context(message):
    """anyhow's .context(): prefix any error raised inside with `message: `."""
    try:
        yield
    except Exception as error:
        raise Fail(f"{message}: {describe(error)}") from None


# --- the environment the process was started with -------------------------


@functools.lru_cache(maxsize=None)
def environment() -> List[Tuple[bytes, bytes]]:
    """This process's environment as the kernel received it at exec (KERN_PROCARGS2).

    Layout: argc, the executable path, NUL padding, argc arguments, then the
    environment strings up to the first empty string."""
    libc = ctypes.CDLL(None, use_errno=True)
    libc.sysctl.argtypes = [
        ctypes.POINTER(ctypes.c_int), ctypes.c_uint, ctypes.c_void_p,
        ctypes.POINTER(ctypes.c_size_t), ctypes.c_void_p, ctypes.c_size_t,
    ]
    mib = (ctypes.c_int * 3)(1, 49, os.getpid())  # CTL_KERN, KERN_PROCARGS2
    size = ctypes.c_size_t(0)
    if libc.sysctl(mib, 3, None, ctypes.byref(size), None, 0) != 0:
        raise OSError(ctypes.get_errno(), "sysctl")
    buffer = ctypes.create_string_buffer(size.value)
    if libc.sysctl(mib, 3, buffer, ctypes.byref(size), None, 0) != 0:
        raise OSError(ctypes.get_errno(), "sysctl")
    data = buffer.raw[: size.value]
    argc = int.from_bytes(data[:4], sys.byteorder)
    _, _, rest = data[4:].partition(b"\0")
    strings = rest.lstrip(b"\0").split(b"\0")[argc:]
    pairs = []
    for entry in itertools.takewhile(bool, strings):
        # As Rust's env::vars_os: the name ends at the first '=' after its first byte.
        split = entry.find(b"=", 1)
        if split > 0:
            pairs.append((entry[:split], entry[split + 1 :]))
    return pairs


def getenv(name: str) -> Optional[bytes]:
    key = os.fsencode(name)
    return next((value for k, value in environment() if k == key), None)


# --- small helpers ----------------------------------------------------------

ASCII_UPPER = str.maketrans("abcdefghijklmnopqrstuvwxyz", "ABCDEFGHIJKLMNOPQRSTUVWXYZ")
ASCII_LOWER = str.maketrans("ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghijklmnopqrstuvwxyz")


def eq_ignore_ascii_case(a: str, b: str) -> bool:
    return a.translate(ASCII_LOWER) == b.translate(ASCII_LOWER)


def components(path: str) -> List[Tuple[str, str]]:
    """Rust's Path::components(): ('root'|'cur'|'parent'|'normal', text).

    Empty segments and '.' vanish, except a '.' that starts a relative path."""
    parts = [("root", "/")] if path.startswith("/") else []
    for index, segment in enumerate(path.split("/")):
        if segment == "" or (segment == "." and index > 0):
            continue
        kind = {".": "cur", "..": "parent"}.get(segment, "normal")
        parts.append((kind, segment))
    return parts


def canonicalize(path) -> Path:
    """Rust's fs::canonicalize (realpath): the path must exist."""
    if os.fspath(path) == "":
        raise FileNotFoundError(errno.ENOENT, os.strerror(errno.ENOENT))
    try:
        return Path(path).resolve(strict=True)
    except RuntimeError:  # pathlib's report of a symlink loop
        raise OSError(errno.ELOOP, os.strerror(errno.ELOOP)) from None


def try_exists(path) -> bool:
    """Rust's Path::try_exists: false only for 'not found'; other errors propagate."""
    try:
        os.stat(path)
        return True
    except FileNotFoundError:
        return False


def is_symlink(path) -> bool:
    """Rust's fs::symlink_metadata(path)?.is_symlink(): errors propagate."""
    return stat.S_ISLNK(os.lstat(path).st_mode)


def sync_all(fd):
    """Rust's File::sync_all, which on macOS is F_FULLFSYNC."""
    fcntl.fcntl(fd, fcntl.F_FULLFSYNC)


def say(message):
    print(message, file=sys.stderr)


# --- JSON, with serde's strictness --------------------------------------------


class Pairs(list):
    """A JSON object as its (key, value) pairs, so repeated struct fields are caught."""


def reject_constant(name):
    raise ValueError(f"invalid number {name}")


def finite(text):
    number = float(text)
    ensure(number not in (float("inf"), float("-inf")), "number out of range")
    return number


def parse_json(data: bytes):
    return json.loads(
        data.decode("utf-8"),
        object_pairs_hook=Pairs,
        parse_constant=reject_constant,
        parse_float=finite,
    )


def as_map(value) -> dict:
    """A JSON object read as a map: a repeated key keeps its last value."""
    ensure(isinstance(value, Pairs), "invalid type: expected a map")
    return dict(value)


def as_struct(value, name, fields, optional=(), deny_unknown=True) -> dict:
    """A JSON object read as a serde struct; Option fields may be missing."""
    ensure(isinstance(value, Pairs), f"invalid type: expected struct {name}")
    found = {}
    for key, item in value:
        if key not in fields:
            ensure(not deny_unknown, f"unknown field `{key}`")
            continue
        ensure(key not in found, f"duplicate field `{key}`")
        found[key] = item
    for field in fields:
        ensure(field in found or field in optional, f"missing field `{field}`")
    return {field: found.get(field) for field in fields}


def as_u32(value) -> int:
    ensure(type(value) is int and 0 <= value <= 0xFFFFFFFF, "invalid type: expected u32")
    return value


def as_bool(value) -> bool:
    ensure(type(value) is bool, "invalid type: expected a boolean")
    return value


def as_str(value) -> str:
    ensure(isinstance(value, str), "invalid type: expected a string")
    return value


def as_opt_str(value) -> Optional[str]:
    return None if value is None else as_str(value)


def json_bytes(value) -> bytes:
    """serde_json::to_vec_pretty plus a newline."""
    return (json.dumps(value, indent=2, ensure_ascii=False) + "\n").encode("utf-8")


# --- store: files, locks, registry ---------------------------------------------


@dataclass
class Account:
    email: str
    org_id: Optional[str]
    subscription: str


@dataclass
class Registry:
    version: int = 1
    default: Optional[str] = None
    accounts: dict = field(default_factory=dict)


def valid_name(name: str):
    ensure(
        re.fullmatch(r"[a-z][a-z0-9_-]{0,63}", name) is not None,
        "Account names must start with a lowercase letter and contain at most 64 lowercase ASCII letters, digits, '-' or '_'",
    )


def ensure_dir(path: Path):
    path = Path(path)
    if path.parent != path and str(path.parent) != ".":
        ensure_dir(path.parent)
    try:
        ensure(stat.S_ISDIR(os.lstat(path).st_mode), f"Unsafe directory: {path}")
    except FileNotFoundError:
        try:
            os.mkdir(path, 0o700)
        except FileExistsError:
            ensure(stat.S_ISDIR(os.lstat(path).st_mode), f"Unsafe directory: {path}")


def open_regular(path):
    with context(f"Cannot read {path}"):
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    file = os.fdopen(fd, "rb", buffering=0)
    if not stat.S_ISREG(os.fstat(fd).st_mode):
        file.close()
        raise Fail(f"Expected a regular file: {path}")
    return file


def read_regular(path) -> bytes:
    with open_regular(path) as file:
        data = file.read(64 * MIB + 1)
    ensure(len(data) <= 64 * MIB, f"File exceeds 64 MiB: {path}")
    return data


def atomic_write(path: Path, data: bytes):
    atomic_write_with(path, lambda file: file.write(data))


def atomic_write_with(path: Path, write: Callable):
    """Write a temporary file beside `path`, sync it, then rename it over `path`."""
    parent = path.parent
    ensure_dir(parent)
    try:
        mode = os.lstat(path).st_mode
    except OSError:
        mode = None
    ensure(mode is None or stat.S_ISREG(mode), f"Unsafe file: {path}")
    temp = parent / f".relay-{os.getpid()}-{next(TEMP_SEQUENCE)}"
    fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(fd, "wb") as file:
            write(file)
            file.flush()
            sync_all(file.fileno())
        os.rename(temp, path)
        directory = os.open(parent, os.O_RDONLY)
        try:
            sync_all(directory)
        finally:
            os.close(directory)
    finally:
        with contextlib.suppress(OSError):
            os.unlink(temp)


def write_json(path: Path, value):
    atomic_write(path, json_bytes(value))


class Lock:
    """An flock, held until this process exits; inherit() passes it on through exec."""

    def __init__(self, path: Path, shared: bool, message: str):
        ensure_dir(path.parent)
        flags = os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK
        self.fd = os.open(path, flags, 0o600)
        try:
            ensure(stat.S_ISREG(os.fstat(self.fd).st_mode), f"Unsafe lock file: {path}")
            try:
                fcntl.flock(self.fd, (fcntl.LOCK_SH if shared else fcntl.LOCK_EX) | fcntl.LOCK_NB)
            except BlockingIOError:
                raise Fail(f"{message}: lock acquisition failed because the operation would block") from None
            except OSError as error:
                raise Fail(f"{message}: {describe(error)}") from None
        except BaseException:
            os.close(self.fd)
            raise

    def inherit(self):
        # exec replaces Relay, retaining the same PID, terminal, signals, and exit status.
        # Keep locks alive in that process and any descendants that inherit this descriptor.
        os.set_inheritable(self.fd, True)


class Store:
    def __init__(self, root: Path):
        # Canonicalize the existing parent (e.g. /tmp on macOS) before creating owned paths.
        root = absolute_root(root)
        ensure_dir(root)
        ensure(
            os.stat(root).st_mode & 0o077 == 0,
            f"{root} must be private; run chmod 700 on this directory",
        )
        self.root = canonicalize(root)
        for name in ["accounts", "shared", "shared/claude", "shared/claude/projects", "locks", "backups"]:
            ensure_dir(self.root / name)
        history = self.root / "shared/claude/history.jsonl"
        try:
            fd = os.open(history, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            try:
                sync_all(fd)
            finally:
                os.close(fd)
        except FileExistsError:
            ensure(
                stat.S_ISREG(os.lstat(history).st_mode),
                f"Unsafe shared prompt history: {history}",
            )

    def account_home(self, name: str) -> Path:
        valid_name(name)
        return self.root / "accounts" / name / "claude"

    def prepare_account(self, name: str) -> Path:
        home = self.account_home(name)
        ensure_dir(home)
        for link_name, target in [
            ("projects", self.projects()),
            ("history.jsonl", self.root / "shared/claude/history.jsonl"),
        ]:
            link = home / link_name
            try:
                mode = os.lstat(link).st_mode
            except FileNotFoundError:
                os.symlink(target, link)
                continue
            ensure(
                stat.S_ISLNK(mode) and Path(os.readlink(link)) == target,
                f"{link} must link to {target}; existing data was left untouched",
            )
        return home

    def projects(self) -> Path:
        return self.root / "shared/claude/projects"

    def gate(self, shared: bool) -> Lock:
        return Lock(
            self.root / "locks/history.lock",
            shared,
            "History is in use by another Relay launch or import; stop it and retry",
        )

    def session_lock(self) -> Lock:
        return Lock(
            self.root / "locks/session.lock",
            False,
            "Another Relay session is using this history store; exit it before resuming or switching accounts",
        )

    def account_lock(self, name: str, shared: bool) -> Lock:
        valid_name(name)
        return Lock(
            self.root / "locks" / f"account-{name}.lock",
            shared,
            "Another login or account update is in progress; retry after it finishes",
        )

    def registry_lock(self) -> Lock:
        return Lock(
            self.root / "locks/registry.lock",
            False,
            "Another account update is in progress; retry",
        )

    def registry(self) -> Registry:
        path = self.root / "config.json"
        if not try_exists(path):
            # try_exists follows symlinks; a dangling one is still an invalid registry.
            ensure(not os.path.lexists(path), f"Unsafe registry: {path}")
            return Registry()
        data = read_regular(path)
        with context("Invalid config.json; restore a known-good copy rather than resetting it"):
            registry = parse_registry(data)
        ensure(registry.version == 1, f"Unsupported registry version {registry.version}")
        for name in registry.accounts:
            valid_name(name)
        if registry.default is not None:
            ensure(registry.default in registry.accounts, "Default account is missing from registry")
        return registry

    def save(self, registry: Registry):
        write_json(
            self.root / "config.json",
            {
                "version": registry.version,
                "default": registry.default,
                "accounts": {
                    name: {
                        "email": account.email,
                        "org_id": account.org_id,
                        "subscription": account.subscription,
                    }
                    for name, account in sorted(registry.accounts.items())
                },
            },
        )


def parse_registry(data: bytes) -> Registry:
    fields = as_struct(parse_json(data), "Registry", ("version", "default", "accounts"), optional=("default",))
    accounts = {}
    for name, value in as_map(fields["accounts"]).items():
        account = as_struct(value, "Account", ("email", "org_id", "subscription"), optional=("org_id",))
        accounts[name] = Account(
            email=as_str(account["email"]),
            org_id=as_opt_str(account["org_id"]),
            subscription=as_str(account["subscription"]),
        )
    return Registry(
        version=as_u32(fields["version"]),
        default=as_opt_str(fields["default"]),
        accounts=accounts,
    )


def absolute_root(path) -> Path:
    path = os.fspath(path)
    if not path.startswith("/"):
        path = os.path.join(os.getcwd(), path)
    ensure(not any(kind == "parent" for kind, _ in components(path)), "Root cannot contain '..'")
    try:
        mode = os.lstat(path).st_mode
    except OSError:
        mode = None
    if mode is not None:
        ensure(not stat.S_ISLNK(mode), "Application root cannot be a symlink")
        return canonicalize(path)
    normal = [text for kind, text in components(path) if kind == "normal"]
    ensure(normal, "Invalid application root")
    parent = "/" + "/".join(normal[:-1])
    parent = canonicalize(parent) if os.path.exists(parent) else absolute_root(parent)
    return parent / normal[-1]


# --- claude: environment and settings checks, auth probes -----------------------


def protected_env(key: str) -> bool:
    key = key.translate(ASCII_UPPER)
    return (
        key.startswith("ANTHROPIC_")
        or key.startswith("CLAUDE_CODE_OAUTH_")
        or key
        in (
            "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
            "CLAUDE_SECURESTORAGE_CONFIG_DIR",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
            "CLAUDE_CODE_USE_MANTLE",
            "CLAUDE_CODE_USE_ANTHROPIC",
            "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
            "CLAUDE_CODE_SESSION_ACCESS_TOKEN",
            "CLAUDE_CODE_WEBSOCKET_AUTH_FILE_DESCRIPTOR",
            "CLAUDE_CODE_SKIP_PROMPT_HISTORY",
            "CLAUDE_CODE_SUBPROCESS_ENV_SCRUB",
        )
    )


def check_environment():
    for key, value in environment():
        key = key.decode("utf-8", "replace")
        ensure(
            not protected_env(key) or value == b"",
            f"Conflicting environment variable {key}; unset it before using a subscription account (value hidden)",
        )


def check_args(args: List[str]):
    for arg in args:
        flag = os.fsencode(arg).decode("utf-8", "replace").split("=")[0]
        ensure(
            flag
            not in (
                "--settings",
                "--setting-sources",
                "--bare",
                "--no-session-persistence",
                "--worktree",
                "-w",
                "--cwd",
                "--config-dir",
                "--bg",
                "--background",
                "--teleport",
                "--remote-control",
                "--rc",
                "--resume-session-at",
            ),
            f"{flag} is unsupported: it can bypass account checks, change history discovery, or detach the session",
        )
    if args:
        ensure(
            args[0] not in ("auth", "setup-token", "gateway", "install", "update"),
            "Use account add/login for authentication; run only conversation commands through Relay",
        )


def inspect_settings(path: Path):
    try:
        os.lstat(path)
    except FileNotFoundError:
        return
    # Settings links are valid, but unreadable or invalid targets must fail closed.
    with context(f"Cannot inspect settings {path}"):
        resolved = canonicalize(path)
    data = read_regular(resolved)
    with context(f"Invalid settings in {path} (contents hidden)"):
        value = parse_json(data)
    ensure(isinstance(value, Pairs), f"Expected settings object in {path}")
    settings = dict(value)
    for key in ["apiKeyHelper", "forceLoginGatewayUrl", "awsAuthRefresh", "awsCredentialExport"]:
        ensure(
            settings.get(key) in (None, ""),
            f"Conflicting setting {key} in {path} (value hidden)",
        )
    if "forceLoginMethod" in settings:
        ensure(settings["forceLoginMethod"] == "claudeai", f"Conflicting forceLoginMethod in {path}")
    if "env" in settings:
        ensure(isinstance(settings["env"], Pairs), f"Invalid env block in {path}")
        for key, item in sorted(dict(settings["env"]).items()):
            ensure(isinstance(item, str), f"Invalid environment setting {key} in {path}")
            ensure(
                not (protected_env(key) or eq_ignore_ascii_case(key, "CLAUDE_CONFIG_DIR")) or item == "",
                f"Conflicting setting env.{key} in {path} (value hidden)",
            )


def check_settings(home: Path, project: Path):
    for name in ["settings.json", "settings.local.json", "managed-settings.json"]:
        inspect_settings(home / name)
    for ancestor in [project, *project.parents]:
        for name in ["settings.json", "settings.local.json"]:
            inspect_settings(ancestor / ".claude" / name)
        git = ancestor / ".git"
        if os.path.isfile(git):
            # Worktrees can load local settings from the main checkout, which is
            # outside the current directory's ancestors. Submodules have no commondir.
            data = read_regular(canonicalize(git))
            with context("Cannot inspect worktree .git pointer"):
                text = data.decode("utf-8")
            ensure(text.startswith("gitdir: "), "Unrecognized worktree .git pointer")
            pointer = text[len("gitdir: ") :].rstrip()
            with context("Cannot resolve worktree gitdir"):
                gitdir = canonicalize(ancestor / pointer)
            common_path = gitdir / "commondir"
            if try_exists(common_path):
                data = read_regular(common_path)
                with context("Cannot inspect worktree commondir"):
                    common = data.decode("utf-8")
                common = canonicalize(gitdir / common.rstrip())
                ensure(common.parent != common, "Cannot resolve main checkout")
                inspect_settings(common.parent / ".claude/settings.local.json")
            break
        elif os.path.isdir(git):
            break
    managed_root = Path("/Library/Application Support/ClaudeCode")
    inspect_settings(managed_root / "managed-settings.json")
    dropins = managed_root / "managed-settings.d"
    try:
        entries = sorted(os.scandir(dropins), key=lambda entry: entry.name)
    except FileNotFoundError:
        entries = []
    except OSError as error:
        raise Fail(f"Cannot inspect {dropins}: {describe(error)}") from None
    for entry in entries:
        if Path(entry.name).suffix == ".json":
            inspect_settings(Path(entry.path))


def parse_u32(text: str) -> int:
    """Rust's str::parse::<u32>(), errors included."""
    ensure(text != "", "cannot parse integer from empty string")
    ensure(re.fullmatch(r"\+?[0-9]+", text) is not None, "invalid digit found in string")
    value = int(text)
    ensure(value <= 0xFFFFFFFF, "number too large to fit in target type")
    return value


class Claude:
    def __init__(self, explicit: Optional[str]):
        if explicit is not None:
            with context("Cannot resolve --claude executable"):
                binary = canonicalize(explicit)
        else:
            path = getenv("PATH")
            ensure(path is not None, "PATH is unset; provide --claude")
            found = next(
                (
                    candidate
                    for directory in os.fsdecode(path).split(":")
                    for candidate in [Path(directory) / "claude"]
                    if executable_file(candidate)
                ),
                None,
            )
            ensure(found is not None, "Claude CLI not found on PATH; install it or provide --claude /absolute/path")
            binary = canonicalize(found)
        mode = os.stat(binary).st_mode
        ensure(stat.S_ISREG(mode) and mode & 0o111 != 0, "Claude executable is not an executable file")
        ensure(
            canonicalize(__file__) != binary,
            "Claude executable points to Agent Relay itself",
        )
        self.binary = binary

    def environment(self, home: Path) -> dict:
        env = dict(environment())
        env[b"CLAUDE_CONFIG_DIR"] = os.fsencode(home)
        # Avoid inheriting another agent's session identity or nested-session guard.
        for key in [b"CLAUDECODE", b"CLAUDE_CODE_SESSION_ID", b"CLAUDE_CODE_PARENT_SESSION_ID"]:
            env.pop(key, None)
        return dict(sorted(env.items()))

    def spawn(self, home: Path, project: Path, args: List[str], **options) -> subprocess.Popen:
        # close_fds=False: like Rust, children get exactly the descriptors marked inheritable.
        return subprocess.Popen(
            [self.binary, *args],
            cwd=project,
            env=self.environment(home),
            close_fds=False,
            **options,
        )

    def exec(self, home: Path, project: Path, args: List[str]):
        env = self.environment(home)
        sys.stdout.flush()
        sys.stderr.flush()
        # Rust's exec resets SIGPIPE; Python also ignores SIGXFSZ. Claude starts with neither ignored.
        signal.signal(signal.SIGPIPE, signal.SIG_DFL)
        signal.signal(signal.SIGXFSZ, signal.SIG_DFL)
        with context("Cannot execute Claude"):
            os.chdir(project)
            os.execve(self.binary, [self.binary, *args], env)

    def check_version(self, home: Path, project: Path) -> str:
        output = probe(self, home, project, ["--version"], False)
        words = output.split()
        ensure(words, "Missing Claude version")
        version = words[0]
        with context("Unrecognized Claude version; expected 2.1.x"):
            parts = [parse_u32(part) for part in version.split(".")]
        ensure(
            len(parts) == 3 and parts[0] == 2 and parts[1] == 1 and parts[2] >= 268,
            f"Unsupported Claude version {version}; Relay currently requires >=2.1.268 and <2.2.0 for configDirectory auth verification",
        )
        return version

    def auth(self, home: Path, project: Path) -> Account:
        return self.check_auth(home, project, False)

    def auth_for_launch(self, home: Path, project: Path) -> Account:
        return self.check_auth(home, project, True)

    def check_auth(self, home: Path, project: Path, allow_refresh: bool) -> Account:
        with context(
            "Cannot verify saved login; retry first. A status-check failure does not prove that sign-in expired; credentials were left untouched"
        ):
            output = probe(self, home, project, ["auth", "status", "--json"], allow_refresh)
        try:
            status = as_struct(
                parse_json(output.encode("utf-8")),
                "AuthStatus",
                ("loggedIn", "authMethod", "configDirectory", "subscriptionType", "email", "orgId"),
                optional=("subscriptionType", "email", "orgId"),
                deny_unknown=False,
            )
            logged_in = as_bool(status["loggedIn"])
            auth_method = as_str(status["authMethod"])
            config_directory = as_str(status["configDirectory"])
            subscription = as_opt_str(status["subscriptionType"])
            email = as_opt_str(status["email"])
            org_id = as_opt_str(status["orgId"])
        except Exception:
            raise Fail("Unrecognized Claude auth status JSON; no subscription launch was authorized") from None
        ensure(
            (logged_in or allow_refresh) and auth_method == "claude.ai",
            "Claude did not report an active claude.ai subscription login; refusing to launch",
        )
        ensure(
            canonicalize(config_directory) == canonicalize(home),
            "Claude reported a different config directory; refusing to launch the wrong account",
        )
        ensure(subscription is not None, "Claude did not report subscriptionType")
        ensure(
            subscription.translate(ASCII_LOWER) in ("pro", "max", "ultra", "team", "enterprise"),
            "Claude did not report a recognized paid subscription; refusing to launch",
        )
        ensure(email, "Claude did not report an account email")
        if not logged_in:
            # A saved identity is not a token-freshness verdict. As in Omarchy,
            # let Claude use its own refresh credential; never refresh/copy it here.
            say("[i] Saved subscription login identified; Claude will attempt to refresh it if needed")
        return Account(email=email, org_id=org_id, subscription=subscription)


def executable_file(path: Path) -> bool:
    try:
        mode = os.stat(path).st_mode
    except OSError:
        return False
    return stat.S_ISREG(mode) and mode & 0o111 != 0


def probe(claude: Claude, home: Path, project: Path, args: List[str], allow_logged_out: bool) -> str:
    # Write output to an anonymous temporary file: bounded pipe reads can deadlock if
    # an unexpected wrapper fills stdout/stderr before it exits. Never echo stderr,
    # which may include credentials. The temporary file is unlinked before spawning.
    with tempfile.TemporaryFile(prefix=f"agent-relay-probe-{os.getpid()}-") as output:
        child = claude.spawn(
            home, project, args, stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.DEVNULL
        )
        try:
            code = child.wait(timeout=10)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
            raise Fail("Claude local status check timed out after 10 seconds") from None
        exit_text = f"Some({code})" if code >= 0 else "None"
        ensure(
            code == 0 or (allow_logged_out and code == 1),
            f"Claude local status check failed (exit {exit_text}; output hidden)",
        )
        output.seek(0)
        raw = output.read(MIB + 1)
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        raise Fail("stream did not contain valid UTF-8") from None
    ensure(code == 0 or text.strip() != "", "Claude status check exited without structured authentication data")
    ensure(len(raw) <= MIB, "Claude status response exceeded 1 MiB")
    return text


# --- history: import a projects/ directory, with a verified snapshot ------------

PHASES = ("Snapshotting", "Merging", "Complete")


@dataclass
class Entry:
    path: str
    sha256: str


@dataclass
class Manifest:
    version: int
    source: str
    phase: str
    entries: List[Entry]

    def to_json(self):
        return {
            "version": self.version,
            "source": self.source,
            "phase": self.phase,
            "entries": [{"path": entry.path, "sha256": entry.sha256} for entry in self.entries],
        }


def parse_manifest(data: bytes) -> Manifest:
    fields = as_struct(parse_json(data), "Manifest", ("version", "source", "phase", "entries"))
    phase = as_str(fields["phase"])
    ensure(phase in PHASES, f"unknown variant `{phase}`")
    ensure(isinstance(fields["entries"], list) and not isinstance(fields["entries"], Pairs), "invalid type: expected a sequence")
    entries = []
    for value in fields["entries"]:
        entry = as_struct(value, "Entry", ("path", "sha256"))
        entries.append(Entry(path=as_str(entry["path"]), sha256=as_str(entry["sha256"])))
    return Manifest(version=as_u32(fields["version"]), source=as_str(fields["source"]), phase=phase, entries=entries)


def path_key(path: str):
    """Rust orders paths by component, not by raw string."""
    return [os.fsencode(text) for _, text in components(path)]


def inventory(root: Path, current: Path, entries: List[Entry]):
    for name in os.listdir(current):
        path = current / name
        mode = os.lstat(path).st_mode
        ensure(not stat.S_ISLNK(mode), f"History source contains a symlink: {path}")
        if stat.S_ISDIR(mode):
            inventory(root, path, entries)
        else:
            ensure(stat.S_ISREG(mode), f"History source contains a special file: {path}")
            entries.append(Entry(path=str(path.relative_to(root)), sha256=hash_file(path)))


def validate(manifest: Manifest):
    ensure(manifest.version == 1, "Unsupported history manifest version")
    for entry in manifest.entries:
        parts = components(entry.path)
        ensure(
            entry.path != ""
            and all(kind == "normal" for kind, _ in parts)
            and re.fullmatch(r"[0-9a-fA-F]{64}", entry.sha256) is not None,
            "Invalid history manifest entry",
        )


def verify(actual: str, entry: Entry):
    ensure(
        actual == entry.sha256,
        f"History changed or backup verification failed for {entry.path}; original files were not modified",
    )


def transfer(reader, write: Callable) -> str:
    """Copy and hash in one pass, holding only one 1 MiB buffer whatever the file size."""
    digest = hashlib.sha256()
    while True:
        try:
            chunk = reader.read(MIB)
        except InterruptedError:
            continue
        if not chunk:
            break
        write(chunk)
        digest.update(chunk)
    return digest.hexdigest()


def hash_file(path) -> str:
    with open_regular(path) as reader:
        return transfer(reader, lambda chunk: None)


def copy_verified(source: Path, target: Path, entry: Entry):
    with open_regular(source) as reader:
        atomic_write_with(target, lambda file: verify(transfer(reader, file.write), entry))


def import_history(store: Store, source: Path) -> str:
    _gate = store.gate(False)
    ensure(not is_symlink(source), "Source must be a real projects directory, not a symlink")
    source = canonicalize(source)
    ensure(os.path.isdir(source), "Source must be a projects directory")
    for name in [".credentials.json", ".claude.json", "settings.json", "settings.local.json", "auth.json", "history.jsonl"]:
        ensure(
            not try_exists(source / name),
            f"Source contains account configuration ({name}); import its projects/ directory, not the account home",
        )
    ensure(
        not source.is_relative_to(store.root) and not store.root.is_relative_to(source),
        "Source must not overlap Agent Relay storage",
    )
    entries = []
    say("[i] Indexing history; large files may take time")
    inventory(source, source, entries)
    entries.sort(key=lambda entry: path_key(entry.path))
    ensure(entries, "Source projects directory is empty")
    id = f"import-{time.time_ns()}-{os.getpid()}"
    operation = store.root / "backups" / id
    ensure_dir(operation)
    manifest = Manifest(version=1, source=str(source), phase="Snapshotting", entries=entries)
    write_json(operation / "manifest.json", manifest.to_json())
    say(f"[i] Recovery ID: {id}")
    complete(store, operation)
    return id


def recover(store: Store, id: str):
    ensure(re.fullmatch(r"import-[A-Za-z0-9-]*", id) is not None, "Invalid recovery ID")
    _gate = store.gate(False)
    operation = store.root / "backups" / id
    ensure(stat.S_ISDIR(os.lstat(operation).st_mode), "Invalid backup directory")
    complete(store, operation)


def complete(store: Store, operation: Path):
    manifest_path = operation / "manifest.json"
    data = read_regular(manifest_path)
    with context("Invalid recovery manifest; keep the backup and restore its manifest"):
        manifest = parse_manifest(data)
    validate(manifest)
    if manifest.phase == "Complete":
        print("[OK] History import is already complete")
        return
    snapshot = operation / "snapshot"
    if manifest.phase == "Snapshotting":
        ensure_dir(snapshot)
        for entry in manifest.entries:
            target = snapshot / entry.path
            validate_tree_path(snapshot, entry.path)
            if try_exists(target):
                verify(hash_file(target), entry)
            else:
                # Check intermediate components too; never follow substituted links.
                validate_tree_path(Path(manifest.source), entry.path)
                copy_verified(Path(manifest.source) / entry.path, target, entry)
        manifest.phase = "Merging"
        write_json(manifest_path, manifest.to_json())
    conflicts = 0
    for entry in manifest.entries:
        validate_tree_path(snapshot, entry.path)
        copy = snapshot / entry.path
        destination = store.projects() / entry.path
        validate_tree_path(store.projects(), entry.path)
        try:
            mode = os.lstat(destination).st_mode
        except FileNotFoundError:
            copy_verified(copy, destination, entry)
            continue
        ensure(stat.S_ISREG(mode), f"Unsafe existing history entry: {destination}")
        if hash_file(destination) != entry.sha256:
            copy_verified(copy, operation / "conflicts" / entry.path, entry)
            conflicts += 1
        else:
            verify(hash_file(copy), entry)
    manifest.phase = "Complete"
    write_json(manifest_path, manifest.to_json())
    print(f"[OK] Imported {len(manifest.entries)} files; {conflicts} conflicting files retained separately")
    print(f"[i] Verified backup: {operation}")
    print("[i] Source history remains unchanged; use Relay to continue in the imported history")


def validate_tree_path(root: Path, relative: str):
    ensure(not is_symlink(root), f"Unsafe history root: {root}")
    path = root
    for kind, text in components(relative):
        ensure(kind == "normal", "Invalid history path")
        path = path / text
        try:
            mode = os.lstat(path).st_mode
        except FileNotFoundError:
            break
        ensure(not stat.S_ISLNK(mode), f"Unsafe history symlink: {path}")


# --- commands ------------------------------------------------------------------


def default_root() -> Path:
    root = getenv("AGENT_RELAY_HOME")
    if root is not None:
        return Path(os.fsdecode(root))
    home = getenv("HOME")
    ensure(home is not None, "HOME is unset; provide --root")
    base = os.path.join(os.fsdecode(home), "Library/Application Support")
    ensure(base.startswith("/"), "Application data directory must be absolute; provide --root")
    return Path(base) / "agent-relay"


def current_project() -> Path:
    return canonicalize(os.getcwd())


def select_account(store: Store, requested: Optional[str]) -> str:
    registry = store.registry()
    name = requested if requested is not None else registry.default
    ensure(name is not None, "No default account; use account default <name> or run <name>")
    valid_name(name)
    ensure(name in registry.accounts, f"Unknown account {name}; use account add first")
    return name


def preflight(claude: Claude, home: Path, project: Path) -> str:
    check_environment()
    check_settings(home, project)
    return claude.check_version(home, project)


def verify_identity(store: Store, name: str, identity: Account):
    stored = store.registry().accounts.get(name)
    ensure(stored is not None, "Account was removed during launch")
    ensure(
        eq_ignore_ascii_case(stored.email, identity.email) and stored.org_id == identity.org_id,
        f"Account identity changed outside Relay; verify the intended browser account with account login {name}",
    )


def login(store: Store, claude: Claude, name: str, adding: bool):
    valid_name(name)
    gate = store.gate(True)
    account = store.account_lock(name, False)
    exists = name in store.registry().accounts
    ensure(
        exists != adding,
        "Account exists; use account login to renew it" if adding else "Unknown account; use account add first",
    )
    project = current_project()
    home = store.prepare_account(name)
    preflight(claude, home, project)
    say(f"[i] Sign in to the intended Claude subscription for '{name}'. Check the browser account carefully.")
    # Inherit locks during login too: interrupting Relay must not unlock an account
    # while a surviving login subprocess is still changing its credential store.
    gate.inherit()
    account.inherit()
    status = claude.spawn(home, project, ["auth", "login"]).wait()
    ensure(status == 0, "Claude login failed or was cancelled; account directory retained for retry")
    check_environment()
    check_settings(home, project)
    identity = claude.auth(home, project)
    _registry = store.registry_lock()
    registry = store.registry()
    for other_name, other in registry.accounts.items():
        ensure(
            other_name == name
            or not eq_ignore_ascii_case(other.email, identity.email)
            or other.org_id != identity.org_id,
            f"This subscription is already registered as '{other_name}'; no tokens were copied or deleted",
        )
    if not adding:
        old = registry.accounts.get(name)
        ensure(old is not None, "Account disappeared during login")
        ensure(
            eq_ignore_ascii_case(old.email, identity.email) and old.org_id == identity.org_id,
            "Login selected a different identity; registry unchanged. Log in again with the original account, or add a new name",
        )
    registry.accounts[name] = identity
    if registry.default is None:
        registry.default = name
    store.save(registry)
    print(f"[OK] Subscription account '{name}' is ready. Resume with: agent-relay run {name} -- --resume")


def execute(cli):
    store = Store(Path(cli.root) if cli.root is not None else default_root())
    if cli.command == "account" and cli.action == "list":
        registry = store.registry()
        for name, account in sorted(registry.accounts.items()):
            marker = "*" if registry.default == name else " "
            print(f"{marker} {name}\t{account.email}\t{account.subscription}")
    elif cli.command == "account" and cli.action == "default":
        _lock = store.registry_lock()
        registry = store.registry()
        ensure(cli.name in registry.accounts, f"Unknown account {cli.name}; use account add first")
        registry.default = cli.name
        store.save(registry)
        print(f"[OK] Default account: {cli.name}. Running sessions are unchanged.")
    elif cli.command == "account":
        login(store, Claude(cli.claude), cli.name, cli.action == "add")
    elif cli.command == "run":
        check_args(cli.args)
        name = select_account(store, cli.account)
        gate = store.gate(True)
        account = store.account_lock(name, True)
        project = current_project()
        session = store.session_lock()
        home = store.prepare_account(name)
        claude = Claude(cli.claude)
        preflight(claude, home, project)
        identity = claude.auth_for_launch(home, project)
        verify_identity(store, name, identity)
        say(f"[i] Account: {name} ({identity.email})")
        say(f"[i] Shared transcripts: {store.projects()}")
        for lock in [gate, account, session]:
            lock.inherit()
        claude.exec(home, project, cli.args)
    elif cli.command == "doctor":
        name = select_account(store, cli.account)
        _gate = store.gate(True)
        _account = store.account_lock(name, True)
        home = store.prepare_account(name)
        project = current_project()
        claude = Claude(cli.claude)
        version = preflight(claude, home, project)
        identity = claude.auth(home, project)
        verify_identity(store, name, identity)
        print(f"[OK] Claude {version}: {identity.email} ({identity.subscription})")
        print(f"[i] Account: {name}\n[i] Config: {home}\n[i] Shared transcripts: {store.projects()}")
        print("[i] Transcripts and prompt history are shared; checkpoints remain account-local")
    elif cli.command == "history":
        say("[i] Relay locks cannot detect direct Claude processes. Import only after stopping them.")
        if cli.action == "import":
            import_history(store, Path(cli.source))
        else:
            recover(store, cli.id)


# --- command line ----------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    # Like clap's `global = true`: --root and --claude are accepted before or after a subcommand.
    common = argparse.ArgumentParser(add_help=False, allow_abbrev=False)
    common.add_argument("--root", default=argparse.SUPPRESS, help="Application data directory (or AGENT_RELAY_HOME)")
    common.add_argument("--claude", default=argparse.SUPPRESS, help="Official Claude CLI executable; defaults to claude on PATH")

    def subcommands(parser, dest):
        group = parser.add_subparsers(dest=dest, metavar="<command>")
        group.required = True
        return lambda name, help: group.add_parser(name, help=help, description=help, parents=[common], allow_abbrev=False)

    parser = argparse.ArgumentParser(
        prog="agent-relay",
        description="Switch Claude subscriptions and resume shared conversations",
        parents=[common],
        allow_abbrev=False,
    )
    parser.add_argument("-V", "--version", action="version", version=f"agent-relay {VERSION}")
    command = subcommands(parser, "command")

    account = subcommands(command("account", "Add, log in to, list, or select a subscription account"), "action")
    account("add", "Log in to a new subscription in its own stable configuration directory").add_argument("name")
    account("login", "Renew an existing account's login; refuses while Relay sessions use it").add_argument("name")
    account("list", "List registered accounts (does not contact Claude or read credentials)")
    account("default", "Choose the account used when run has no explicit account").add_argument("name")

    run = command("run", "Start the selected account; pass Claude arguments after --")
    run.add_argument("account", nargs="?", help="Account name, or omit to use the default")
    run.set_defaults(args=[])

    command("doctor", "Inspect account identity, subscription authentication, and history links").add_argument(
        "account", nargs="?"
    )

    history = subcommands(command("history", "Explicitly import existing project transcripts without modifying the source"), "action")
    importing = history("import", "Import a projects/ directory; retain verified snapshots and conflicting files")
    importing.add_argument("source")
    importing.add_argument(
        "--agents-stopped",
        action="store_true",
        required=True,
        help="Confirm all direct Claude processes writing the source/destination are stopped",
    )
    recovering = history("recover", "Resume an interrupted import by its printed recovery ID")
    recovering.add_argument("id")
    recovering.add_argument("--agents-stopped", action="store_true", required=True)
    return parser


def parse_cli(argv: List[str]):
    """Parse argv; for `run`, everything after the first `--` goes to Claude untouched."""
    parser = build_parser()
    index = 0
    while index < len(argv) and argv[index].startswith("-"):
        index += 2 if argv[index] in ("--root", "--claude") else 1
    if index < len(argv) and argv[index] == "run" and "--" in argv[index:]:
        split = argv.index("--", index)
        cli = parser.parse_args(argv[:split])
        cli.args = argv[split + 1 :]
    else:
        cli = parser.parse_args(argv)
    # Not argparse defaults: the parsers share these option objects, so a subcommand's
    # default would overwrite a value given before it.
    cli.root = getattr(cli, "root", None)
    cli.claude = getattr(cli, "claude", None)
    return cli


def main():
    # Die on Ctrl-C like the Rust binary (no traceback); print lines as they happen.
    signal.signal(signal.SIGINT, signal.SIG_DFL)
    sys.stdout.reconfigure(line_buffering=True)
    cli = parse_cli(sys.argv[1:])
    try:
        execute(cli)
    except Exception as error:
        say(f"[X] {describe(error)}")
        sys.exit(1)


if __name__ == "__main__":
    main()
