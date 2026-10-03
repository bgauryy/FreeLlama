"""Read-only execution of model-written shell commands.

`delegate_research` runs the bash adapter against the caller's *real* workspace, so a command is
untrusted input with the user's privileges. A regex denylist in front of `bash -c` let
`echo x > README.md`, `rm -rf ./x`, `git checkout -- .` and `cat $(printf "\\057etc\\057hostname")`
through, and the child inherited every secret in the MCP server's environment.

Confinement is layered so that no single check carries the whole guarantee:

1. **Command validation** (always): every pipeline segment must start with an allowlisted
   read-only tool, tool flags that write or execute (``find -delete``, ``sed -i``, ``rg --pre`` ...)
   are refused, and shell features that hide a command or a path from validation (command/process
   substitution, parameter expansion, ANSI-C quoting, subshells, loops) are refused.
2. **Restricted bash** (always): ``bash -r`` refuses output redirection, ``cd``, command names with
   a slash, and changing ``PATH``; ``PATH`` holds only symlinks to the allowlisted tools, so
   ``python3``, ``rm`` or ``curl`` do not exist for the command.
3. **Scrubbed environment** (always): only locale variables survive; ``HOME`` is the workspace.
4. **OS sandbox** (when available): ``bwrap`` on Linux mounts only system directories and the
   workspace read-only with no network; ``sandbox-exec`` on macOS denies writes, network, and reads
   under ``/Users`` and ``/Volumes`` outside the workspace. Each is probed once and skipped if the
   host refuses it (for example a container without user namespaces).
"""

from __future__ import annotations

import glob
import os
import re
import shlex
import shutil
import subprocess
import tempfile
from dataclasses import dataclass, field
from pathlib import Path

# Tools that only read. `awk` is absent on purpose: `system()`, `print >` and `cmd | getline` make
# any awk program a general executor, and mawk/BSD awk have no sandbox flag.
READ_ONLY_TOOLS = (
    "ls", "find", "grep", "egrep", "fgrep", "rg", "cat", "head", "tail", "wc", "sort", "uniq",
    "cut", "tr", "nl", "file", "stat", "du", "tree", "basename", "dirname", "realpath", "readlink",
    "sed", "jq", "diff", "cmp", "column", "xargs", "git", "tac", "fold", "paste", "comm", "od",
    "strings", "seq", "expr",
)
SAFE_BUILTINS = frozenset({"echo", "printf", "true", "false", "test", "[", "pwd"})
READ_ONLY_GIT = frozenset({
    "log", "show", "grep", "ls-files", "ls-tree", "diff", "blame", "status", "rev-parse",
    "cat-file", "shortlog", "describe", "rev-list", "name-rev", "annotate",
})

# Per-tool flags that write a file or run another program.
_FORBIDDEN_FLAGS: dict[str, tuple[str, ...]] = {
    "find": ("-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprint0", "-fprintf", "-fls"),
    "sort": ("-o", "--output", "--compress-program", "--files0-from"),
    "wc": ("--files0-from",),
    "grep": ("-f", "--file"),
    "egrep": ("-f", "--file"),
    "fgrep": ("-f", "--file"),
    "rg": ("--pre", "--pre-glob", "--search-zip", "-z", "-f", "--file"),
    "tree": ("-o",),
    "file": ("-C", "--compile", "-f", "--files-from"),
    "jq": ("-f", "--from-file", "-L", "--library-path"),
    "git": ("-C", "-c", "--config-env", "--exec-path", "--output", "-o", "--ext-diff", "--textconv", "--open-files-in-pager", "-O"),
    "diff": ("--to-file",),
}
_SED_IN_PLACE = re.compile(r"^(?:-[a-zA-Z]*i|--in-place)")

# Shell syntax that would hide a command or a path from validation.
_HIDDEN_EXPANSION = re.compile(r"\$\(|`|<\(|>\(|\$'|\$\{|\$[A-Za-z_0-9@*#?!$-]")
_ALLOWED_STDERR_REDIRECT = re.compile(r"(?<![\w>&])2>\s*/dev/null|(?<![\w>&])2>&1")

