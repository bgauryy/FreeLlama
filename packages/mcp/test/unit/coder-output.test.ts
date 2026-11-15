import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, readFileSync, statSync, writeSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { tmpdir } from "node:os";
import { afterEach, describe, expect, it } from "vitest";
import { CommandRunner, describeRun } from "../../src/research/coder-actions.js";
import { CommandOutput } from "../../src/research/coder-output.js";

const roots: string[] = [];
const temp = () => { const dir = mkdtempSync(path.join(tmpdir(), "freellama-command-audit-")); roots.push(dir); return dir; };
const quote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`;
const node = (script: string) => `${quote(process.execPath)} -e ${quote(script)}`;
function runner(extra: Partial<ConstructorParameters<typeof CommandRunner>[3]> = {}) {
  const work = temp();
  return new CommandRunner(work, [work], { PATH: process.env.PATH }, {
    privateDir: work, osSandbox: "off", allowUnsandboxed: true, logDir: temp(), ...extra,
  }).enter();
}
afterEach(() => { for (const dir of roots.splice(0)) rmSync(dir, { recursive: true, force: true }); });

describe("lossless bounded command audit", () => {
  it("retains raw bytes including the middle omitted from model and inline previews", async () => {
    const text = `head\n${"x".repeat(2 * 1024 * 1024)}\nMIDDLE_SENTINEL\n${"y".repeat(2 * 1024 * 1024)}\ntail\n`;
    const outcome = await runner().run(node('process.stdout.write("head\\n" + "x".repeat(2*1024*1024) + "\\nMIDDLE_SENTINEL\\n" + "y".repeat(2*1024*1024) + "\\ntail\\n")'), 10);
    const raw = readFileSync(outcome.audit.path);
    expect(raw.equals(Buffer.from(text))).toBe(true);
    expect(outcome.observed).not.toContain("MIDDLE_SENTINEL");
    expect(outcome.kept).not.toContain("MIDDLE_SENTINEL");
    expect(outcome.audit).toMatchObject({ bytes: raw.length, complete: true, end_reason: "complete", sha256: createHash("sha256").update(raw).digest("hex") });
    expect(outcome.failed).toBe(false);
    expect(statSync(outcome.audit.path).mode & 0o777).toBe(0o600);
    expect(statSync(path.dirname(outcome.audit.path)).mode & 0o777).toBe(0o700);
    expect(describeRun(outcome)).toContain("preview only");
  });

  it("terminates a producer at the exact disk cap and reports failure instead of dropping middle bytes", async () => {
    const outcome = await runner({ maxOutputBytes: 1024 }).run(node('setInterval(() => process.stdout.write("x".repeat(65536)), 1)'), 10);
    expect(outcome.audit).toMatchObject({ bytes: 1024, complete: false, end_reason: "output_limit" });
    expect(readFileSync(outcome.audit.path).equals(Buffer.alloc(1024, "x"))).toBe(true);
    expect(outcome.failed).toBe(true);
    expect(describeRun(outcome)).toMatch(/INCOMPLETE: output_limit; command failed/);
    expect(outcome.durationMs).toBeLessThan(5000);
  });

  it("handles short writes and terminates on a disk failure while preserving the written prefix", async () => {
    let writes = 0;
    const outcome = await runner({ openOutput: (file, limit) => new CommandOutput(file, limit, (fd, data, offset) => {
      if (++writes > 1) throw new Error("simulated disk full");
      return writeSync(fd, data, offset, 2);
    }) }).run(node('setInterval(() => process.stdout.write("abcdef"), 1)'), 10);
    expect(outcome.audit).toMatchObject({ bytes: 2, complete: false, end_reason: "write_error", error: "simulated disk full" });
    expect(readFileSync(outcome.audit.path, "utf8")).toBe("ab");
    expect(outcome.failed).toBe(true);
    expect(outcome.observed).toBe("ab");
  });

  it("writes an incomplete-start record before execution and finalizes it with the audit", () => {
    const output = new CommandOutput(path.join(temp(), "raw.log"), 128, writeSync, "exact command");
    expect(JSON.parse(readFileSync(`${output.path}.json`, "utf8"))).toMatchObject({ command: "exact command", state: "incomplete" });
    output.append(Buffer.from("evidence"));
    const audit = output.finish("complete", undefined, { exit_code: 0 });
    expect(JSON.parse(readFileSync(audit.metadata_path, "utf8"))).toMatchObject({ command: "exact command", state: "complete", output: { bytes: 8 }, execution: { exit_code: 0 } });
  });

  it("retains complete output on nonzero exit and marks timeout output incomplete", async () => {
    const failed = await runner().run(node('process.stdout.write("out\\n"); process.stderr.write("err\\n"); process.exitCode=7'), 10);
    expect(failed.exitCode).toBe(7);
    expect(failed.failed).toBe(true);
    expect(failed.audit.complete).toBe(true);
    const raw = readFileSync(failed.audit.path, "utf8");
    expect(raw).toContain("out\n"); expect(raw).toContain("err\n");
    const timed = await runner().run("echo started; sleep 30", 0.1);
    expect(timed.audit).toMatchObject({ complete: false, end_reason: "timeout" });
    expect(readFileSync(timed.audit.path, "utf8")).toContain("started");
    expect(timed.failed).toBe(true);
  });

  it("refuses execution before starting a command when the audit cannot be opened", async () => {
    const root = temp(), obstruction = path.join(root, "file"); writeFileSync(obstruction, "not a directory");
    const commandRunner = runner({ logDir: obstruction });
    const marker = path.join(commandRunner.cwd, "executed");
    await expect(commandRunner.run("echo bad > executed", 10)).rejects.toThrow();
    expect(existsSync(marker)).toBe(false);
  });
});
