// Execute the standalone health helper against offline command fixtures.
//
// Hermetic: check.sh sees only fake curl/ollama/df/launchctl/sysctl, a minimal environment, no
// stdin, and runs in its own process group so a timeout kills every descendant instead of blocking
// on a pipe that an orphaned grandchild still holds. Run: node --test skills/freellama/scripts/test-check.mjs
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const CHECK = path.join(path.dirname(fileURLToPath(import.meta.url)), "check.sh");
const TIMEOUT_SECONDS = 30;
const CONTRACTS = {
  hardware_fit: "sent_num_ctx",
  machine_profile: "portable_host_memory_v2",
  model_backends: "explicit_cpu_assignment",
  placement_preference: "guarded_hint",
  placement_observation: "ollama_api_ps_after_execution",
  placement_evidence_gate: "configured_or_observed",
  placement_feedback: "three_sample_runtime",
  placement_feedback_metric: "normalized_work_unit_10_percent",
  placement_feedback_persistence: "versioned_atomic_snapshot_v1",
  authentication: "optional_bearer_all_routes",
  immediate_unload_observation: "observe_then_unload",
};

function currentHealth() {
  return {
    contracts: { ...CONTRACTS },
    backends: { gpu: { upstream: "http://fixture-ollama.invalid", admission: { slots_total: 2 } }, cpu: null },
    security: { authentication: "none", remote_access: false },
    feedback: { persistence: { enabled: true, last_error: null } },
  };
}

// Plain sh fakes: no interpreter start-up per call, and no path can reach a real network.
const PROGRAMS = {
  curl: "#!/bin/sh\nfor url; do :; done\ncase \"$url\" in\n  */_freellama/v1/health) cat \"$HEALTH_FIXTURE\" ;;\n  */api/version) echo '{\"version\": \"0.0.1\"}' ;;\n  */api/ps) echo '{\"models\": []}' ;;\n  *) exit 22 ;;\nesac\n",
  ollama: "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'ollama version is 0.0.1'; else echo 'NAME ID SIZE MODIFIED'; printf '%s\\n' \"$HEALTH_MODEL_ROWS\"; fi\n",
  df: "#!/bin/sh\necho 'Filesystem 1024-blocks Used Available Capacity Mounted'\necho 'fixture 100000000 10000000 90000000 10% /'\n",
  launchctl: "#!/bin/sh\nexit 0\n",
  sysctl: "#!/bin/sh\necho 17179869184\n",
};