_HOME_ESCAPE = re.compile(r"(?:^|[\s=\"'])(?:~|\$HOME\b|\$\{HOME\})")
_ABS_PATH = re.compile(r"(?:^|[\s=\"'(:,])(/[^\s\"';|&)]*)")
_DOTDOT_PATH = re.compile(r"(?:^|[\s=\"'/])\.\.(?:/|[\s\"']|$)")
_FS_ABS_PREFIX = re.compile(
    r"^/(?:etc|usr|home|Users|var|tmp|private|opt|root|System|Library|bin|sbin|dev|proc|sys|"
    r"Applications|Volumes|mnt|media|srv|run|nix|snap)(?:/|$)"
)
_GLOB_CHARS = re.compile(r"[*?\[]")


def _outside_single_quotes(command: str) -> str:
    """The command with single-quoted spans blanked, since bash expands nothing inside them."""
    out: list[str] = []
    quoted = False
    escaped = False
    for char in command:
        if quoted:
            quoted = char != "'"
            out.append(" ")
            continue
        if escaped:
            escaped = False
            out.append(" ")
            continue
        if char == "\\":
            escaped = True
            out.append(" ")
            continue
        if char == "'":
            quoted = True
            # Keep the quote after an unquoted `$` so `$'\057etc'` (ANSI-C quoting) stays visible.
            out.append("'" if out and out[-1] == "$" else " ")
            continue
        out.append(char)
    return "".join(out)


def assert_command_confined(root: Path, command_text: str) -> None:
    """Reject command text that names a path outside the workspace."""
    if _HOME_ESCAPE.search(command_text):
        raise ValueError("command blocked: home-directory path is outside the workspace")
    if _DOTDOT_PATH.search(command_text):
        raise ValueError("command blocked: '..' walks outside the workspace")
    root = root.resolve()
    for match in _ABS_PATH.finditer(command_text):
        raw = match.group(1)
        if _GLOB_CHARS.search(raw):
            # `/et?/hostname` exists only after expansion; judge every match, and treat a
            # pattern that matches nothing inside the workspace as an escape.
            matches = [Path(item) for item in glob.glob(raw)]
            if not matches and not raw.startswith(str(root)):
                raise ValueError(f"command blocked: path pattern escapes workspace: {raw}")
            candidates = matches
        else:
            candidate = Path(raw)
            if _FS_ABS_PREFIX.match(raw) is None and not candidate.exists():
                continue  # e.g. a URL path in a grep pattern, not a filesystem path
            candidates = [candidate]
        for candidate in candidates:
            resolved = candidate.resolve()
            if resolved != root and root not in resolved.parents:
                raise ValueError(f"command blocked: path escapes workspace: {raw}")


def _segments(command_text: str) -> list[list[str]]:
    lexer = shlex.shlex(command_text, posix=True, punctuation_chars=";&|()<>")
    lexer.whitespace_split = True
    lexer.commenters = ""
    segments: list[list[str]] = [[]]
    for token in lexer:
        if token in {"|", "||", "&&", ";"}:
            segments.append([])
        elif token and set(token) <= set(";&|()<>"):
            if token == "<":
                segments[-1].append(token)
                continue
            raise ValueError(
                f"command blocked: shell operator {token!r} is not allowed (read-only research: "
                "no output redirection, background jobs or subshells)"
            )
        else:
            segments[-1].append(token)
    return [segment for segment in segments if segment]


