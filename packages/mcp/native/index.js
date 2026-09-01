// Hand-written glue: napi-rs CLI did not emit JS bindings for this crate/cli pair, so this
// file re-exports the compiled `.node`. Rebuild with `npm run build` from the repo root.
// See packages/mcp/README.md.
const { existsSync } = require("node:fs");
const path = require("node:path");

// napi-rs filenames: `freellama.<platform>-<arch>[-<abi>].node`. ABI suffix is present on
// Linux/Windows, absent on macOS. On Linux try both gnu and musl — Node's glibc probe does not
// have to match how the addon was compiled.
const ABI_SUFFIXES = {
  linux: process.report?.getReport()?.header?.glibcVersionRuntime ? ["-gnu", "-musl"] : ["-musl", "-gnu"],
  win32: ["-msvc"],
};

function candidateNames() {
  const { platform, arch } = process;
  const suffixes = ABI_SUFFIXES[platform] ?? [];
  // Bare name last: on a triple with no ABI component (macOS) it is the only form napi-rs emits.
  return [...new Set(suffixes.concat("").map((s) => `freellama.${platform}-${arch}${s}.node`))];
}

const candidates = candidateNames();
const found = candidates.find((name) => existsSync(path.join(__dirname, name)));

if (!found) {
  throw new Error(
    `freellama: no native addon for ${process.platform}-${process.arch}.\n` +
      `Looked for: ${candidates.join(", ")} in ${__dirname}\n` +
      "The addon is a compiled artifact and is not checked into git. Build it from the repo " +
      "root with:\n" +
      "  npm install && npm run build\n" +
      "(which runs `napi build --platform --release --features napi -o packages/mcp/native`). " +
      "This requires a Rust toolchain. A package release can contain " +
      "fewer prebuilt targets than the source supports — see packages/mcp/README.md.",
  );
}

module.exports = require(`./${found}`);
