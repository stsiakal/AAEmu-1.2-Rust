"""Unit tests for the pure parts of bots_soak: python -m unittest (run from rust/tools).

No servers, MySQL or lane registry are touched.
"""

from datetime import datetime
import contextlib
import io
import os
from pathlib import Path
import re
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


class SlotTests(unittest.TestCase):
    def test_every_bot_gets_its_own_slot_in_list_order(self):
        accounts = [f"sk10301234b{i}" for i in range(8)]
        slots = bots_soak.assign_slots(accounts)
        self.assertEqual([slots[a] for a in accounts], list(range(8)))
        self.assertEqual(len(set(slots.values())), 8)

    def test_a_full_grid_is_unique_and_more_bots_wrap_onto_the_first_slots(self):
        accounts = [f"b{i}" for i in range(bots_soak.SPAWN_SLOTS + 2)]
        slots = bots_soak.assign_slots(accounts)
        self.assertEqual(len({slots[a] for a in accounts[: bots_soak.SPAWN_SLOTS]}), bots_soak.SPAWN_SLOTS)
        self.assertEqual(slots[accounts[bots_soak.SPAWN_SLOTS]], 0)
        self.assertEqual(slots[accounts[bots_soak.SPAWN_SLOTS + 1]], 1)
        self.assertLess(max(slots.values()), bots_soak.SPAWN_SLOTS)

    def test_the_slot_count_matches_the_testclients_grid(self):
        source = (
            bots_soak.REPO / "rust" / "crates" / "aaemu-testclient" / "src" / "scenario" / "coreloop.rs"
        ).read_text(encoding="utf-8")
        match = re.search(r"pub\(super\) const SLOTS: u32 = (\d+);", source)
        self.assertIsNotNone(match, "coreloop.rs no longer defines SLOTS")
        self.assertEqual(int(match.group(1)), bots_soak.SPAWN_SLOTS)

    def provisioned(self, bots: int, scenarios: list[str], lost: str | None = None):
        runner = fake_runner()
        runner.args = types.SimpleNamespace(prefix="sk", bots=bots, scenarios=scenarios, load_bots=0)
        runner.run_stamp = datetime(2026, 10, 30, 12, 34)
        runner.provision = lambda accounts: [a for a in accounts if not (lost and a.endswith(lost))]
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            runner.provision_all()
        return runner, out.getvalue()

    def test_slots_follow_the_accounts_that_were_provisioned(self):
        runner, _ = self.provisioned(5, ["questchain"], lost="b2")
        self.assertEqual(len(runner.bot_accounts), 4)
        self.assertEqual(set(runner.slots), set(runner.bot_accounts))
        self.assertEqual(sorted(runner.slots.values()), [0, 1, 2, 3])

    def test_more_bots_than_slots_are_warned_about_for_questchain_only(self):
        _, out = self.provisioned(bots_soak.SPAWN_SLOTS + 4, ["questchain", "gather"])
        self.assertIn(f"WARN: {bots_soak.SPAWN_SLOTS + 4} bots but {bots_soak.SPAWN_SLOTS} spawn slots", out)
        _, out = self.provisioned(bots_soak.SPAWN_SLOTS + 4, ["gather", "mount"])
        self.assertNotIn("spawn slots", out)
        _, out = self.provisioned(bots_soak.SPAWN_SLOTS, ["questchain"])
        self.assertNotIn("spawn slots", out)


class ScenarioCommandTests(unittest.TestCase):
    def runner(self, slots: dict[str, int], attempt_timeout: int = 300) -> bots_soak.Runner:
        runner = fake_runner()
        runner.lane = types.SimpleNamespace(testclient=Path("tc.exe"), login="127.0.0.1:36237", password="pw")
        runner.args = types.SimpleNamespace(attempt_timeout=attempt_timeout)
        runner.slots = slots
        return runner

    @staticmethod
    def value(cmd: list[str], flag: str) -> str:
        return cmd[cmd.index(flag) + 1]

    def test_the_command_carries_the_bots_slot_and_a_timeout_below_the_attempt_timeout(self):
        runner = self.runner({"sk1b0": 0, "sk1b3": 3})
        cmd = runner.scenario_command("questchain", "sk1b3", Path("out.json"), None)
        self.assertEqual(self.value(cmd, "--slot"), "3")
        self.assertEqual(self.value(cmd, "--scenario-timeout"), "225")
        self.assertEqual(self.value(cmd, "--scenario"), "questchain")
        self.assertEqual(self.value(cmd, "--user"), "sk1b3")
        self.assertEqual(self.value(cmd, "--character"), "sk1b3")
        self.assertEqual(self.value(cmd, "--outcome-json"), "out.json")
        self.assertNotIn("--config", cmd)

    def test_slot_zero_is_passed_as_well(self):
        cmd = self.runner({"sk1b0": 0}).scenario_command("full", "sk1b0", Path("o.json"), None)
        self.assertEqual(self.value(cmd, "--slot"), "0")

    def test_an_account_without_a_slot_gets_no_slot_flag(self):
        cmd = self.runner({"sk1b0": 0}).scenario_command("full", "other", Path("o.json"), None)
        self.assertNotIn("--slot", cmd)

    def test_a_config_file_replaces_the_character_flag(self):
        cmd = self.runner({"sk1b1": 1}).scenario_command("npc-return", "sk1b1", Path("o.json"), Path("c.json"))
        self.assertEqual(self.value(cmd, "--config"), "c.json")
        self.assertNotIn("--character", cmd)
        self.assertEqual(self.value(cmd, "--slot"), "1")

    def test_the_scenario_timeout_leaves_the_cleanup_reserve_and_keeps_a_floor(self):
        self.assertEqual(bots_soak.scenario_timeout(300), 300 - bots_soak.CLEANUP_RESERVE)
        self.assertEqual(bots_soak.scenario_timeout(600), 600 - bots_soak.CLEANUP_RESERVE)
        self.assertEqual(bots_soak.scenario_timeout(60), bots_soak.MIN_SCENARIO_TIMEOUT)
        cmd = self.runner({}, attempt_timeout=480).scenario_command("housing", "a", Path("o.json"), None)
        self.assertEqual(self.value(cmd, "--scenario-timeout"), "405")


