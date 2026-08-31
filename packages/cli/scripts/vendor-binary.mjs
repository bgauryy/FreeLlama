// Copy the release binary into vendor/<platform>-<arch>/ so `npm pack` ships something runnable.
// Runs at prepack, not postinstall: consumers must not need a Rust toolchain.
import { copyFileSync, existsSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const exe = process.platform === "win32" ? "freellama.exe" : "freellama";
const src = path.join(root, "..", "..", "target", "release", exe);
if (!existsSync(src)) {
  console.error(`vendor-binary: ${src} missing — run \`cargo build --release\` first`);
  process.exit(1);
}
const dir = path.join(root, "vendor", `${process.platform}-${process.arch}`);
mkdirSync(dir, { recursive: true });
copyFileSync(src, path.join(dir, exe));
console.log(`vendor-binary: ${process.platform}-${process.arch} -> vendor/`);