async function runCheck(health, modelRows = "") {
  const root = mkdtempSync(path.join(os.tmpdir(), "freellama-check-"));
  try {
    const tools = path.join(root, "tools");
    mkdirSync(tools);
    const payload = path.join(root, "health.json");
    writeFileSync(payload, JSON.stringify(health));
    for (const [name, program] of Object.entries(PROGRAMS)) {
      const executable = path.join(tools, name);
      writeFileSync(executable, program);
      chmodSync(executable, 0o755);
    }
    symlinkSync(process.execPath, path.join(tools, "node"));
    // Built from scratch rather than inherited: BASH_ENV, NODE_OPTIONS, proxy, or live
    // FREELLAMA_*/OLLAMA_* variables from the caller's shell must not reach check.sh.
    const env = {
      PATH: [tools, "/usr/bin", "/bin"].join(path.delimiter),
      HOME: root, TMPDIR: root, LANG: "C", LC_ALL: "C",
      HEALTH_FIXTURE: payload,
      HEALTH_MODEL_ROWS: modelRows,
      FREELLAMA_CHECK_ROOT: root, FREELLAMA_REPO: "", FREELLAMA_AUTH_TOKEN_FILE: "",
      OLLAMA_ENDPOINT: "http://fixture-ollama.invalid", FREELLAMA_ENDPOINT: "http://fixture-gateway.invalid",
      OLLAMA_HOST: "127.0.0.1:11434", OLLAMA_NO_CLOUD: "1", OLLAMA_KV_CACHE_TYPE: "q8_0",
      OLLAMA_NUM_PARALLEL: "1", OLLAMA_MAX_LOADED_MODELS: "1", OLLAMA_MAX_QUEUE: "512", OLLAMA_FLASH_ATTENTION: "1",
      MIN_FREE_GB: "15",
    };
    const child = spawn("/bin/bash", ["--noprofile", "--norc", CHECK], {
      env, cwd: root, stdio: ["ignore", "pipe", "pipe"], detached: true,
    });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8").on("data", (chunk) => { stdout += chunk; });
    child.stderr.setEncoding("utf8").on("data", (chunk) => { stderr += chunk; });
    let timedOut = false;
    const guard = setTimeout(() => {
      timedOut = true;
      try {
        process.kill(-child.pid, "SIGKILL");
      } catch {
        // The group already exited.
      }
    }, TIMEOUT_SECONDS * 1000);
    const returncode = await new Promise((resolve, reject) => {
      child.on("error", reject);
      child.on("close", (code, signal) => resolve(code ?? (signal ? -1 : 0)));
    });
    clearTimeout(guard);
    if (timedOut) assert.fail(`check.sh exceeded ${TIMEOUT_SECONDS}s\n${stdout}${stderr}`);
    return { returncode, stdout, stderr, output: stdout + stderr };
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

test("current loopback health passes", async () => {
  const result = await runCheck(currentHealth());
  assert.equal(result.returncode, 0, result.output);
  assert.ok(result.stdout.includes("serve contracts are current"));
});

test("ephemeral feedback warns without claiming stale contracts", async () => {
  const health = currentHealth();
  health.feedback.persistence.enabled = false;
  const result = await runCheck(health);
  assert.equal(result.returncode, 0, result.output);
  assert.ok(result.stdout.includes("WARN"));
  assert.ok(result.stdout.includes("ephemeral"));
  assert.ok(!result.stdout.includes("stale or incomplete"));
});

test("resource hold warns without misreporting service health", async () => {
  const health = currentHealth();
  health.admission = { resources: { holding: true, reasons: ["active_swapping"] } };
  const result = await runCheck(health);
  assert.equal(result.returncode, 0, result.output);
  assert.ok(result.stdout.includes("managed task execution is held: active_swapping"));
  assert.ok(!result.stdout.includes("nothing blocking"));
  assert.ok(!result.stdout.includes("stale or incomplete"));
});

test("authenticated remote access warns about ingress", async () => {
  const health = currentHealth();
  health.security = { remote_access: true, authentication: "bearer" };
  const result = await runCheck(health);
  assert.equal(result.returncode, 0, result.output);
  assert.ok(result.stdout.includes("WARN"));
  assert.ok(result.stdout.includes("TLS"));
  assert.ok(!result.stdout.includes("stale or incomplete"));
});

test("unauthenticated remote access fails as security error", async () => {
  const health = currentHealth();
  health.security.remote_access = true;
  const result = await runCheck(health);
  assert.equal(result.returncode, 1, result.output);
  assert.ok(result.stdout.includes("remote access"));
  assert.ok(result.stdout.includes("unauthenticated"));
  assert.ok(!result.stdout.includes("stale or incomplete"));
});

test("each required contract remains a hard failure", async (t) => {
  for (const name of Object.keys(CONTRACTS)) {
    await t.test(name, async () => {
      const health = currentHealth();
      delete health.contracts[name];
      const result = await runCheck(health);
      assert.equal(result.returncode, 1, result.output);
      assert.ok(result.stdout.includes("stale or incomplete"));
    });
  }
});

test("missing backend ownership or admission still fails", async (t) => {
  for (const field of ["upstream", "admission"]) {
    await t.test(field, async () => {
      const health = currentHealth();
      delete health.backends.gpu[field];
      const result = await runCheck(health);
      assert.equal(result.returncode, 1, result.output);
      assert.ok(result.stdout.includes("stale or incomplete"));
    });
  }
});

test("model metadata age distinguishes two and three months", async () => {
  const rows = "recent:tag id2 1GB 2 months ago\nolder:tag id3 1GB 3 months ago\nyearly:tag id4 1GB 1 year ago";
  const result = await runCheck(currentHealth(), rows);
  assert.equal(result.returncode, 0, result.output);
  assert.ok(!result.stdout.includes("old model metadata: recent:tag"));
  assert.ok(result.stdout.includes("old model metadata: older:tag"));
  assert.ok(result.stdout.includes("old model metadata: yearly:tag"));
  assert.ok(result.stdout.includes("2 model(s) have metadata timestamps at least 3 months old"));
  assert.ok(result.stdout.includes("Metadata age does not establish when a model was last used"));
  assert.ok(!result.stdout.includes("ollama rm"));
});
