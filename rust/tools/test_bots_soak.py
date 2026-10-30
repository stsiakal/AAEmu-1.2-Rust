"""Unit tests for the pure parts of bots_soak: python -m unittest (run from rust/tools).

No servers, MySQL or lane registry are touched.
"""

from datetime import datetime
import contextlib
import io
import os
from pathlib import Path
import signal
import sys
import tempfile
import threading
import types
import unittest
from unittest import mock

import bots_soak


class ClassifyAttemptTests(unittest.TestCase):
    def test_clean_exit_without_outcome_file_is_a_failure(self):
        ok, error, steps = bots_soak.classify_attempt(0, False, None, "", "", 300)
        self.assertFalse(ok)
        self.assertEqual(error, "no outcome file")
        self.assertEqual(steps, [])

    def test_outcome_ok_is_a_success(self):
        outcome = {"error": None, "steps": [{"name": "login.auth", "ok": True}]}
        ok, error, steps = bots_soak.classify_attempt(0, False, outcome, "", "", 300)
        self.assertTrue(ok)
        self.assertIsNone(error)
        self.assertEqual(steps, [])

    def test_failed_step_names_and_note(self):
        outcome = {
            "error": None,
            "steps": [
                {"name": "game.move", "ok": False, "note": "disconnected"},
                {"name": "game.leave", "ok": True},
            ],
        }
        ok, error, steps = bots_soak.classify_attempt(0, False, outcome, "", "", 300)
        self.assertFalse(ok)
        self.assertEqual(error, "game.move: disconnected")
        self.assertEqual(steps, ["game.move"])

    def test_outcome_error_wins(self):
        outcome = {"error": "scenario failed: boom", "steps": []}
        ok, error, _ = bots_soak.classify_attempt(0, False, outcome, "", "", 300)
        self.assertFalse(ok)
        self.assertEqual(error, "scenario failed: boom")

    def test_exit_code_without_outcome(self):
        ok, error, _ = bots_soak.classify_attempt(1, False, None, "", "Error: connection refused", 300)
        self.assertFalse(ok)
        self.assertEqual(error, "connection refused")

    def test_exit_code_with_ok_outcome_is_still_a_failure(self):
        outcome = {"error": None, "steps": [{"name": "login.auth", "ok": True}]}
        ok, error, _ = bots_soak.classify_attempt(1, False, outcome, "", "", 300)
        self.assertFalse(ok)
        self.assertEqual(error, "testclient exited with code 1")

    def test_timeout(self):
        ok, error, _ = bots_soak.classify_attempt(None, True, None, "", "", 300)
        self.assertFalse(ok)
        self.assertEqual(error, "timeout after 300s")


class NamingTests(unittest.TestCase):
    def test_account_prefix_contains_the_date(self):
        first = bots_soak.run_account_prefix("soak", datetime(2026, 10, 30, 12, 34, 56))
        second = bots_soak.run_account_prefix("soak", datetime(2026, 11, 30, 12, 34, 56))
        self.assertNotEqual(first, second)
        self.assertTrue(first.startswith("soak1030"))
        self.assertTrue(second.startswith("soak1130"))

    def test_npc_character_name_length_is_bounded_for_every_seq(self):
        account = bots_soak.run_account_prefix("sk", datetime(2026, 10, 30, 12, 34)) + "b0"
        lengths = set()
        for seq in (0, 99, 100, 5000, 1296 * 3 + 7):
            name = bots_soak.npc_character_name(account, seq, 0)
            self.assertLessEqual(len(name), 18, name)
            lengths.add(len(name))
        self.assertEqual(len(lengths), 1, f"sequence encoding changed the name length: {lengths}")

    def test_seq_encoding_wraps_without_reusing_a_name(self):
        account = bots_soak.run_account_prefix("sk", datetime(2026, 10, 30, 12, 34)) + "b0"
        used: set[str] = set()
        first = bots_soak.npc_character_name(account, 0, 0, used)
        wrapped = bots_soak.npc_character_name(account, 1296, 0, used)
        self.assertNotEqual(first, wrapped)
        self.assertEqual(len(wrapped), len(first))

    def test_npc_character_names_are_unique_per_retry(self):
        account = bots_soak.run_account_prefix("sk", datetime(2026, 10, 30, 12, 34)) + "b0"
        names = {bots_soak.npc_character_name(account, 7, retry) for retry in range(3)}
        self.assertEqual(len(names), 3)
        for name in names:
            self.assertLessEqual(len(name), 18, name)

    def test_default_prefix_fits_64_bots(self):
        stamp = datetime(2026, 10, 30, 12, 34)
        accounts = [f"{bots_soak.run_account_prefix('sk', stamp)}b{i}" for i in range(64)]
        self.assertIsNone(bots_soak.npc_name_too_long(accounts))
        self.assertLessEqual(len(bots_soak.npc_character_name(accounts[-1], 5000, 2)), 18)

    def test_old_long_prefix_no_longer_fits_64_bots(self):
        stamp = datetime(2026, 10, 30, 12, 34)
        accounts = [f"{bots_soak.run_account_prefix('soak', stamp)}b{i}" for i in range(64)]
        self.assertIsNotNone(bots_soak.npc_name_too_long(accounts))


