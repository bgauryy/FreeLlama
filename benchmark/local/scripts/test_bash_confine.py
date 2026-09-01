#!/usr/bin/env python3
"""Regression: the default MCP adapter must not read outside the workspace root."""
from pathlib import Path
import tempfile
import unittest

from bash_agent import assert_command_confined


class ConfineTests(unittest.TestCase):
    def setUp(self):
        self.td = tempfile.TemporaryDirectory()
        self.root = Path(self.td.name)
        (self.root / "readme.md").write_text("ok\n")

    def tearDown(self):
        self.td.cleanup()

    def test_relative_read_is_allowed(self):
        assert_command_confined(self.root, "cat readme.md")

    def test_absolute_etc_is_blocked(self):
        with self.assertRaises(ValueError) as ctx:
            assert_command_confined(self.root, "cat /etc/hosts")
        self.assertIn("escapes workspace", str(ctx.exception))

    def test_home_is_blocked(self):
        with self.assertRaises(ValueError):
            assert_command_confined(self.root, "ls $HOME")
        with self.assertRaises(ValueError):
            assert_command_confined(self.root, "cat ~/.ssh/id_rsa")

    def test_dotdot_is_blocked(self):
        with self.assertRaises(ValueError):
            assert_command_confined(self.root, "cat ../secret")
        with self.assertRaises(ValueError):
            assert_command_confined(self.root, "ls ..")


if __name__ == "__main__":
    unittest.main()
