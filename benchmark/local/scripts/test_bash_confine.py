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
        "jq '{name: .name}' data.json",
        'jq "{name: .name}" data.json',
        "sed -n '1{p;}' readme.md",
        "grep -rn \"ok\" . --exclude-dir={node_modules,.git}",
        "grep -n 'fn [a-z]+$' readme.md | head -5",
        'grep -rn "/api/tags" . 2>/dev/null | wc -l',
        "find . -name '*.md' | sort | wc -l",
        "printf '%s\\n' hello | xargs echo",
        "sed -n '1,20p' readme.md",
        "sed -e '1p' -e 's/ok/new/g' readme.md",
        "sed --expression='s/ok/new/g' readme.md",
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

    def test_all_sed_programs_and_joined_write_flags_are_blocked_without_os_sandbox(self):
        from unittest.mock import patch
        (self.root / "program.sed").write_text("w escaped.txt\n")
        commands = [
            "sed -e '1p' -e 'w escaped.txt' readme.md",
            "sed -e '1p' --expression='w escaped.txt' readme.md",
            "sed -e '1p' -e'w escaped.txt' readme.md",
            "sed -f program.sed readme.md",
            "sed -nfprogram.sed readme.md",
            "sed --file=program.sed readme.md",
            "sort -oescaped.txt readme.md",
            "sed -e '1p' -e 'wescaped.txt' readme.md",
            "sed -e '1p' -e '\\|ok|w escaped.txt' readme.md",
            "sed 's/ok/new/w escaped.txt' readme.md",
        ]
        with patch.dict("os.environ", {"FREELLAMA_AGENT_OS_SANDBOX": "off"}):
            with ReadOnlyShell(self.root) as shell:
                for command in commands:
                    with self.subTest(command=command):
                        with self.assertRaisesRegex(ValueError, "sed|sort"):
                            shell.run(command, 5)
                        self.assertFalse((self.root / "escaped.txt").exists())
        self.assertEqual((self.root / "readme.md").read_text(), "ok\n")

    def test_dynamic_and_symlink_paths_are_refused_without_os_sandbox(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as outside:
            external = Path(outside) / "secret"
            external.write_text("outside scratch sentinel\n")
            (self.root / "link").symlink_to(external)
            commands = {
                "cat link": "relative path escapes workspace",
                "grep -flink readme.md": "grep -f",
                "rg -flink readme.md": "rg -f",
                "file -flink": "file -f",
                "jq -flink readme.md": "jq -f",
                "jq --from-file=readme.md readme.md": "jq --from-file",
                "jq -Llink '.' readme.md": "jq -L",
                "wc --files0-from=readme.md": "wc --files0-from",
                "sort --files0-from=readme.md": "sort --files0-from",
                "xargs -alink echo": "xargs argument files",
                "git -Clink log": "git -C",
                "git --git-dir=link log": "relative path escapes workspace",
                "cat li{n,k}": "brace-expanded paths",
                "cat l*": "relative path escapes workspace",
                "printf '%s\\n' '-o' 'escaped.txt' 'readme.md' | xargs sort": "xargs filesystem arguments",
                "printf '/et%s/passwd\\n' c | xargs cat": "xargs filesystem arguments",
                "rg -L ok .": "recursive symlink following",
                "grep -R ok .": "recursive symlink following",
                "find -L .": "recursive symlink following",
            }
            with patch.dict("os.environ", {"FREELLAMA_AGENT_OS_SANDBOX": "off"}):
                with ReadOnlyShell(self.root) as shell:
                    for command, reason in commands.items():
                        with self.subTest(command=command):
                            with self.assertRaisesRegex(ValueError, reason):
                                shell.run(command, 5)
            self.assertFalse((self.root / "escaped.txt").exists())
            self.assertEqual(external.read_text(), "outside scratch sentinel\n")
        self.assertEqual((self.root / "readme.md").read_text(), "ok\n")

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