def _check_tool(words: list[str]) -> None:
    if not words:
        return
    name = words[0]
    args = words[1:]
    if "/" in name:
        raise ValueError(f"command blocked: run tools by name, not path: {name}")
    if name in SAFE_BUILTINS:
        return
    if name not in READ_ONLY_TOOLS:
        raise ValueError(
            f"command blocked: {name!r} is not a read-only research tool "
            f"(allowed: {', '.join(READ_ONLY_TOOLS)})"
        )
    for flag in _FORBIDDEN_FLAGS.get(name, ()):
        if any(
            arg == flag or arg.startswith(flag + "=")
            or (len(flag) == 2 and flag.startswith("-") and not arg.startswith("--")
                and arg.startswith("-") and flag[1] in arg[1:])
            for arg in args
        ):
            raise ValueError(f"command blocked: `{name} {flag}` loads external arguments, writes or executes")
    if name == "sed":
        for arg in args:
            if _SED_IN_PLACE.match(arg):
                raise ValueError("command blocked: sed -i edits files")
        scripts = _sed_programs(args)
        for script in scripts:
            _check_sed_program(script)
    if name == "git":
        subcommand = next((arg for arg in args if not arg.startswith("-")), None)
        if subcommand not in READ_ONLY_GIT:
            raise ValueError(
                f"command blocked: `git {subcommand}` is not read-only "
                f"(allowed: {', '.join(sorted(READ_ONLY_GIT))})"
            )
    if name == "xargs":
        if any(arg == "--arg-file" or arg.startswith("--arg-file=")
               or (arg.startswith("-a") and not arg.startswith("--")) for arg in args):
            raise ValueError("command blocked: xargs argument files cannot be confined")
        target = _xargs_command(args)
        if target[0] not in {"echo", "printf", "true", "false"}:
            raise ValueError("command blocked: xargs filesystem arguments cannot be confined")
        _check_tool(target)


def _check_sed_program(program: str) -> None:
    """Accept the bounded read-only sed grammar, including addressed substitutions."""
    index = 0

    def delimited(start: int) -> int:
        delimiter = program[start]
        cursor = start + 1
        while cursor < len(program):
            if program[cursor] == "\\":
                cursor += 2
            elif program[cursor] == delimiter:
                return cursor + 1
            else:
                cursor += 1
        raise ValueError("command blocked: unterminated sed expression")

    while index < len(program):
        while index < len(program) and (program[index].isspace() or program[index] in ";{}"):
            index += 1
        if index == len(program):
            return
        # An address may be numeric, last-line, or a delimited regular expression.
        for address in range(2):
            if index < len(program) and program[index].isdigit():
                while index < len(program) and program[index].isdigit():
                    index += 1
            elif index < len(program) and program[index] == "$":
                index += 1
            elif index < len(program) and program[index] == "/":
                index = delimited(index)
            elif index + 1 < len(program) and program[index] == "\\":
                index = delimited(index + 1)
            else:
                break
            while index < len(program) and program[index].isspace():
                index += 1
            if address == 0 and index < len(program) and program[index] == ",":
                index += 1
                continue
            break
        while index < len(program) and (program[index].isspace() or program[index] == "!"):
            index += 1
        if index == len(program):
            raise ValueError("command blocked: sed command is missing")
        command = program[index]
        index += 1
        if command == "s" and index < len(program):
            index = delimited(index)
            # The replacement shares the first delimiter, so include it in the next scan.
            index = delimited(index - 1)
            flags_start = index
            while index < len(program) and program[index] not in ";}\n":
                index += 1
            flags = program[flags_start:index].strip()
            if any(flag not in "gIp0123456789" for flag in flags):
                raise ValueError("command blocked: sed substitution flags can read, write or execute")
        elif command not in "pPdDqQnNhHgGx={}":
            raise ValueError("command blocked: sed command can read, write or execute files")
        if command not in "{}" and index < len(program) and not (program[index].isspace() or program[index] in ";{}"):
            raise ValueError("command blocked: unsupported sed command argument")


def _sed_programs(args: list[str]) -> list[str]:
    """Inspect every inline program; loading another file would hide its commands."""
    programs: list[str] = []
    index = 0
    while index < len(args):
        arg = args[index]
        if arg == "--":
            programs.extend(args[index + 1:])
            break
        if arg == "--file" or arg.startswith("--file="):
            raise ValueError("command blocked: sed script files can read, write or execute")
        if arg == "--expression":
            index += 1
            if index >= len(args):
                raise ValueError("command blocked: sed expression is missing")
            programs.append(args[index])
        elif arg.startswith("--expression="):
            programs.append(arg.split("=", 1)[1])
        elif arg.startswith("-") and not arg.startswith("--"):
            # n/E/r are no-argument mode switches. e/f end a short-option cluster and
            # consume the remainder (or next argument), so attached programs stay visible.
            short = arg[1:]
            for offset, option in enumerate(short):
                if option == "f":
                    raise ValueError("command blocked: sed script files can read, write or execute")
                if option == "e":
                    program = short[offset + 1:]
                    if not program:
                        index += 1
                        if index >= len(args):
                            raise ValueError("command blocked: sed expression is missing")
                        program = args[index]
                    programs.append(program)
                    break
                if option not in "nEr":
                    raise ValueError("command blocked: unsupported sed option")
        elif not arg.startswith("--"):
            if not programs:
                programs.append(arg)
        else:
            raise ValueError("command blocked: unsupported sed option")
        index += 1
    return programs