class FakeLane:
    def __init__(
        self,
        reachable: bool = True,
        start_rc: int = 0,
        start_message: str | None = None,
        fail_first_stop: bool = False,
    ):
        self.mysql_port = "3317"
        self.login_port = 35237
        self.game_port = 35239
        self.reachable = reachable
        self.start_rc = start_rc
        self.start_message = start_message
        self.fail_first_stop = fail_first_stop
        self.calls: list[tuple[str, str]] = []

    def mysql_reachable(self) -> bool:
        return self.reachable

    def stack(self, action: str, log_path: Path, abort=None) -> int:
        self.calls.append((action, str(log_path)))
        if action == "stop" and self.fail_first_stop:
            self.fail_first_stop = False
            raise KeyboardInterrupt
        if action == "start" and self.start_message:
            Path(log_path).write_text(self.start_message, encoding="utf-8")
        return self.start_rc if action == "start" else 0


def fake_runner() -> bots_soak.Runner:
    runner = bots_soak.Runner.__new__(bots_soak.Runner)
    runner.abort = threading.Event()
    runner.children = {}
    runner.children_lock = threading.Lock()
    runner.lock = threading.Lock()
    runner.worker_threads = []
    runner.interrupted = False
    runner.started = 0.0
    runner.finished = 0.0
    runner.stack_attempted = False
    runner.run_dir = Path(".")
    runner.bug_run_dir = Path(".")
    return runner


def quiet(runner: bots_soak.Runner) -> None:
    for name in ("build", "prepare_homes", "run_load_phase", "soak"):
        setattr(runner, name, lambda: None)


