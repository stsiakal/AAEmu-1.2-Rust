"""Soak runner for the headless 1.2 client bots, with server bug reports and a dashboard.

    Scripts/bots-soak.sh --minutes 30 --bots 8
    Scripts/bots-soak.sh --minutes 5 --bots 3 --seed 7 --scenarios full,combat,questitem

Run from inside a lane worktree (`Scripts/lane.sh new <name>`). The runner uses the lane's own
runtime directory, ports, databases and target dir; it never touches MySQL (the shared private
instance on 3317 stays up) and stops only its own stack at the end.

What it does:
  * builds (`live` profile) and syncs the lane stack, enables `BugReports` in the lane's runtime
    `rs-game/Config.Local.json` (reports go to <runtime>/soak/<run>/bugreports/), then restarts
    the stack by stopping and starting it (never MySQL);
  * provisions `--bots` accounts (AutoAccount login + game `accounts` row, access level 100 so
    the GM scenarios work), plus the accounts of the one parallel `load` phase;
  * runs the scenarios in a per-bot loop, each cycle shuffled with `--seed`, until `--minutes`
    have passed, recording every outcome (testclient `--outcome-json`);
  * writes <runtime>/soak/<run>/dashboard.md and dashboard.json from the scenario outcomes, the
    bug reporter's `summary.json` and the server log, and leaves the raw logs/reports there.

Scenario failures are data, not runner errors: the exit status is 0 unless the stack or the
provisioning could not be brought up.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import re
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
from collections import Counter
from datetime import datetime
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]

KNOWN_SCENARIOS = [
    "full",
    "combat",
    "questitem",
    "doodad",
    "plant",
    "craft",
    "portal",
    "npc-return",
    "mate-revive",
    "stonethrow",
    "ghost",
    "lunafrost",
    "cometsboon",
]
DEFAULT_SCENARIOS = KNOWN_SCENARIOS + ["load"]

# Scenarios that teleport every bot to the same NPC/doodad/plot. Contention on these is left in
# by default (a soak should load the server); `--exclusive-scenarios` serialises the named ones
# per scenario when clean client-side outcomes matter more than parallel load.
EXCLUSIVE_SCENARIOS: set[str] = set()

SETUP_ERROR_HINTS = [
    r"\bGM\b",
    r"access level|permission|not allowed",
    r"character creation|create character|SCCharacterCreationFailed",
    r"no npc|no giver|not found|no .* in (sight|range)",
    r"no bag item|item .* missing|/item|/kill|/tp",
    r"teleport",
    r"quest",
]

PRINT_LOCK = threading.Lock()


def log(msg: str) -> None:
    with PRINT_LOCK:
        print(msg, flush=True)


def fail(msg: str) -> "NoReturn":  # noqa: F821 - simple script, no typing import
    print(f"bots-soak: {msg}", file=sys.stderr, flush=True)
    raise SystemExit(2)


def read_env_file(path: Path) -> dict[str, str]:
    values: dict[str, str] = {}
    if not path.exists():
        return values
    for raw in path.read_text(encoding="utf-8", errors="replace").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("export "):
            line = line[len("export ") :].strip()
        if "=" not in line:
            continue
        key, value = line.split("=", 1)
        value = value.strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
            value = value[1:-1]
        values[key.strip()] = value
    return values


def native_path(value: str) -> str:
    match = re.match(r"^/([A-Za-z])/(.*)$", value)
    if match:
        return f"{match.group(1).upper()}:/{match.group(2)}"
    return value


def detect_lane() -> str:
    lane = os.environ.get("AAEMU_LANE", "").strip()
    if lane:
        return lane
    git = shutil.which("git")
    if not git:
        fail("git not found on PATH")
    out = subprocess.run(
        [git, "-C", str(REPO), "rev-parse", "--absolute-git-dir"],
        capture_output=True,
        text=True,
        check=False,
    )
    match = re.search(r"[/\\]worktrees[/\\]([^/\\]+)\s*$", out.stdout.strip())
    if not match:
        fail("run from inside a lane worktree (Scripts/lane.sh new <name>)")
    return match.group(1)


def find_bash() -> str:
    explicit = os.environ.get("AAEMU_BASH", "").strip()
    if explicit and Path(native_path(explicit)).exists():
        return native_path(explicit)
    for candidate in (
        "C:/Program Files/Git/bin/bash.exe",
        "C:/Program Files/Git/usr/bin/bash.exe",
        "C:/Program Files (x86)/Git/bin/bash.exe",
    ):
        if Path(candidate).exists():
            return candidate
    found = shutil.which("bash")
    if found and "system32" not in found.lower():
        return found
    fail("Git Bash not found (set AAEMU_BASH to bash.exe)")
    return "bash"  # unreachable


class Lane:
    def __init__(self) -> None:
        self.name = detect_lane()
        self.lanes_dir = Path(native_path(os.environ.get("AAEMU_LANES", "D:/aaemu-lanes")))
        machine = read_env_file(self.lanes_dir / "machine.conf")
        registry = read_env_file(self.lanes_dir / f"{self.name}.env")
        if not registry:
            fail(f"no lane '{self.name}' ({self.lanes_dir / (self.name + '.env')})")
        self.machine = machine

        def setting(key: str, default: str = "") -> str:
            return os.environ.get(key) or machine.get(key) or registry.get(key) or default

        self.live = Path(native_path(setting("LIVE")))
        self.target = Path(native_path(setting("TARGET")))
        self.port_prefix = setting("PORT_PREFIX")
        self.db_game = setting("DB_GAME")
        self.db_login = setting("DB_LOGIN")
        self.profile = os.environ.get("AAEMU_PROFILE") or "live"
        self.bin = Path(native_path(os.environ.get("AAEMU_BIN") or str(self.target / self.profile)))
        self.testclient = self.bin / "aaemu-testclient.exe"
        self.mysql_bin = Path(native_path(os.environ.get("AAEMU_MYSQL_BIN") or machine.get("AAEMU_MYSQL_BIN") or "C:/Program Files/MySQL/MySQL Server 8.0/bin"))
        self.mysql = self.mysql_bin / "mysql.exe"
        self.mysql_port = os.environ.get("AAEMU_MYSQL_PORT") or machine.get("AAEMU_MYSQL_PORT") or "3317"
        self.login = os.environ.get("AAEMU_LOGIN") or f"127.0.0.1:{self.port_prefix}237"
        self.game_port = int(f"{self.port_prefix}239")
        self.login_port = int(self.login.rsplit(":", 1)[1])
        self.password = os.environ.get("AAEMU_VERIFY_PASSWORD") or "testpass"
        self.bash = find_bash()

    def stack(self, action: str, log_path: Path) -> int:
        env = os.environ.copy()
        env.update(
            {
                "AAEMU_LANE": self.name,
                "AAEMU_PROFILE": self.profile,
                "NO_COLOR": "1",
                "RUST_LOG": os.environ.get("AAEMU_SOAK_RUST_LOG", "info"),
            }
        )
        for key in ("AAEMU_MYSQL_BIN", "AAEMU_MYSQL_PORT", "AAEMU_DB_USER"):
            if key not in env and self.machine.get(key):
                env[key] = self.machine[key]
        log_path.parent.mkdir(parents=True, exist_ok=True)
        with log_path.open("ab") as out:
            out.write(f"\n== {datetime.now().isoformat(timespec='seconds')} live-stack.sh {action}\n".encode())
            out.flush()
            kwargs = {}
            if os.name == "nt":
                # A Ctrl+Break aimed at the runner must not kill the servers behind its back;
                # the finally block stops this lane's stack explicitly.
                kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
            proc = subprocess.Popen(
                [self.bash, str(REPO / "Scripts/live-stack.sh"), action],
                cwd=str(REPO),
                env=env,
                stdout=out,
                stderr=subprocess.STDOUT,
                **kwargs,
            )
            try:
                return proc.wait(timeout=1800)
            except subprocess.TimeoutExpired:
                kill_tree(proc.pid)
                proc.wait(timeout=30)
                log(f"[setup] live-stack.sh {action} timed out after 1800s; see {log_path}")
                return 124
            except BaseException:
                kill_tree(proc.pid)
                raise

    def mysql_cmd(self, sql: str, database: str | None = None) -> str:
        cmd = [
            str(self.mysql),
            f"-h127.0.0.1",
            f"-P{self.mysql_port}",
            "-uroot",
            "-N",
            "-B",
        ]
        if database:
            cmd.append(database)
        cmd.extend(["-e", sql])
        proc = subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8", errors="replace")
        if proc.returncode != 0:
            raise RuntimeError(f"mysql failed: {proc.stderr.strip() or proc.stdout.strip()}")
        return proc.stdout.strip()


def port_open(port: int) -> bool:
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=2):
            return True
    except OSError:
        return False


def iso_now() -> str:
    return datetime.now().astimezone().isoformat(timespec="seconds")


def normalize_message(text: str, limit: int = 220) -> str:
    text = re.sub(r"\s+", " ", (text or "").strip())
    text = re.sub(r"\b\d{4,}\b", "#", text)
    if len(text) > limit:
        text = text[: limit - 1] + "\u2026"
    return text


def last_meaningful_line(text: str) -> str:
    for line in reversed((text or "").splitlines()):
        line = line.strip()
        if line:
            return line
    return ""


def strip_error_prefix(text: str) -> str:
    return re.sub(r"^Error:\s*", "", text.strip())


def classify_attempt(
    rc: int | None,
    timed_out: bool,
    outcome: dict | None,
    stdout: str,
    stderr: str,
    timeout_seconds: int,
) -> tuple[bool, str | None, list[str]]:
    """True when the scenario succeeded; (ok, error, failed step names)."""
    failed_steps: list[str] = []
    error: str | None = None
    ok = False
    if timed_out:
        error = f"timeout after {timeout_seconds}s"
    elif outcome is not None:
        failed_steps = [str(s.get("name")) for s in outcome.get("steps", []) if not s.get("ok")]
        ok = outcome.get("error") is None and not failed_steps
        if not ok:
            error = outcome.get("error")
            if not error and failed_steps:
                step = next((s for s in outcome.get("steps", []) if not s.get("ok")), {})
                error = f"{step.get('name')}: {step.get('note') or 'failed'}"
    elif rc == 0:
        # The testclient always writes the outcome file when it ran the scenario; a clean exit
        # without one means the result is unknown, so do not count it as a success.
        error = "no outcome file"
    else:
        error = strip_error_prefix(
            last_meaningful_line(stderr) or last_meaningful_line(stdout) or f"exit code {rc}"
        )
    if rc not in (0, None) and outcome is not None and ok:
        ok = False
        error = error or f"testclient exited with code {rc}"
    if not ok and not error:
        error = f"exit code {rc}" if rc is not None else "testclient failed"
    return ok, error, failed_steps


def run_account_prefix(prefix: str, stamp: datetime) -> str:
    """Per-run account prefix: the user prefix plus month/day/hour/minute of the run."""
    return f"{prefix}{stamp.strftime('%m%d%H%M')}"


def npc_character_name(account: str, seq: int, retries: int) -> str:
    """Unique disposable npc-return character per attempt and per transient retry."""
    suffix = "abcdefghijklmnopqrstuvwxyz"[retries % 26]
    return f"{account}n{seq:02d}{suffix}"


def remaining_scenarios(scenarios: list[str], skipped: set[str]) -> list[str]:
    """Per-bot scenarios that still have work; `load` runs once as its own phase."""
    return [name for name in scenarios if name != "load" and name not in skipped]


def kill_tree(pid: int) -> None:
    if os.name == "nt":
        subprocess.run(
            ["taskkill", "/F", "/T", "/PID", str(pid)],
            capture_output=True,
            check=False,
        )
        return
    try:
        os.kill(pid, signal.SIGKILL)
    except OSError:
        pass


def install_break_handler() -> None:
    """Ctrl+Break is the only console event a new Windows process group still receives."""
    if hasattr(signal, "SIGBREAK"):
        signal.signal(signal.SIGBREAK, signal.default_int_handler)


def join_threads(threads: list[threading.Thread], timeout: float | None = None) -> None:
    """Join in short slices so a console interrupt reaches the main thread promptly."""
    deadline = None if timeout is None else time.monotonic() + timeout
    for thread in threads:
        while thread.is_alive():
            if deadline is not None and time.monotonic() >= deadline:
                return
            thread.join(timeout=0.5)


def parse_outcome(path: Path) -> dict | None:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


class ScenarioStats:
    def __init__(self) -> None:
        self.attempts = 0
        self.ok = 0
        self.fail = 0
        self.errors: Counter[str] = Counter()
        self.error_examples: dict[str, str] = {}
        self.failed_steps: Counter[str] = Counter()
        self.durations: list[int] = []
        self.skipped = False
        self.skip_reason = ""

    def add(self, ok: bool, error: str | None, failed_steps: list[str], ms: int) -> None:
        self.attempts += 1
        self.durations.append(ms)
        if ok:
            self.ok += 1
            return
        self.fail += 1
        if error:
            key = normalize_message(error)
            self.errors[key] += 1
            self.error_examples.setdefault(key, error)
        for step in failed_steps:
            self.failed_steps[step] += 1

    def setup_broken(self, min_attempts: int) -> bool:
        if self.ok or self.attempts < min_attempts:
            return False
        top = self.errors.most_common(1)
        if not top:
            return False
        return any(re.search(hint, top[0][0], re.IGNORECASE) for hint in SETUP_ERROR_HINTS)

    def never_passed(self, min_attempts: int) -> bool:
        return self.ok == 0 and self.attempts >= min_attempts


class Runner:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.lane = Lane()
        self.run_stamp = datetime.now()
        self.run_id = self.run_stamp.strftime("%Y%m%d-%H%M%S")
        self.run_dir = self.lane.live / "soak" / self.run_id
        self.bug_root = self.run_dir / "bugreports"
        self.logs_dir = self.run_dir / "logs"
        self.attempts_path = self.run_dir / "attempts.jsonl"
        self.attempts: list[dict] = []
        self.lock = threading.Lock()
        self.children: dict[int, subprocess.Popen] = {}
        self.children_lock = threading.Lock()
        self.worker_threads: list[threading.Thread] = []
        self.interrupted = False
        self.next_seq_map: dict[str, int] = {}
        self.home: dict[str, dict] = {}
        self.abort = threading.Event()
        self.abort_reason = ""
        self.stats: dict[str, ScenarioStats] = {name: ScenarioStats() for name in args.scenarios}
        exclusive = set(args.exclusive_scenarios) if args.exclusive_scenarios else EXCLUSIVE_SCENARIOS
        self.exclusive = {name: threading.Lock() for name in exclusive if name in args.scenarios}
        self.accounts: list[str] = []
        self.load_accounts: list[str] = []
        self.bot_accounts: list[str] = []
        self.started = 0.0
        self.finished = 0.0
        self.started_wall = ""
        self.server_alive_at_end = False
        self.bug_run_dir: Path | None = None
        self.server_down_reason = ""
        self.stack_started = False

    # ---- child processes ---------------------------------------------------------------------
    def spawn(self, cmd, **kwargs) -> subprocess.Popen:
        # Isolate console events: a Ctrl+C/Ctrl+Break aimed at the runner must not decide the
        # bots' fate; the runner kills its children by pid in the finally block.
        if os.name == "nt":
            kwargs.setdefault("creationflags", subprocess.CREATE_NEW_PROCESS_GROUP)
        proc = subprocess.Popen(cmd, **kwargs)
        with self.children_lock:
            self.children[proc.pid] = proc
        return proc

    def forget(self, proc: subprocess.Popen) -> None:
        with self.children_lock:
            self.children.pop(proc.pid, None)

    def run_capture(
        self, cmd, timeout: float, **kwargs
    ) -> tuple[int | None, str, str, bool]:
        proc = self.spawn(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            **kwargs,
        )
        try:
            stdout, stderr = proc.communicate(timeout=timeout)
            return proc.returncode, stdout or "", stderr or "", False
        except subprocess.TimeoutExpired:
            try:
                proc.kill()
            except OSError:
                pass
            stdout, stderr = proc.communicate()
            return None, stdout or "", stderr or "", True
        finally:
            # Keep a child that was interrupted mid-communicate in the registry so the
            # finally in main can still kill it by pid.
            if proc.poll() is not None:
                self.forget(proc)

    def kill_children(self) -> None:
        with self.children_lock:
            procs = list(self.children.values())
            self.children.clear()
        for proc in procs:
            if proc.poll() is None:
                try:
                    proc.kill()
                except OSError:
                    pass
        for proc in procs:
            try:
                proc.wait(timeout=5)
            except (subprocess.TimeoutExpired, OSError):
                pass

    def stop_workers(self, timeout: float = 10) -> None:
        join_threads(self.worker_threads, timeout)
        self.worker_threads.clear()

    # ---- setup -------------------------------------------------------------------------------
    def build(self) -> None:
        build_log = self.run_dir / "build.log"
        log(f"[setup] building the lane stack ({self.lane.profile} profile) -> {build_log}")
        rc = self.lane.stack("build", build_log)
        if rc != 0:
            tail = last_meaningful_line(build_log.read_text(encoding="utf-8", errors="replace")[-4000:])
            fail(f"live-stack.sh build failed (rc {rc}): {tail}")
        if not self.lane.testclient.exists():
            fail(f"missing {self.lane.testclient} after the build")
        log("[setup] build ok")

    def sync_and_configure(self) -> None:
        sync_log = self.run_dir / "sync.log"
        log("[setup] syncing app config/data into the lane runtime dir")
        if self.lane.stack("sync", sync_log) != 0:
            fail(f"live-stack.sh sync failed ({sync_log})")
        self.configure_runtime()

    def configure_runtime(self) -> None:
        self.bug_root.mkdir(parents=True, exist_ok=True)
        db_user = os.environ.get("AAEMU_DB_USER") or self.lane.machine.get("AAEMU_DB_USER")
        for name in ("rs-login", "rs-game"):
            cfg_path = self.lane.live / name / "Config.Local.json"
            if not cfg_path.exists():
                fail(f"missing {cfg_path}; run Scripts/live-stack.sh sync in the lane first")
            cfg = json.loads(cfg_path.read_text(encoding="utf-8"))
            if db_user:
                cfg.setdefault("Connections", {}).setdefault("MySQLProvider", {})["User"] = db_user
            if name == "rs-game":
                cfg["BugReports"] = {
                    "Enabled": True,
                    "Directory": self.bug_root.as_posix(),
                }
            cfg_path.write_text(json.dumps(cfg, indent=2) + "\n", encoding="utf-8")
        log(f"[setup] BugReports enabled in {self.lane.live / 'rs-game' / 'Config.Local.json'} -> {self.bug_root}")

    def restart_stack(self) -> None:
        log("[setup] stopping this lane's stack (MySQL stays up)")
        self.lane.stack("stop", self.run_dir / "stack-stop.log")
        log("[setup] starting the stack with bug reports enabled")
        rc = self.lane.stack("start", self.run_dir / "stack-start.log")
        if rc != 0:
            tail = last_meaningful_line((self.run_dir / "stack-start.log").read_text(encoding="utf-8", errors="replace")[-4000:])
            fail(f"live-stack.sh start failed (rc {rc}): {tail}")
        self.stack_started = True
        if not port_open(self.lane.login_port) or not port_open(self.lane.game_port):
            fail("the stack did not open its login/game ports")
        if self.find_bug_run() is None:
            log("[setup] WARN: no bug report run directory appeared; check the game log for 'bug reports disabled'")
        log("[setup] stack up")

    def account_id(self, account: str) -> int:
        out = self.lane.mysql_cmd(
            f"SELECT id FROM users WHERE username='{account}' ORDER BY id DESC LIMIT 1",
            self.lane.db_login,
        )
        if not out:
            raise RuntimeError(f"no login account {account}")
        return int(out.splitlines()[0])

    def provision(self, accounts: list[str]) -> list[str]:
        alive: list[str] = []
        for account in accounts:
            outcome = self.run_dir / "setup" / f"{account}-login.json"
            outcome.parent.mkdir(parents=True, exist_ok=True)
            ok = False
            error = ""
            for attempt in (1, 2):
                rc, stdout, stderr, timed_out = self.run_capture(
                    [
                        str(self.lane.testclient),
                        "run",
                        "--scenario",
                        "login",
                        "--login",
                        self.lane.login,
                        "--user",
                        account,
                        "--password",
                        self.lane.password,
                        "--outcome-json",
                        str(outcome),
                    ],
                    timeout=180,
                    cwd=str(self.run_dir),
                )
                if not timed_out and rc == 0:
                    ok = True
                    break
                error = (
                    "timeout after 180s"
                    if timed_out
                    else strip_error_prefix(last_meaningful_line(stderr or stdout))
                )
                time.sleep(1)
            if not ok:
                log(f"[setup] WARN: AutoAccount login failed for {account}: {error}")
                continue
            try:
                uid = self.account_id(account)
                self.lane.mysql_cmd(
                    "INSERT INTO accounts (account_id) VALUES "
                    f"({uid}) ON DUPLICATE KEY UPDATE account_id = VALUES(account_id);",
                    self.lane.db_game,
                )
                self.lane.mysql_cmd(
                    f"UPDATE accounts SET access_level=100 WHERE account_id={uid};",
                    self.lane.db_game,
                )
                alive.append(account)
            except RuntimeError as exc:
                log(f"[setup] WARN: game account provisioning failed for {account}: {exc}")
        return alive

    def provision_all(self) -> None:
        # Fresh accounts (and therefore fresh spawn characters) per run: no stale positions,
        # quest state or dead characters from an earlier soak leak into this one. The prefix
        # carries the run's date and time, so a rerun never reuses an account.
        self.account_prefix = run_account_prefix(self.args.prefix, self.run_stamp)
        self.bot_accounts = [f"{self.account_prefix}b{i}" for i in range(self.args.bots)]
        load_count = 0 if "load" not in self.args.scenarios else self.args.load_bots
        self.load_accounts = [f"{self.account_prefix}l{i}" for i in range(load_count)]
        longest = max(self.bot_accounts + self.load_accounts, key=len, default="")
        if longest and len(npc_character_name(longest, 99, 0)) > 18:
            fail(
                "--prefix is too long: character names must stay within 18 characters "
                f"({npc_character_name(longest, 99, 0)})"
            )
        log(f"[setup] account prefix {self.account_prefix}; provisioning {len(self.bot_accounts) + len(self.load_accounts)} account(s)")
        alive = self.provision(self.bot_accounts + self.load_accounts)
        self.accounts = alive
        self.bot_accounts = [a for a in self.bot_accounts if a in alive]
        self.load_accounts = [a for a in self.load_accounts if a in alive]
        if not self.bot_accounts:
            fail("no bot account could be provisioned")
        if len(self.bot_accounts) < self.args.bots:
            log(f"[setup] WARN: only {len(self.bot_accounts)} of {self.args.bots} bot accounts are usable")
        log(f"[setup] {len(self.bot_accounts)} bot + {len(self.load_accounts)} load account(s) ready")

    # ---- scenario attempts -------------------------------------------------------------------
    def testclient_env(self) -> dict[str, str]:
        env = os.environ.copy()
        env.setdefault("NO_COLOR", "1")
        env.setdefault("RUST_LOG", "warn")
        return env

    def next_seq(self, account: str) -> int:
        with self.lock:
            seq = self.next_seq_map.get(account, 0)
            self.next_seq_map[account] = seq + 1
            return seq

    def read_position(self, name: str) -> dict | None:
        try:
            out = self.lane.mysql_cmd(
                f"SELECT world_id, zone_id, x, y, z, yaw FROM characters WHERE name='{name}' LIMIT 1",
                self.lane.db_game,
            )
        except RuntimeError:
            return None
        fields = out.split("\t") if out else []
        if len(fields) != 6:
            return None
        return dict(zip(("world_id", "zone_id", "x", "y", "z", "yaw"), fields))

    def reset_position(self, account: str) -> None:
        home = self.home.get(account)
        if not home:
            return
        sql = (
            "UPDATE characters SET world_id={world_id}, zone_id={zone_id}, x={x}, y={y}, z={z}, yaw={yaw} "
            f"WHERE name='{account}'".format(**home)
        )
        for attempt in (1, 2):
            try:
                self.lane.mysql_cmd(sql, self.lane.db_game)
                return
            except RuntimeError as exc:
                # Concurrent resets from the other bots can deadlock; a retry is enough.
                if attempt == 1 and "Deadlock" in str(exc):
                    time.sleep(0.5)
                    continue
                log(f"[{account}] position reset failed: {exc}")
                return

    def prepare_attempt(self, name: str, account: str, seq: int, retries: int) -> Path | None:
        if name == "npc-return":
            # The scenario needs a character with no quest 330 history; the server keeps a
            # logged-out character cached, so deleting quest rows from the DB is not enough.
            # A retry must not reuse the name the failed try may already have created.
            character = npc_character_name(account, seq, retries)
            config = self.run_dir / "configs" / f"{account}-{seq:04d}-npc-return-r{retries}.json"
            config.parent.mkdir(parents=True, exist_ok=True)
            config.write_text(
                json.dumps({"character": character, "create_new_character": True}),
                encoding="utf-8",
            )
            return config
        self.reset_position(account)
        return None

    TRANSIENT = re.compile(
        r"enter world denied|already (?:connected|logged)|still pending|"
        r"connection refused|connection reset",
        re.IGNORECASE,
    )

    def run_attempt(self, name: str, account: str) -> dict:
        seq = self.next_seq(account)
        retries = 0
        while True:
            config = self.prepare_attempt(name, account, seq, retries)
            record = self.run_attempt_once(name, account, seq, retries, config)
            record["retries"] = retries
            if (
                record["ok"]
                or retries >= self.args.transient_retries
                or not self.TRANSIENT.search(record.get("error") or "")
                or self.abort.is_set()
            ):
                break
            log(
                f"[{account}] {name} transient failure (retry {retries + 1}): {record['error']}"
            )
            time.sleep(self.args.transient_delay)
            retries += 1
        with self.lock:
            self.attempts.append(record)
            with self.attempts_path.open("a", encoding="utf-8") as out:
                out.write(json.dumps(record) + "\n")
            stats = self.stats.setdefault(name, ScenarioStats())
            stats.add(record["ok"], record["error"], record["failed_steps"], record["ms"])
            self.maybe_skip(name)
        status = "ok" if record["ok"] else "FAIL"
        log(
            f"[{account}] {name:<12} {status:<4} {record['ms'] / 1000:6.1f}s"
            + ("" if record["ok"] else f"  {record['error']}")
        )
        return record

    def run_attempt_once(
        self, name: str, account: str, seq: int, retries: int, config: Path | None
    ) -> dict:
        attempt_dir = self.run_dir / "outcomes" / account
        attempt_dir.mkdir(parents=True, exist_ok=True)
        suffix = f"-r{retries}" if retries else ""
        outcome_path = attempt_dir / f"{seq:04d}-{name}{suffix}.json"
        log_dir = self.logs_dir / account
        log_dir.mkdir(parents=True, exist_ok=True)
        log_path = log_dir / f"{seq:04d}-{name}{suffix}.log"
        cmd = [
            str(self.lane.testclient),
            "run",
            "--scenario",
            name,
            "--login",
            self.lane.login,
            "--user",
            account,
            "--password",
            self.lane.password,
            "--outcome-json",
            str(outcome_path),
        ]
        if config is not None:
            cmd.extend(["--config", str(config)])
        else:
            cmd.extend(["--character", account])
        display = [c if c != self.lane.password else "***" for c in cmd]
        started = iso_now()
        t0 = time.monotonic()
        rc, stdout, stderr, timed_out = self.run_capture(
            cmd,
            timeout=self.args.attempt_timeout,
            cwd=str(self.run_dir),
            env=self.testclient_env(),
        )
        ms = int((time.monotonic() - t0) * 1000)

        with log_path.open("w", encoding="utf-8", errors="replace") as out:
            out.write(f"$ {' '.join(display)}\n\n")
            out.write(stdout)
            out.write("\n--- stderr ---\n")
            out.write(stderr)

        ok, error, failed_steps = classify_attempt(
            rc, timed_out, parse_outcome(outcome_path), stdout, stderr, self.args.attempt_timeout
        )

        record = {
            "scenario": name,
            "account": account,
            "seq": seq,
            "started": started,
            "ms": ms,
            "ok": ok,
            "exit_code": rc,
            "timeout": timed_out,
            "error": error,
            "failed_steps": failed_steps,
            "outcome": outcome_path.relative_to(self.run_dir).as_posix() if outcome_path.exists() else None,
            "log": log_path.relative_to(self.run_dir).as_posix(),
        }
        return record

    def maybe_skip(self, name: str) -> None:
        stats = self.stats[name]
        if stats.skipped or not stats.setup_broken(self.args.max_setup_failures):
            return
        stats.skipped = True
        stats.skip_reason = "setup broken (never passed; " + stats.errors.most_common(1)[0][0] + ")"
        log(f"[runner] skipping '{name}' for the rest of the run: {stats.skip_reason}")

    def run_load_phase(self) -> None:
        if "load" not in self.args.scenarios or not self.load_accounts:
            return
        count = len(self.load_accounts)
        log(f"[soak] parallel load phase: {count} account(s)")
        name = "load"
        log_path = self.logs_dir / "load.log"
        log_path.parent.mkdir(parents=True, exist_ok=True)
        cmd = [
            str(self.lane.testclient),
            "load",
            "--bots",
            str(count),
            "--prefix",
            f"{self.account_prefix}l",
            "--user",
            f"{self.account_prefix}l0",
            "--login",
            self.lane.login,
            "--password",
            self.lane.password,
            "--ramp-ms",
            "500",
        ]
        t0 = time.monotonic()
        rc, stdout, stderr, timed_out = self.run_capture(
            cmd,
            timeout=self.args.attempt_timeout,
            cwd=str(self.run_dir),
            env=self.testclient_env(),
        )
        ms = int((time.monotonic() - t0) * 1000)
        log_path.write_text(stdout + "\n--- stderr ---\n" + stderr, encoding="utf-8", errors="replace")
        ok = not timed_out and rc == 0
        errors = [line[len("error: ") :] for line in stdout.splitlines() if line.startswith("error: ")]
        error = None
        if timed_out:
            error = f"timeout after {self.args.attempt_timeout}s"
        elif not ok:
            error = errors[0] if errors else strip_error_prefix(last_meaningful_line(stderr) or last_meaningful_line(stdout))
        record = {
            "scenario": name,
            "account": f"{self.account_prefix}l*",
            "seq": 0,
            "started": iso_now(),
            "ms": ms,
            "ok": ok,
            "exit_code": rc,
            "timeout": timed_out,
            "error": error,
            "failed_steps": [],
            "outcome": None,
            "log": log_path.relative_to(self.run_dir).as_posix(),
        }
        with self.lock:
            self.attempts.append(record)
            with self.attempts_path.open("a", encoding="utf-8") as out:
                out.write(json.dumps(record) + "\n")
            self.stats[name].add(ok, error, [], ms)
        log(f"[soak] load phase {'ok' if ok else 'FAIL'} {ms / 1000:.1f}s" + ("" if ok else f": {error}"))

    # ---- soak loop ---------------------------------------------------------------------------
    def watchdog(self, stop_at: float) -> None:
        down_since = 0.0
        while not self.abort.is_set() and time.monotonic() < stop_at:
            login_up = port_open(self.lane.login_port)
            game_up = port_open(self.lane.game_port)
            if login_up and game_up:
                down_since = 0.0
            else:
                if down_since == 0.0:
                    down_since = time.monotonic()
                elif time.monotonic() - down_since > 90:
                    self.abort_reason = (
                        f"stack down for {int(time.monotonic() - down_since)}s "
                        f"(login {'up' if login_up else 'down'}, game {'up' if game_up else 'down'})"
                    )
                    log(f"[runner] aborting: {self.abort_reason}")
                    self.abort.set()
                    return
            self.abort.wait(10)

    def bot_worker(self, account: str, index: int, stop_at: float) -> None:
        rng = random.Random(self.args.seed * 100003 + index)
        while not self.abort.is_set() and time.monotonic() < stop_at:
            with self.lock:
                skipped = {name for name, stats in self.stats.items() if stats.skipped}
            order = remaining_scenarios(self.args.scenarios, skipped)
            if not order:
                log(f"[{account}] no per-bot scenarios left to run (all skipped or only load); leaving the soak")
                return
            rng.shuffle(order)
            ran_any = False
            for name in order:
                if self.abort.is_set() or time.monotonic() >= stop_at:
                    break
                with self.lock:
                    if self.stats[name].skipped:
                        continue
                if name in self.exclusive:
                    remaining = stop_at - time.monotonic()
                    if remaining <= 0:
                        break
                    if not self.exclusive[name].acquire(timeout=remaining):
                        continue
                try:
                    self.run_attempt(name, account)
                    ran_any = True
                except Exception as exc:  # never let one bot kill the soak
                    log(f"[{account}] {name} runner error: {exc}")
                    with self.lock:
                        self.stats[name].add(False, f"runner error: {exc}", [], 0)
                finally:
                    if name in self.exclusive:
                        self.exclusive[name].release()
                if self.args.between_seconds > 0 and time.monotonic() < stop_at:
                    time.sleep(self.args.between_seconds)
            if not ran_any and not self.abort.is_set():
                # Nothing became runnable (e.g. every lock holder timed out): never spin.
                self.abort.wait(1)

    def prepare_homes(self) -> None:
        if not self.bot_accounts:
            return
        log("[setup] one 'full' run per bot to create its character and learn the spawn position")

        def worker(account: str) -> None:
            try:
                self.run_attempt("full", account)
            except Exception as exc:
                log(f"[{account}] home run error: {exc}")
            time.sleep(self.args.between_seconds)
            position = self.read_position(account)
            if position:
                with self.lock:
                    self.home[account] = position

        threads = [threading.Thread(target=worker, args=(account,), daemon=True) for account in self.bot_accounts]
        self.worker_threads = threads
        for thread in threads:
            thread.start()
        join_threads(threads)
        self.worker_threads = []
        log(f"[setup] spawn position known for {len(self.home)}/{len(self.bot_accounts)} bot(s)")

    def soak(self) -> None:
        self.started_wall = iso_now()
        self.started = time.monotonic()
        stop_at = self.started + self.args.minutes * 60
        threads = [
            threading.Thread(target=self.bot_worker, args=(account, i, stop_at), daemon=True)
            for i, account in enumerate(self.bot_accounts)
        ]
        watch = threading.Thread(target=self.watchdog, args=(stop_at,), daemon=True)
        log(f"[soak] {len(threads)} bot(s), {self.args.minutes:g} min, seed {self.args.seed}, scenarios: {', '.join(n for n in self.args.scenarios if n != 'load')}")
        self.worker_threads = threads
        watch.start()
        for thread in threads:
            thread.start()
        join_threads(threads)
        self.worker_threads = []
        self.finished = time.monotonic()
        self.server_alive_at_end = port_open(self.lane.game_port)
        self.abort.set()
        join_threads([watch], 15)
        log(f"[soak] done after {(self.finished - self.started) / 60:.1f} min")

    # ---- reporting ---------------------------------------------------------------------------
    def find_bug_run(self) -> Path | None:
        with self.lock:
            if self.bug_run_dir is not None:
                return self.bug_run_dir
            candidates = [p for p in self.bug_root.iterdir() if p.is_dir()] if self.bug_root.exists() else []
            self.bug_run_dir = max(candidates, key=lambda p: p.stat().st_mtime) if candidates else None
            return self.bug_run_dir

    def bug_summary(self) -> dict:
        run_dir = self.find_bug_run()
        if run_dir is None:
            return {"run_dir": None, "fingerprints": [], "systems": [], "error": "no bug report run directory was created"}
        summary_path = run_dir / "summary.json"
        if not summary_path.exists():
            return {
                "run_dir": run_dir.as_posix(),
                "summary": None,
                "fingerprints": [],
                "systems": [],
                "error": "summary.json missing",
            }
        try:
            summary = json.loads(summary_path.read_text(encoding="utf-8"))
        except ValueError as exc:
            return {
                "run_dir": run_dir.as_posix(),
                "summary": None,
                "fingerprints": [],
                "systems": [],
                "error": f"invalid summary.json: {exc}",
            }
        fingerprints = []
        for entry in summary.get("fingerprints", []):
            example = entry.get("example")
            example_path = (run_dir / example) if example else None
            message = entry.get("subject")
            level = None
            if example_path and example_path.exists():
                try:
                    report = json.loads(example_path.read_text(encoding="utf-8"))
                except ValueError:
                    report = {}
                trigger = report.get("trigger") or {}
                message = trigger.get("message") or message
                level = trigger.get("level")
            fingerprints.append(
                {
                    "fingerprint": entry.get("fingerprint"),
                    "kind": entry.get("kind"),
                    "subject": entry.get("subject"),
                    "message": message,
                    "level": level,
                    "severity": entry.get("severity"),
                    "count": entry.get("count", 0),
                    "first": entry.get("first"),
                    "last": entry.get("last"),
                    "example": example_path.as_posix() if example_path else None,
                    "example_relative": (Path("bugreports") / run_dir.name / example).as_posix() if example else None,
                }
            )
        fingerprints.sort(key=lambda e: (-(e.get("count") or 0), str(e.get("fingerprint"))))
        return {
            "run_dir": run_dir.as_posix(),
            "summary": summary_path.as_posix(),
            "updated": summary.get("updated"),
            "fingerprints": fingerprints,
            "systems": summary.get("systems", []),
            "reports": summary.get("reports", 0),
            "full_reports": summary.get("full_reports", 0),
            "capped_reports": summary.get("capped_reports", 0),
            "dropped_events": summary.get("dropped_events", 0),
            "error": None,
        }

    def server_log(self) -> dict:
        result = {"warn": 0, "error": 0, "panics": 0, "top": [], "logs": []}
        messages: Counter[tuple[str, str]] = Counter()
        level_re = re.compile(
            r"\[(?P<level>WARN|ERROR)\]\s+(?P<target>\S+)\s+-\s+(?P<msg>.*)$"
        )
        for name in ("rs-game.log", "rs-login.log"):
            path = self.lane.live / "logs" / name
            if not path.exists():
                continue
            result["logs"].append(path.as_posix())
            for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
                if "panicked at" in line:
                    result["panics"] += 1
                match = level_re.search(line)
                if not match:
                    continue
                level = match.group("level").lower()
                result[level] += 1
                key = (match.group("level"), f"{match.group('target').strip()}: {normalize_message(match.group('msg'))}")
                messages[key] += 1
        result["top"] = [
            {"level": level, "message": message, "count": count}
            for (level, message), count in messages.most_common(15)
        ]
        return result

    def scenario_rows(self) -> list[dict]:
        rows = []
        for name, stats in self.stats.items():
            durations = sorted(stats.durations)
            p50 = durations[len(durations) // 2] if durations else 0
            p95 = durations[min(len(durations) - 1, int(len(durations) * 0.95))] if durations else 0
            setup_broken = stats.setup_broken(self.args.max_setup_failures)
            rows.append(
                {
                    "scenario": name,
                    "attempts": stats.attempts,
                    "ok": stats.ok,
                    "fail": stats.fail,
                    "success_rate": (stats.ok / stats.attempts) if stats.attempts else 0.0,
                    "setup_broken": setup_broken,
                    "never_passed": stats.never_passed(self.args.max_setup_failures),
                    "skipped": stats.skipped,
                    "skip_reason": stats.skip_reason,
                    "errors": [
                        {"message": message, "count": count, "example": stats.error_examples.get(message, message)}
                        for message, count in stats.errors.most_common(5)
                    ],
                    "failed_steps": [
                        {"step": step, "count": count} for step, count in stats.failed_steps.most_common(5)
                    ],
                    "p50_ms": p50,
                    "p95_ms": p95,
                }
            )
        rows.sort(key=lambda r: (-r["fail"], r["scenario"]))
        return rows

    def dashboard(self) -> dict:
        bugs = self.bug_summary()
        rows = self.scenario_rows()
        server = self.server_log()
        total_attempts = sum(r["attempts"] for r in rows)
        total_ok = sum(r["ok"] for r in rows)
        actual = (self.finished - self.started) / 60 if self.finished and self.started else 0.0
        return {
            "run": self.run_id,
            "lane": self.lane.name,
            "started": self.started_wall or None,
            "actual_minutes": round(actual, 2),
            "requested_minutes": self.args.minutes,
            "bots": len(self.bot_accounts),
            "seed": self.args.seed,
            "scenarios": [n for n in self.args.scenarios if n != "load"],
            "login": self.lane.login,
            "runtime": self.lane.live.as_posix(),
            "run_dir": self.run_dir.as_posix(),
            "game_log": (self.lane.live / "logs" / "rs-game.log").as_posix(),
            "bug_reports": bugs,
            "aborted": self.abort.is_set() and bool(self.abort_reason),
            "abort_reason": self.abort_reason or None,
            "interrupted": self.interrupted,
            "server_alive_at_end": self.server_alive_at_end,
            "totals": {
                "attempts": total_attempts,
                "ok": total_ok,
                "fail": total_attempts - total_ok,
                "success_rate": (total_ok / total_attempts) if total_attempts else 0.0,
                "distinct_fingerprints": len(bugs["fingerprints"]),
                "bug_reports": bugs.get("reports", 0),
            },
            "scenario_rows": rows,
            "server_log": server,
        }

    def write_dashboard(self) -> tuple[Path, dict]:
        data = self.dashboard()
        json_path = self.run_dir / "dashboard.json"
        json_path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")

        lines: list[str] = []
        add = lines.append
        add(f"# Bots soak {self.run_id}")
        add("")
        add(f"- Lane: `{self.lane.name}`")
        add(f"- Bots: {data['bots']} (seed {self.args.seed}); requested {self.args.minutes:g} min, ran {data['actual_minutes']:.1f} min")
        add(f"- Scenarios: {', '.join(data['scenarios'])}")
        add(f"- Login: `{self.lane.login}`; runtime: `{self.lane.live.as_posix()}`")
        add(f"- Game log: `{data['game_log']}`")
        add(f"- Server alive at end: {'yes' if data['server_alive_at_end'] else 'no'}")
        if data["interrupted"]:
            add("- **INTERRUPTED**: partial run; the bot clients were killed and the stack stopped")
        if data["aborted"]:
            add(f"- **ABORTED**: {data['abort_reason']}")
        totals = data["totals"]
        add("")
        add("## Totals")
        add("")
        add("| metric | value |")
        add("| --- | --- |")
        add(f"| attempts | {totals['attempts']} |")
        add(f"| success | {totals['ok']} ({totals['success_rate'] * 100:.1f}%) |")
        add(f"| failure | {totals['fail']} |")
        add(f"| distinct bug fingerprints | {totals['distinct_fingerprints']} |")
        add(f"| server bug reports | {totals['bug_reports']} (full: {data['bug_reports'].get('full_reports', 0)}) |")
        add(f"| server WARN / ERROR | {data['server_log']['warn']} / {data['server_log']['error']} ({data['server_log']['panics']} panic lines) |")
        add("")

        add("## Scenarios")
        add("")
        add("| scenario | attempts | ok | fail | rate | p50 | p95 | notes |")
        add("| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |")
        for row in data["scenario_rows"]:
            notes = []
            if row["setup_broken"]:
                notes.append("**setup broken**")
            elif row["never_passed"]:
                notes.append("never passed")
            if row["skipped"]:
                notes.append("skipped")
            if self.args.minutes and row["attempts"] == 0:
                notes.append("no attempts")
            add(
                f"| {row['scenario']} | {row['attempts']} | {row['ok']} | {row['fail']} | "
                f"{row['success_rate'] * 100:.0f}% | {row['p50_ms']} ms | {row['p95_ms']} ms | {'; '.join(notes)} |"
            )
        add("")

        failures = [row for row in data["scenario_rows"] if row["fail"]]
        if failures:
            add("## Failures")
            add("")
            for row in failures:
                add(f"### {row['scenario']} ({row['fail']}/{row['attempts']} failed)")
                if row["skipped"]:
                    add("")
                    add(f"Skipped: {row['skip_reason']}")
                for err in row["errors"]:
                    add(f"- x{err['count']}: `{err['example']}`")
                if row["failed_steps"]:
                    steps = ", ".join(f"{s['step']} (x{s['count']})" for s in row["failed_steps"])
                    add(f"- failing steps: {steps}")
                add("")

        add("## Bug fingerprints")
        add("")
        bugs = data["bug_reports"]
        if bugs.get("error"):
            add(f"Bug reports unavailable: {bugs['error']}")
        elif not bugs["fingerprints"]:
            add("No server bug reports were recorded.")
        else:
            add(f"Report run: `{bugs['run_dir']}`" + (f" (summary updated {bugs['updated']})" if bugs.get("updated") else ""))
            add("")
            add("| count | kind | severity | message / subject | example report |")
            add("| ---: | --- | --- | --- | --- |")
            for fp in bugs["fingerprints"][:40]:
                subject = normalize_message(str(fp.get("message") or fp.get("subject") or ""), 160)
                example = fp.get("example_relative") or "(no full report)"
                add(f"| {fp.get('count')} | {fp.get('kind')} | {fp.get('severity')} | {subject} | `{example}` |")
            if len(bugs["fingerprints"]) > 40:
                add("")
                add(f"{len(bugs['fingerprints']) - 40} more fingerprints in dashboard.json.")
        add("")

        add("## Systems (`bugreport::outcome`)")
        add("")
        systems = data["bug_reports"].get("systems") or []
        if not systems:
            add("No `bugreport::outcome` rows recorded by the server in this run.")
        else:
            add("| system | action | ok | fail | refused | total | success rate |")
            add("| --- | --- | ---: | ---: | ---: | ---: | ---: |")
            for row in systems:
                add(
                    f"| {row.get('system')} | {row.get('action')} | {row.get('ok')} | {row.get('fail')} | "
                    f"{row.get('refused')} | {row.get('total')} | {float(row.get('success_rate') or 0) * 100:.0f}% |"
                )
        add("")

        add("## Server log")
        add("")
        add(f"WARN {data['server_log']['warn']}, ERROR {data['server_log']['error']}, panic lines {data['server_log']['panics']}.")
        if data["server_log"]["top"]:
            add("")
            add("| level | count | message |")
            add("| --- | ---: | --- |")
            for entry in data["server_log"]["top"]:
                add(f"| {entry['level']} | {entry['count']} | `{normalize_message(entry['message'], 180)}` |")
        add("")
        add("## Artifacts")
        add("")
        add(f"- `{self.run_dir.as_posix()}/dashboard.md`, `dashboard.json`")
        add(f"- attempts: `{self.attempts_path.as_posix()}`")
        add(f"- per-attempt logs: `{self.logs_dir.as_posix()}/<account>/`")
        if bugs.get("run_dir"):
            add(f"- server bug reports: `{bugs['run_dir']}`")
        add("")

        md_path = self.run_dir / "dashboard.md"
        md_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
        return md_path, data


def parse_scenarios(value: str) -> list[str]:
    names = [part.strip() for part in value.split(",") if part.strip()]
    unknown = [name for name in names if name not in KNOWN_SCENARIOS and name != "load"]
    if unknown:
        fail(f"unknown scenario(s): {', '.join(unknown)}; known: {', '.join(KNOWN_SCENARIOS)}, load")
    ordered = []
    for name in names:
        if name not in ordered:
            ordered.append(name)
    return ordered


def main(argv: list[str]) -> int:
    install_break_handler()
    parser = argparse.ArgumentParser(
        description="Run the headless-client scenarios with N bots against this lane's stack and write a dashboard.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument("--minutes", type=float, default=30, help="soak duration in minutes (default 30)")
    parser.add_argument("--bots", type=int, default=4, help="parallel bot accounts (default 4)")
    parser.add_argument(
        "--scenarios",
        default=",".join(DEFAULT_SCENARIOS),
        help="comma-separated scenario names (default: all, including the one parallel 'load' phase)",
    )
    parser.add_argument(
        "--exclusive-scenarios",
        default="",
        help="comma-separated scenarios to run one bot at a time (default: none; contention is part of the load)",
    )
    parser.add_argument("--seed", type=int, default=1, help="seed for the per-bot scenario order (default 1)")
    parser.add_argument(
        "--prefix",
        default="soak",
        help="account/character prefix (default soak; a per-run time suffix is appended)",
    )
    parser.add_argument(
        "--load-bots",
        type=int,
        default=None,
        help="accounts in the parallel 'load' phase (default: --bots; 0 disables it)",
    )
    parser.add_argument("--attempt-timeout", type=int, default=300, help="seconds per scenario attempt (default 300)")
    parser.add_argument(
        "--between-seconds",
        type=float,
        default=3,
        help="pause between a bot's scenario attempts, so a session is fully released (default 3)",
    )
    parser.add_argument(
        "--transient-retries",
        type=int,
        default=2,
        help="retries after a transient failure such as 'enter world denied' (default 2)",
    )
    parser.add_argument(
        "--transient-delay",
        type=float,
        default=10,
        help="seconds to wait before retrying a transient failure (default 10)",
    )
    parser.add_argument(
        "--max-setup-failures",
        type=int,
        default=5,
        help="attempts without a success before a never-passing scenario is called setup broken and skipped (default 5)",
    )
    parser.add_argument("--no-build", action="store_true", help="reuse the current lane binaries")
    parser.add_argument("--no-sync", action="store_true", help="do not copy app configs/data into the runtime dir")
    parser.add_argument("--keep-stack", action="store_true", help="leave the stack running at the end")
    parser.add_argument("--dry-run", action="store_true", help="print the resolved plan and exit")
    args = parser.parse_args(argv)
    args.scenarios = parse_scenarios(args.scenarios)
    if args.bots < 1:
        fail("--bots must be at least 1")
    if args.minutes <= 0:
        fail("--minutes must be positive")
    if args.load_bots is None:
        args.load_bots = args.bots
    if "load" in args.scenarios and args.load_bots < 1:
        args.scenarios = [name for name in args.scenarios if name != "load"]

    runner = Runner(args)
    log(f"bots-soak: lane {runner.lane.name}, runtime {runner.lane.live}")
    log(f"bots-soak: run dir {runner.run_dir}")
    if args.dry_run:
        log(
            f"bots-soak: would run {args.bots} bot(s) for {args.minutes:g} min, "
            f"scenarios {', '.join(args.scenarios)}, seed {args.seed}"
        )
        return 0

    runner.run_dir.mkdir(parents=True, exist_ok=True)
    runner.attempts_path.touch()
    interrupted = False
    try:
        if not args.no_build:
            runner.build()
        if not args.no_sync:
            runner.sync_and_configure()
        else:
            runner.configure_runtime()
        runner.restart_stack()
        runner.provision_all()
        runner.prepare_homes()
        runner.run_load_phase()
        runner.soak()
        log("[report] waiting 12 s for the bug reporter's summary refresh")
        time.sleep(12)
    except KeyboardInterrupt:
        interrupted = True
        runner.interrupted = True
        runner.abort.set()
        if runner.started:
            runner.finished = time.monotonic()
        log("[runner] interrupted: killing bot clients and writing a partial dashboard")
    finally:
        runner.kill_children()
        runner.stop_workers()
        runner.kill_children()
        if not args.keep_stack:
            log("[setup] stopping this lane's stack (MySQL stays up)")
            runner.lane.stack("stop", runner.run_dir / "stack-stop.log")
    md, data = runner.write_dashboard()
    totals = data["totals"]
    log(
        f"[report] {totals['attempts']} attempts, {totals['ok']} ok, {totals['fail']} failed, "
        f"{totals['distinct_fingerprints']} bug fingerprints, "
        f"WARN {data['server_log']['warn']} / ERROR {data['server_log']['error']}"
    )
    log(f"[report] dashboard: {md}")
    return 130 if interrupted else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except KeyboardInterrupt:
        raise SystemExit(130)