def _xargs_command(args: list[str]) -> list[str]:
    with_value = {"-n", "-I", "-d", "-P", "-L", "-s", "-E", "-a", "--arg-file", "--delimiter", "--max-args", "--max-procs"}
    index = 0
    while index < len(args) and args[index].startswith("-"):
        index += 2 if args[index] in with_value else 1
    command = args[index:]
    return command or ["echo"]


def validate_command(root: Path, command_text: str) -> str:
    """Return the command to run under restricted bash, or raise ValueError naming the refusal."""
    if not command_text.strip():
        raise ValueError("empty shell command")
    if "\n" in command_text.strip():
        raise ValueError("command blocked: send one command line per turn")
    # stderr is captured together with stdout already, so these two redirects are no-ops that
    # models write out of habit; rbash would refuse them as output redirection.
    command_text = _ALLOWED_STDERR_REDIRECT.sub(" ", command_text).strip()
    unquoted = _outside_single_quotes(command_text)
    if _HIDDEN_EXPANSION.search(unquoted):
        raise ValueError(
            "command blocked: command substitution, process substitution and $-expansion are "
            "not allowed; write paths and patterns literally"
        )
    # Quoted program braces are literal; unquoted brace paths expand before execution.
    brace_tokens = shlex.shlex(command_text, posix=False)
    brace_tokens.whitespace_split = True
    brace_tokens.commenters = ""
    for token in brace_tokens:
        if token.startswith(("'", '"', "--exclude-dir=")):
            continue
        if "{" in token or "}" in token:
            raise ValueError("command blocked: brace-expanded paths cannot be confined")
    for segment in _segments(command_text):
        for word in segment[1:]:
            if word.startswith("--exclude-dir="):
                continue
            if word == "<":
                continue
            if word.startswith("-"):
                if "=" not in word:
                    continue
                word = word.split("=", 1)[1]
            candidate = root / word
            candidates = [Path(match) for match in glob.glob(str(candidate))] if _GLOB_CHARS.search(word) else [candidate]
            for candidate in candidates:
                if candidate.exists() or candidate.is_symlink():
                    resolved = candidate.resolve()
                    if resolved != root.resolve() and root.resolve() not in resolved.parents:
                        raise ValueError("command blocked: relative path escapes workspace")
        tool = segment[0]
        follow_flags = {"rg": "L", "grep": "R", "egrep": "R", "fgrep": "R", "find": "L", "tree": "l", "du": "L"}.get(tool)
        if follow_flags and any(
            arg == "--follow" or arg == "--dereference" or arg == "--dereference-recursive"
            or (arg.startswith("-") and not arg.startswith("--") and follow_flags in arg[1:])
            for arg in segment[1:]
        ):
            raise ValueError("command blocked: recursive symlink following cannot be confined")
        if "=" in segment[0] and not segment[0].startswith("="):
            raise ValueError("command blocked: variable assignment is not allowed")
        if segment[0] in {"for", "while", "until", "if", "case", "function", "{", "eval", "exec", "source", "."}:
            raise ValueError(f"command blocked: shell construct {segment[0]!r} is not allowed; use one pipeline")
        _check_tool([word for word in segment if word != "<"])
    assert_command_confined(root, command_text)
    return command_text


def _probe(argv: list[str], env: dict[str, str]) -> bool:
    try:
        return subprocess.run(argv, env=env, capture_output=True, timeout=10, check=False).returncode == 0
    except (OSError, subprocess.TimeoutExpired):
        return False