class SetupSafetyTests(unittest.TestCase):
    def test_mysql_down_refuses_before_any_stack_command(self):
        runner = fake_runner()
        runner.lane = FakeLane(reachable=False)
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            with self.assertRaises(SystemExit):
                runner.restart_stack()
        self.assertEqual(runner.lane.calls, [])
        self.assertIn(
            "shared MySQL 3317 is down; start it outside the soak runner",
            stderr.getvalue(),
        )

    def test_mysql_down_refuses_before_any_stack_command_in_run_soak(self):
        runner = fake_runner()
        runner.lane = FakeLane(reachable=False)
        runner.sync_and_configure = lambda: None
        args = types.SimpleNamespace(no_build=True, no_sync=False, keep_stack=False)
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                bots_soak.run_soak(runner, args)
        self.assertEqual(runner.lane.calls, [])
        self.assertFalse(runner.stack_attempted)

    def test_spawn_after_abort_is_refused(self):
        runner = fake_runner()
        runner.abort.set()
        with self.assertRaises(bots_soak.SoakAborted):
            runner.spawn([sys.executable, "-c", "pass"])
        self.assertEqual(runner.children, {})

    def test_interrupt_during_provisioning_stops_once_and_refuses_children(self):
        runner = fake_runner()
        lane = FakeLane()
        runner.lane = lane
        runner.sync_and_configure = lambda: None
        quiet(runner)

        def interrupt():
            raise KeyboardInterrupt

        runner.provision_all = interrupt
        args = types.SimpleNamespace(no_build=True, no_sync=False, keep_stack=False)
        with mock.patch.object(bots_soak, "port_open", return_value=True):
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                interrupted = bots_soak.run_soak(runner, args)

        self.assertTrue(interrupted)
        self.assertTrue(runner.interrupted)
        self.assertTrue(runner.abort.is_set())
        self.assertTrue(runner.stack_attempted)
        self.assertEqual([call[0] for call in lane.calls], ["stop", "start", "stop"])
        with self.assertRaises(bots_soak.SoakAborted):
            runner.spawn([sys.executable, "-c", "pass"])
        self.assertEqual(runner.children, {})

    def test_interrupt_in_initial_stop_still_runs_the_final_stop(self):
        runner = fake_runner()
        lane = FakeLane(fail_first_stop=True)
        runner.lane = lane
        runner.sync_and_configure = lambda: None
        quiet(runner)
        runner.provision_all = lambda: self.fail("provisioning must not run")
        args = types.SimpleNamespace(no_build=True, no_sync=False, keep_stack=False)
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            interrupted = bots_soak.run_soak(runner, args)
        self.assertTrue(interrupted)
        self.assertTrue(runner.stack_attempted)
        self.assertEqual([call[0] for call in lane.calls], ["stop", "stop"])

    def test_live_stack_mysql_down_after_tcp_check_fails_without_provisioning(self):
        runner = fake_runner()
        lane = FakeLane(
            start_rc=1,
            start_message=(
                "MySQL on 127.0.0.1:3317 is not up; start it outside live-stack.sh "
                "(AAEMU_LIVE_NO_MYSQL_START=1 forbids starting it)\n"
            ),
        )
        runner.lane = lane
        runner.sync_and_configure = lambda: None
        quiet(runner)
        provisioned: list[str] = []
        runner.provision_all = lambda: provisioned.append("provision")
        args = types.SimpleNamespace(no_build=True, no_sync=False, keep_stack=False)
        stderr = io.StringIO()
        with tempfile.TemporaryDirectory() as tmp:
            runner.run_dir = Path(tmp)
            with mock.patch.object(bots_soak, "port_open", return_value=True):
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
                    with self.assertRaises(SystemExit):
                        bots_soak.run_soak(runner, args)
        self.assertEqual(provisioned, [])
        self.assertIn("MySQL", stderr.getvalue())
        self.assertEqual([call[0] for call in lane.calls], ["stop", "start", "stop"])

    def test_interrupts_ignored_restores_handlers(self):
        previous = signal.getsignal(signal.SIGINT)
        with bots_soak.interrupts_ignored():
            self.assertIs(signal.getsignal(signal.SIGINT), signal.SIG_IGN)
            if hasattr(signal, "SIGBREAK"):
                self.assertIs(signal.getsignal(signal.SIGBREAK), signal.SIG_IGN)
        self.assertEqual(signal.getsignal(signal.SIGINT), previous)


class StackCommandTests(unittest.TestCase):
    def test_stack_command_sets_no_mysql_start_and_never_uses_mysql_flag(self):
        captured: dict[str, object] = {}

        class Proc:
            pid = 4242

            def wait(self, timeout=None):
                return 0

        def fake_popen(cmd, **kwargs):
            captured["cmd"] = cmd
            captured["env"] = kwargs["env"]
            captured["flags"] = kwargs.get("creationflags", 0)
            return Proc()

        lane = bots_soak.Lane.__new__(bots_soak.Lane)
        lane.name = "bots-soak"
        lane.profile = "live"
        lane.machine = {}
        lane.bash = "bash"
        with tempfile.TemporaryDirectory() as tmp:
            with mock.patch.object(bots_soak.subprocess, "Popen", side_effect=fake_popen):
                rc = lane.stack("stop", Path(tmp) / "stack.log")
        self.assertEqual(rc, 0)
        env = captured["env"]
        self.assertEqual(env["AAEMU_LIVE_NO_MYSQL_START"], "1")
        self.assertNotIn("--mysql", captured["cmd"])
        if os.name == "nt":
            self.assertTrue(captured["flags"] & bots_soak.subprocess.CREATE_NEW_PROCESS_GROUP)
            self.assertTrue(captured["flags"] & bots_soak.subprocess.DETACHED_PROCESS)


class RemainingScenariosTests(unittest.TestCase):
    def test_all_skipped_leaves_no_work(self):
        self.assertEqual(bots_soak.remaining_scenarios(["full", "combat"], {"full", "combat"}), [])

    def test_load_is_not_per_bot_work(self):
        self.assertEqual(bots_soak.remaining_scenarios(["load"], set()), [])

    def test_partially_skipped(self):
        self.assertEqual(
            bots_soak.remaining_scenarios(["full", "combat", "load"], {"combat"}),
            ["full"],
        )


if __name__ == "__main__":
    unittest.main()
