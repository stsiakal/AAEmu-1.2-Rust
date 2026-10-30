"""Unit tests for the pure parts of bots_soak: python -m unittest (run from rust/tools).

No servers, MySQL or lane registry are touched.
"""

from datetime import datetime
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

    def test_npc_character_names_are_unique_per_retry_and_fit_the_limit(self):
        account = bots_soak.run_account_prefix("soak", datetime(2026, 10, 30, 12, 34)) + "b0"
        names = {bots_soak.npc_character_name(account, 7, retry) for retry in range(3)}
        self.assertEqual(len(names), 3)
        for name in names:
            self.assertLessEqual(len(name), 18, name)

    def test_default_prefix_fits_with_retry_suffix(self):
        stamp = datetime(2026, 10, 30, 12, 34)
        account = bots_soak.run_account_prefix("soak", stamp) + "b7"
        name = bots_soak.npc_character_name(account, 99, 2)
        self.assertLessEqual(len(name), 18, name)


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
