#!/usr/bin/env python3
"""Regression contracts for AST-derived holdout truth."""

import unittest
from pathlib import Path

from build_truth import unique_distinctive_caller


class UniqueCallerTests(unittest.TestCase):
    def test_generic_caller_still_makes_question_ambiguous(self) -> None:
        sites = {
            (Path("models.py"), "prepare"),
            (Path("sessions.py"), "rebuild_auth"),
        }
        self.assertIsNone(unique_distinctive_caller(sites, "prepare_auth"))

    def test_single_generic_caller_is_not_loosely_gradable(self) -> None:
        sites = {(Path("environment.py"), "parse")}
        self.assertIsNone(unique_distinctive_caller(sites, "set_environment"))

    def test_one_distinctive_caller_is_kept(self) -> None:
        expected = (Path("helpers.py"), "render_template")
        self.assertEqual(unique_distinctive_caller({expected}, "htmlsafe_json_dumps"), expected)


if __name__ == "__main__":
    unittest.main()
