// runDelegate: execFile-compatible capture and errors, but timeout/abort/overflow kill the whole
// process group so adapter grandchildren (npx octocode, sandbox shells) are not orphaned.
import { describe, expect, it } from "vitest";
import { runDelegate } from "../../src/lifecycle.js";

const alive = (pid: number) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};
const base = { env: process.env, timeoutMs: 10_000, maxBuffer: 1024 * 1024 };

describe.skipIf(process.platform === "win32")("runDelegate", () => {
  it("resolves stdout and stderr on a clean exit", async () => {
    await expect(runDelegate("sh", ["-c", "echo out; echo err >&2"], base).done)
      .resolves.toEqual({ stdout: "out\n", stderr: "err\n" });
  });

  it("rejects a non-zero exit with execFile's Command failed message and stderr", async () => {
    await expect(runDelegate("sh", ["-c", "echo boom >&2; exit 3"], base).done)
      .rejects.toMatchObject({ message: "Command failed: sh -c echo boom >&2; exit 3\nboom\n", code: 3, killed: false });
  });

  it("kills the grandchild with the child on timeout", async () => {
    const run = runDelegate("sh", ["-c", "sleep 30 & echo $!; wait"], { ...base, timeoutMs: 500 });
    const error = await run.done.catch((caught) => caught);
    expect(error).toMatchObject({ killed: true, signal: "SIGKILL" });
    const grandchild = Number(String(error.stdout).trim());
    expect(grandchild).toBeGreaterThan(0);
    await new Promise((resolve) => setTimeout(resolve, 100));
    expect(alive(grandchild)).toBe(false);
  });

  it("kills the group and rejects with AbortError on abort", async () => {
    const controller = new AbortController();
    const run = runDelegate("sh", ["-c", "sleep 30 & wait"], { ...base, signal: controller.signal });
    setTimeout(() => controller.abort(), 200);
    await expect(run.done).rejects.toMatchObject({ name: "AbortError" });
  });

  it("rejects with the maxBuffer RangeError when output overflows", async () => {
    const run = runDelegate("sh", ["-c", "head -c 4096 /dev/zero; sleep 30"], { ...base, maxBuffer: 1024 });
    await expect(run.done).rejects.toMatchObject({ code: "ERR_CHILD_PROCESS_STDIO_MAXBUFFER" });
  });
});
