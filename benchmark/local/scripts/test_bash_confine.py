#!/usr/bin/env python3
"""Regression: the default MCP adapter must not read outside the workspace root."""
from pathlib import Path
import tempfile
import unittest

from bash_agent import assert_command_confined
from shell_sandbox import ReadOnlyShell, validate_command


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


class ReadOnlyShellTests(unittest.TestCase):
    """Every command here got past the old regex denylist in front of `bash -c`."""

    BYPASSES = [
        "echo hi > readme.md",
        "rm -rf ./x",
        "git checkout -- .",
        'cat $(printf "\\057etc\\057hostname")',
        "cat `printf /etc/hostname`",
        "cd / && cat etc/hostname",
        "python3 -c \"open('/etc/hostname')\"",
        "cat /et?/hostname",
        "cat ${PWD%/*}/secret",
        "cat $'\\057etc\\057hostname'",
        "find . -name '*.md' -delete",
        "find . -exec rm {} ;",
        "sed -i s/ok/bad/ readme.md",
        "sed -n 'w out.txt' readme.md",
        "sort -o readme.md readme.md",
        "rg --pre ./evil ok",
        "git -c core.pager=sh log",
        "xargs rm < readme.md",
        "awk 'BEGIN{system(\"id\")}'",
        "env",
        "FOO=1 cat readme.md",
        "for f in *; do cat $f; done",
        "(cat readme.md)",
        "cat readme.md &",
        "cat readme.md >> readme.md",
        "tee readme.md < readme.md",
    ]

    ALLOWED = [
        "cat readme.md",
        "grep -rn \"ok\" . --exclude-dir={node_modules,.git}",
        "grep -n 'fn [a-z]+$' readme.md | head -5",
        'grep -rn "/api/tags" . 2>/dev/null | wc -l',
        "find . -name '*.md' | sort | xargs wc -l",
        "sed -n '1,20p' readme.md",
        "git log --oneline -3",
        "ls -la && wc -l readme.md",
        'grep -n "Vec<String>" readme.md',
    ]

    def setUp(self):
        self.td = tempfile.TemporaryDirectory()
        self.root = Path(self.td.name)
        (self.root / "readme.md").write_text("ok\n")

    def tearDown(self):
        self.td.cleanup()

    def test_known_bypasses_are_refused(self):
        for command in self.BYPASSES:
            with self.subTest(command=command):
                with self.assertRaises(ValueError):
                    validate_command(self.root, command)

    def test_research_commands_are_allowed(self):
        for command in self.ALLOWED:
            with self.subTest(command=command):
                validate_command(self.root, command)

    def test_commands_run_and_workspace_is_untouched(self):
        with ReadOnlyShell(self.root) as shell:
            self.assertIn("ok", shell.run("cat readme.md", 10))
            self.assertIn("1", shell.run("grep -c ok readme.md 2>/dev/null", 10))
            with self.assertRaises(ValueError):
                shell.run("echo gone > readme.md", 10)
        self.assertEqual((self.root / "readme.md").read_text(), "ok\n")

    def test_environment_is_scrubbed(self):
        import os
        os.environ["FREELLAMA_TEST_SECRET"] = "s3cret"
        try:
            with ReadOnlyShell(self.root) as shell:
                # `env` is not an allowed tool, and printf cannot expand variables past validation.
                with self.assertRaises(ValueError):
                    shell.run("env", 10)
                self.assertNotIn("s3cret", shell.run("grep -r s3cret . ; echo done", 10))
                self.assertNotIn("FREELLAMA_TEST_SECRET", shell._env)
        finally:
            del os.environ["FREELLAMA_TEST_SECRET"]

    def test_restricted_bash_backs_up_validation(self):
        # Even with validation skipped, rbash refuses output redirection and slashed commands.
        with ReadOnlyShell(self.root) as shell:
            import subprocess
            result = subprocess.run(
                [*shell._prefix, shell._bash, "--restricted", "-c", "echo x > readme.md; /bin/rm readme.md"],
                cwd=self.root, env=shell._env, capture_output=True, text=True, check=False,
            )
            self.assertIn("restricted", result.stderr)
        self.assertEqual((self.root / "readme.md").read_text(), "ok\n")


if __name__ == "__main__":
    unittest.main()
