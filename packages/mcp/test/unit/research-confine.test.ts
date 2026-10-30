// Regression: the default research adapter must not read or write outside the workspace root.
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { assertCommandConfined } from "../../src/research/bash-agent.js";
import { ValidationError } from "../../src/research/errors.js";
import { ReadOnlyShell, validateCommand } from "../../src/research/sandbox.js";

let root = "";
beforeEach(() => {
  root = mkdtempSync(path.join(tmpdir(), "freellama-confine-test-"));
  writeFileSync(path.join(root, "readme.md"), "ok\n");
});
afterEach(() => rmSync(root, { recursive: true, force: true }));

function valueError(run: () => unknown): string {
  try {
    run();
  } catch (error) {
    expect(error).toBeInstanceOf(ValidationError);
    return (error as Error).message;
  }
  throw new Error("expected a ValidationError");
}

async function asyncValueError(run: () => Promise<unknown>): Promise<string> {
  try {
    await run();
  } catch (error) {
    expect(error).toBeInstanceOf(ValidationError);
    return (error as Error).message;
  }
  throw new Error("expected a ValidationError");
}

async function withShell<T>(use: (shell: ReadOnlyShell) => Promise<T>, osSandbox?: string): Promise<T> {
  const previous = process.env.FREELLAMA_AGENT_OS_SANDBOX;
  if (osSandbox !== undefined) process.env.FREELLAMA_AGENT_OS_SANDBOX = osSandbox;
  try {
    const shell = new ReadOnlyShell(root).enter();
    try {
      return await use(shell);
    } finally {
      shell.close();
    }
  } finally {
    if (osSandbox !== undefined) {
      if (previous === undefined) delete process.env.FREELLAMA_AGENT_OS_SANDBOX;
      else process.env.FREELLAMA_AGENT_OS_SANDBOX = previous;
    }
  }
}

describe("confinement", () => {
  it("relative read is allowed", () => assertCommandConfined(root, "cat readme.md"));
  it("absolute /etc is blocked", () => expect(valueError(() => assertCommandConfined(root, "cat /etc/hosts"))).toContain("escapes workspace"));
  it("home is blocked", () => {
    valueError(() => assertCommandConfined(root, "ls $HOME"));
    valueError(() => assertCommandConfined(root, "cat ~/.ssh/id_rsa"));
  });
  it("dot-dot is blocked", () => {
    valueError(() => assertCommandConfined(root, "cat ../secret"));
    valueError(() => assertCommandConfined(root, "ls .."));
  });
});

// Every command here got past the old regex denylist in front of `bash -c`.
const BYPASSES = [
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
];

const ALLOWED = [
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
];

describe("read-only shell", () => {
  it.each(BYPASSES)("refuses known bypass %s", (command) => {
    valueError(() => validateCommand(root, command));
  });

  it("blocks every sed program and joined write flags without an OS sandbox", async () => {
    writeFileSync(path.join(root, "program.sed"), "w escaped.txt\n");
    const commands = [
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
    ];
    await withShell(async (shell) => {
      for (const command of commands) {
        expect(await asyncValueError(() => shell.run(command, 5))).toMatch(/sed|sort/);
        expect(existsSync(path.join(root, "escaped.txt"))).toBe(false);
      }
    }, "off");
    expect(readFileSync(path.join(root, "readme.md"), "utf8")).toBe("ok\n");
  });

  it("refuses dynamic and symlink paths without an OS sandbox", async () => {
    const outside = mkdtempSync(path.join(tmpdir(), "freellama-confine-outside-"));
    try {
      const external = path.join(outside, "secret");
      writeFileSync(external, "outside scratch sentinel\n");
      symlinkSync(external, path.join(root, "link"));
      const commands: Record<string, string> = {
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
      };
      await withShell(async (shell) => {
        for (const [command, reason] of Object.entries(commands)) {
          expect(await asyncValueError(() => shell.run(command, 5))).toContain(reason);
        }
      }, "off");
      expect(existsSync(path.join(root, "escaped.txt"))).toBe(false);
      expect(readFileSync(external, "utf8")).toBe("outside scratch sentinel\n");
    } finally {
      rmSync(outside, { recursive: true, force: true });
    }
    expect(readFileSync(path.join(root, "readme.md"), "utf8")).toBe("ok\n");
  });

  it.each(ALLOWED)("allows research command %s", (command) => {
    validateCommand(root, command);
  });

  it("runs commands and leaves the workspace untouched", async () => {
    await withShell(async (shell) => {
      expect(await shell.run("cat readme.md", 10)).toContain("ok");
      expect(await shell.run("grep -c ok readme.md 2>/dev/null", 10)).toContain("1");
      await asyncValueError(() => shell.run("echo gone > readme.md", 10));
    });
    expect(readFileSync(path.join(root, "readme.md"), "utf8")).toBe("ok\n");
  });

  it("scrubs the environment", async () => {
    process.env.FREELLAMA_TEST_SECRET = "s3cret";
    try {
      await withShell(async (shell) => {
        // `env` is not an allowed tool, and printf cannot expand variables past validation.
        await asyncValueError(() => shell.run("env", 10));
        expect(await shell.run("grep -r s3cret . ; echo done", 10)).not.toContain("s3cret");
        expect(shell.env).not.toHaveProperty("FREELLAMA_TEST_SECRET");
      });
    } finally {
      delete process.env.FREELLAMA_TEST_SECRET;
    }
  });

  it("restricted bash backs up validation", async () => {
    // Even with validation skipped, rbash refuses output redirection and slashed commands.
    await withShell(async (shell) => {
      const [program, ...args] = [...shell.prefix, shell.bash, "--restricted", "-c", "echo x > readme.md; /bin/rm readme.md"];
      const result = spawnSync(program!, args, { cwd: root, env: shell.env, encoding: "utf8" });
      expect(result.stderr).toContain("restricted");
    });
    expect(readFileSync(path.join(root, "readme.md"), "utf8")).toBe("ok\n");
  });
});
