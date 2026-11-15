// The Windows installer against local release files; its HTTP cmdlet is replaced before loading
// the script, so neither the product nor Ollama can be downloaded or installed from the network.
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const installer = fileURLToPath(new URL("../../../scripts/install.ps1", import.meta.url));

describe.runIf(process.platform === "win32")("scripts/install.ps1 (Windows PowerShell)", () => {
  it.each([true, false])("checks the exact binary checksum before installation (matching=%s)", (matching) => {
    const directory = mkdtempSync(join(tmpdir(), "freellama-powershell-install-"));
    try {
      const release = join(directory, "release");
      const destination = join(directory, "installed");
      const calls = join(directory, "requests.txt");
      mkdirSync(release);
      const bytes = Buffer.from("fixture executable; never launched");
      const digest = matching ? createHash("sha256").update(bytes).digest("hex") : "0".repeat(64);
      const assets = ["win32-x64-msvc", "win32-arm64-msvc"].map((target) => `freellama-${target}.exe`);
      for (const asset of assets) writeFileSync(join(release, asset), bytes);
      writeFileSync(join(release, "SHA256SUMS"), assets.map((asset) => `${digest}  ${asset}`).join("\n"));
      const wrapper = join(directory, "fixture.ps1");
      writeFileSync(wrapper, `
$ErrorActionPreference = 'Stop'
function Invoke-WebRequest {
  [CmdletBinding()] param([string]$Uri, [string]$OutFile)
  $asset = [System.IO.Path]::GetFileName(([uri]$Uri).AbsolutePath)
  Add-Content -Path $env:FREELLAMA_TEST_REQUESTS -Value $asset
  Copy-Item -LiteralPath (Join-Path $env:FREELLAMA_TEST_RELEASE $asset) -Destination $OutFile
}
function Get-Command {
  [CmdletBinding()] param([string]$Name)
  if ($Name -eq 'ollama') { return $null }
  throw "Unexpected external command lookup: $Name"
}
. $env:FREELLAMA_TEST_INSTALLER -Version v0.0.0 -Method binary -BinDir $env:FREELLAMA_TEST_DESTINATION -NoOllama
`);
      const result = spawnSync("powershell.exe", ["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", wrapper], {
        encoding: "utf8", timeout: 20_000,
        env: {
          ...process.env, LOCALAPPDATA: join(directory, "appdata"), FREELLAMA_RELEASE_BASE: "https://release.invalid/fixture",
          FREELLAMA_TEST_RELEASE: release, FREELLAMA_TEST_REQUESTS: calls,
          FREELLAMA_TEST_INSTALLER: installer, FREELLAMA_TEST_DESTINATION: destination,
        },
      });
      expect(result.error).toBeUndefined();
      expect(readFileSync(calls, "utf8").trim().split(/\r?\n/)).toEqual([expect.stringMatching(/^freellama-win32-(x64|arm64)-msvc\.exe$/), "SHA256SUMS"]);
      const executable = join(destination, "freellama.exe");
      if (matching) {
        expect(result.status, result.stderr).toBe(0);
        expect(readFileSync(executable)).toEqual(bytes);
        expect(result.stdout).toContain("Ollama is not installed");
      } else {
        expect(result.status).not.toBe(0);
        expect(result.stderr).toContain("Checksum mismatch");
        expect(existsSync(executable)).toBe(false);
      }
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  });
});
