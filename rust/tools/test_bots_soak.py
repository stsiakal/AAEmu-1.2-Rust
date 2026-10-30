"""Unit tests for the pure parts of bots_soak: python -m unittest (run from rust/tools).

No servers, MySQL or lane registry are touched.
"""

from datetime import datetime
import contextlib
import io
from pathlib import Path
import sys
import threading
import types
import unittest

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
    def __init__(self, reachable: bool = True):
        self.mysql_port = "3317"
        self.reachable = reachable
        self.calls: list[tuple[str, str]] = []

    def mysql_reachable(self) -> bool:
        return self.reachable

    def stack(self, action: str, log_path: Path, abort=None) -> int:
        self.calls.append((action, str(log_path)))
        return 0


def fake_runner() -> bots_soak.Runner:
    runner = bots_soak.Runner.__new__(bots_soak.Runner)
    runner.abort = threading.Event()
    runner.children = {}
    runner.children_lock = threading.Lock()
    runner.worker_threads = []
    runner.interrupted = False
    runner.started = 0.0
    runner.finished = 0.0
    runner.stack_attempted = False
    runner.run_dir = Path(".")
    return runner


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
        runner.lane = FakeLane()
        runner.stack_attempted = True
        for name in (
            "build",
            "sync_and_configure",
            "configure_runtime",
            "restart_stack",
            "prepare_homes",
            "run_load_phase",
            "soak",
        ):
            setattr(runner, name, lambda: None)

        def interrupt():
            raise KeyboardInterrupt

        runner.provision_all = interrupt
        args = types.SimpleNamespace(no_build=True, no_sync=True, keep_stack=False)
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            interrupted = bots_soak.run_soak(runner, args)

        self.assertTrue(interrupted)
        self.assertTrue(runner.interrupted)
        self.assertTrue(runner.abort.is_set())
        self.assertEqual([call[0] for call in runner.lane.calls], ["stop"])
        for call in runner.lane.calls:
            self.assertNotIn("--mysql", " ".join(call))
        with self.assertRaises(bots_soak.SoakAborted):
            runner.spawn([sys.executable, "-c", "pass"])
        self.assertEqual(runner.children, {})


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
