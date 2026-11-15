// scripts/install.sh against stub curl, npm and ollama: nothing is downloaded or installed for real.
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";

const SCRIPT = new URL("../../../scripts/install.sh", import.meta.url).pathname;
const ASSETS = ["darwin-arm64", "darwin-x64", "linux-x64-gnu", "linux-arm64-gnu", "linux-x64-musl", "linux-arm64-musl"].map((p) => `freellama-${p}`);
const dirs: string[] = [];

function stub(dir: string, name: string, body: string) {
  writeFileSync(join(dir, name), `#!/bin/sh\n${body}\n`);
  chmodSync(join(dir, name), 0o755);
}

/** A sandbox whose `curl` serves files from `release/` (by basename) and logs every call. */
function sandbox(options: { npm?: boolean; ollama?: boolean; binary?: string; sums?: string } = {}) {
  const root = mkdtempSync(join(tmpdir(), "freellama-install-"));
  dirs.push(root);
  const bin = join(root, "bin"), release = join(root, "release");
  mkdirSync(bin);
  mkdirSync(release);
  const binary = options.binary ?? "#!/bin/sh\necho freellama\n";
  writeFileSync(join(release, "SHA256SUMS"), options.sums ?? ASSETS.map((a) => `${createHash("sha256").update(binary).digest("hex")}  ${a}`).join("\n") + "\n");
  for (const asset of ASSETS) writeFileSync(join(release, asset), binary);
  writeFileSync(join(release, "install.sh"), `touch "${join(root, "ollama-installed")}"\n`);
  stub(bin, "curl", [
    `echo "$*" >> "${join(root, "curl.log")}"`,
    `out=""; url=""; while [ $# -gt 0 ]; do case "$1" in -o) out="$2"; shift 2 ;; http*) url="$1"; shift ;; *) shift ;; esac; done`,
    `file="${release}/$(basename "$url")"; [ -f "$file" ] || exit 22`,
    `if [ -n "$out" ]; then cp "$file" "$out"; else cat "$file"; fi`,
  ].join("\n"));
  if (options.npm) stub(bin, "npm", `echo "$*" >> "${join(root, "npm.log")}"`);
  if (options.ollama) stub(bin, "ollama", "exit 0");
  const run = (...args: string[]) => new Promise<{ code: number | null; out: string }>((resolve, reject) => {
    // detached starts a new session with no controlling terminal, so the Ollama prompt cannot
    // open /dev/tty and wait for input.
    const child = spawn("sh", [SCRIPT, ...args], {
      detached: true,
      stdio: ["ignore", "pipe", "pipe"],
      env: {
        PATH: `${bin}:/usr/bin:/bin:/usr/sbin:/sbin`,
        HOME: root,
        FREELLAMA_RELEASE_BASE: "https://releases.example/download",
        FREELLAMA_OLLAMA_INSTALLER: "https://ollama.example/install.sh",
        FREELLAMA_OLLAMA_PATHS: "",
      },
    });
    let out = "";
    child.stdout.on("data", (chunk) => (out += chunk));
    child.stderr.on("data", (chunk) => (out += chunk));
    child.on("error", reject);
    child.on("close", (code) => resolve({ code, out }));
  });
  const read = (name: string) => (existsSync(join(root, name)) ? readFileSync(join(root, name), "utf8") : "");
  return { root, run, read, ollamaInstalled: () => existsSync(join(root, "ollama-installed")) };
}

afterEach(() => {
  for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

describe.skipIf(process.platform === "win32")("scripts/install.sh (POSIX shell)", () => {
  it("installs the npm package by default and only prints the Ollama command without a terminal", async () => {
    const box = sandbox({ npm: true });
    const { code, out } = await box.run();
    expect(code).toBe(0);
    expect(box.read("npm.log").trim()).toBe("install -g @octocodeai/freellama");
    expect(out).toContain("Ollama is not installed. Install it with: curl -fsSL https://ollama.example/install.sh | sh");
    expect(box.ollamaInstalled()).toBe(false);
    expect(out).toContain("freellama init");
  });

  it("runs Ollama's installer only with consent, and skips it when Ollama is present", async () => {
    const consent = sandbox({ npm: true });
    expect((await consent.run("--with-ollama")).code).toBe(0);
    expect(consent.ollamaInstalled()).toBe(true);

    const present = sandbox({ npm: true, ollama: true });
    const { out } = await present.run("--with-ollama");
    expect(out).toContain("Ollama is installed.");
    expect(present.read("curl.log")).toBe("");
  });

  it("keeps a pinned --version as the checksum-verified binary install", async () => {
    const box = sandbox({ npm: true });
    const binDir = join(box.root, "out");
    const { code, out } = await box.run("--version", "v1.2.3", "--bin-dir", binDir, "--no-ollama");
    expect(code, out).toBe(0);
    expect(box.read("npm.log")).toBe("");
    expect(readFileSync(join(binDir, "freellama"), "utf8")).toContain("echo freellama");
  });

  it("refuses a binary whose checksum does not match", async () => {
    const box = sandbox({ sums: ASSETS.map((a) => `${"0".repeat(64)}  ${a}`).join("\n") + "\n" });
    const binDir = join(box.root, "out");
    const { code, out } = await box.run("--method", "binary", "--bin-dir", binDir, "--no-ollama");
    expect(code).not.toBe(0);
    expect(out).toContain("Checksum mismatch");
    expect(existsSync(join(binDir, "freellama"))).toBe(false);
  });

  it("pins the npm version without the tag's v", async () => {
    const box = sandbox({ npm: true });
    expect((await box.run("--method", "npm", "--version", "v0.12.0", "--no-ollama")).code).toBe(0);
    expect(box.read("npm.log").trim()).toBe("install -g @octocodeai/freellama@0.12.0");
  });
});