class ScenarioListTests(unittest.TestCase):
    def test_the_core_loop_scenarios_are_known_and_part_of_the_default_soak(self):
        for name in ("gather", "mount", "questchain", "housing"):
            self.assertIn(name, bots_soak.KNOWN_SCENARIOS)
            self.assertIn(name, bots_soak.DEFAULT_SCENARIOS)
        self.assertEqual(
            bots_soak.parse_scenarios("gather, mount,questchain,housing,gather"),
            ["gather", "mount", "questchain", "housing"],
        )

    def test_an_unknown_scenario_is_still_refused(self):
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                bots_soak.parse_scenarios("gather,farming")

    def test_every_known_scenario_is_a_testclient_scenario(self):
        source = (
            bots_soak.REPO / "rust" / "crates" / "aaemu-testclient" / "src" / "scenario.rs"
        ).read_text(encoding="utf-8")
        for name in bots_soak.KNOWN_SCENARIOS:
            self.assertIn(f'"{name}" => Self::', source, f"the testclient does not parse '{name}'")


class RunCountersTests(unittest.TestCase):
    def test_only_retry_counters_are_read(self):
        outcome = {
            "vars": {
                "retry_out_of_angle": 2,
                "retry_obstacle": 0,
                "slot": 3,
                "account_id": 9,
                "retry_negative": -1,
                "retry_text": "x",
                "retry_flag": True,
            }
        }
        self.assertEqual(bots_soak.run_counters(outcome), {"retry_out_of_angle": 2, "retry_obstacle": 0})

    def test_missing_outcome_or_vars_have_no_counters(self):
        self.assertEqual(bots_soak.run_counters(None), {})
        self.assertEqual(bots_soak.run_counters({}), {})
        self.assertEqual(bots_soak.run_counters({"vars": None}), {})


class CounterStatsTests(unittest.TestCase):
    def stats(self) -> bots_soak.ScenarioStats:
        stats = bots_soak.ScenarioStats()
        stats.add(True, None, [], 100, {"retry_out_of_angle": 2, "retry_obstacle": 0})
        stats.add(True, None, [], 100, {"retry_out_of_angle": 0, "retry_obstacle": 0})
        stats.add(False, "failed early", ["game.questchain.giver"], 100, None)
        stats.add(True, None, [], 100, {"retry_out_of_angle": 1, "retry_obstacle": 3})
        return stats

    def test_counters_are_summed_and_count_the_attempts_that_had_retries(self):
        stats = self.stats()
        self.assertEqual(stats.counters["retry_out_of_angle"], 3)
        self.assertEqual(stats.counter_hits["retry_out_of_angle"], 2)
        # The attempt that failed before the fight reported nothing.
        self.assertEqual(stats.counter_reports["retry_out_of_angle"], 3)
        self.assertEqual(stats.counters["retry_obstacle"], 3)
        self.assertEqual(stats.counter_hits["retry_obstacle"], 1)
        self.assertEqual(stats.attempts, 4)

    def test_the_rows_and_the_table_show_the_counters(self):
        runner = fake_runner()
        runner.args = types.SimpleNamespace(max_setup_failures=5)
        runner.stats = {"questchain": self.stats(), "gather": bots_soak.ScenarioStats()}
        rows = runner.scenario_rows()
        questchain = next(row for row in rows if row["scenario"] == "questchain")
        self.assertEqual(
            questchain["counters"],
            [
                {"name": "retry_obstacle", "total": 3, "attempts_with": 1, "attempts_reporting": 3},
                {"name": "retry_out_of_angle", "total": 3, "attempts_with": 2, "attempts_reporting": 3},
            ],
        )
        table = "\n".join(bots_soak.counter_table(rows))
        self.assertIn("## Retry counters", table)
        self.assertIn("| questchain | retry_out_of_angle | 3 | 2 | 3 |", table)
        self.assertIn("| questchain | retry_obstacle | 3 | 1 | 3 |", table)
        # A dashboard without any counter has no such section.
        gather = [row for row in rows if row["scenario"] == "gather"]
        self.assertEqual(bots_soak.counter_table(gather), [])


if __name__ == "__main__":
    unittest.main()
