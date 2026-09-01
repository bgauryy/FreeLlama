// Copy the research adapters into the package so a published install has them.
// Source of truth stays benchmark/local/scripts/ — this is a build-time copy, gitignored.
import { copyFileSync, existsSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
const pkg = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
// Two levels up: this package lives at packages/mcp, benchmark/ is at the repo root.
const src = path.join(pkg, "..", "..", "benchmark", "local", "scripts");
const dst = path.join(pkg, "adapters");
mkdirSync(dst, { recursive: true });
let n = 0;
// agent_context.py is imported by both adapters at runtime — omitting it turns a packed
// install into an ImportError on the first delegate_research call.
for (const f of ["agent_context.py", "agent_transport.py", "bash_agent.py", "octocode_agent.py"]) {
  const from = path.join(src, f);
  if (!existsSync(from)) {
    console.error(`bundle-adapters: missing ${from} — delegate_research will not work when packed`);
    process.exit(1);
  }
  copyFileSync(from, path.join(dst, f)); n += 1;
}
console.log(`bundle-adapters: copied ${n} adapters into ${path.relative(pkg, dst)}/`);