def _macos_profile(workspace: Path, scratch: Path) -> str:
    def quote(path: Path) -> str:
        return '"' + str(path).replace("\\", "\\\\").replace('"', '\\"') + '"'

    return f"""(version 1)
(allow default)
(deny network*)
(deny file-write*)
(allow file-write* (literal "/dev/null") (literal "/dev/tty") (literal "/dev/dtracehelper") (subpath {quote(scratch)}))
(deny file-read-data (subpath "/Users") (subpath "/Volumes") (subpath "/private/var/root"))
(allow file-read-data (subpath {quote(workspace)}) (subpath {quote(scratch)}))
"""


@dataclass
class ReadOnlyShell:
    """A reusable read-only shell rooted at `workspace`. Use as a context manager."""

    workspace: Path
    kind: str = "restricted-bash"
    _scratch: tempfile.TemporaryDirectory[str] | None = field(default=None, repr=False)
    _prefix: list[str] = field(default_factory=list, repr=False)
    _env: dict[str, str] = field(default_factory=dict, repr=False)
    _bash: str = "/bin/bash"

    def __enter__(self) -> "ReadOnlyShell":
        self.workspace = self.workspace.resolve()
        self._scratch = tempfile.TemporaryDirectory(prefix="freellama-shell-")
        scratch = Path(self._scratch.name)
        bin_dir = scratch / "bin"
        bin_dir.mkdir()
        # Resolve tools against the caller's PATH once, before the environment is scrubbed.
        for tool in READ_ONLY_TOOLS:
            located = shutil.which(tool)
            if located:
                (bin_dir / tool).symlink_to(os.path.realpath(located))
        self._bash = shutil.which("bash") or "/bin/bash"
        self._env = {
            key: value
            for key, value in os.environ.items()
            if key in {"LANG", "LC_ALL", "LC_CTYPE", "TZ"}
        }
        self._env.update({
            "PATH": str(bin_dir),
            "HOME": str(self.workspace),
            "TMPDIR": str(scratch),
            "TERM": "dumb",
            "PAGER": "cat",
            "GIT_PAGER": "cat",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_OPTIONAL_LOCKS": "0",  # `git status` otherwise rewrites the index
            "GIT_TERMINAL_PROMPT": "0",
        })
        self._prefix = self._os_sandbox(scratch, bin_dir)
        return self

    def __exit__(self, *_exc: object) -> None:
        if self._scratch is not None:
            self._scratch.cleanup()
            self._scratch = None

    def _os_sandbox(self, scratch: Path, bin_dir: Path) -> list[str]:
        if os.environ.get("FREELLAMA_AGENT_OS_SANDBOX", "auto") == "off":
            return []
        sandbox_exec = shutil.which("sandbox-exec")
        if sandbox_exec:
            profile = scratch / "profile.sb"
            profile.write_text(_macos_profile(self.workspace, scratch), encoding="utf-8")
            prefix = [sandbox_exec, "-f", str(profile)]
            if _probe([*prefix, self._bash, "-c", "true"], self._env):
                self.kind = "sandbox-exec+restricted-bash"
                return prefix
        bwrap = shutil.which("bwrap")
        if bwrap:
            prefix = [bwrap, "--unshare-all", "--die-with-parent", "--new-session",
                      "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]
            for system_dir in ("/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc/alternatives",
                               "/etc/ld.so.cache", "/etc/localtime", "/nix/store"):
                prefix += ["--ro-bind-try", system_dir, system_dir]
            prefix += ["--ro-bind", str(self.workspace), str(self.workspace),
                       "--ro-bind", str(bin_dir), str(bin_dir),
                       "--bind", str(scratch), str(scratch),
                       "--chdir", str(self.workspace), "--"]
            if _probe([*prefix, self._bash, "-c", "true"], self._env):
                self.kind = "bwrap+restricted-bash"
                return prefix
        return []

    def run(self, command_text: str, timeout_seconds: float) -> str:
        command = validate_command(self.workspace, command_text)
        result = subprocess.run(
            [*self._prefix, self._bash, "--restricted", "--noprofile", "--norc", "-c", command],
            cwd=self.workspace,
            env=self._env,
            text=True,
            capture_output=True,
            timeout=timeout_seconds,
            check=False,
            stdin=subprocess.DEVNULL,
        )
        output = (result.stdout or "") + (result.stderr or "")
        if not output.strip():
            output = f"(no output, exit code {result.returncode})"
        return output
