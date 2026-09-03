// Hand-written glue: napi-rs CLI did not emit JS bindings for this crate/cli pair, so this
// file re-exports the compiled `.node`. Rebuild with `npm run build` from the repo root.
// See packages/mcp/README.md.
const { existsSync } = require("node:fs");
const path = require("node:path");
const { createRequire } = require("node:module");
const requireFromHere = createRequire(__filename);

// napi-rs filenames: `freellama.<platform>-<arch>[-<abi>].node`. ABI suffix is present on
// Linux/Windows, absent on macOS. On Linux try both gnu and musl — Node's glibc probe does not
// have to match how the addon was compiled.
const ABI_SUFFIXES = {
  linux: process.report?.getReport()?.header?.glibcVersionRuntime ? ["-gnu", "-musl"] : ["-musl", "-gnu"],
  win32: ["-msvc"],
};

function candidateIds() {
  const { platform, arch } = process;
  const suffixes = ABI_SUFFIXES[platform] ?? [];
  return [...new Set(suffixes.concat("").map((suffix) => `${platform}-${arch}${suffix}`))];
}

const candidates = candidateIds();
const found = candidates.find((id) => existsSync(path.join(__dirname, `freellama.${id}.node`)));

if (!found) {
  const failures = [];
  for (const id of candidates) {
    const packageName = `@octocodeai/freellama-native-${id}`;
    try {
      module.exports = requireFromHere(packageName);
      return;
    } catch (error) {
      failures.push(`${packageName}: ${error?.message ?? error}`);
    }
  }
  throw new Error(
    `freellama: no usable native addon for ${process.platform}-${process.arch}.\n` +
      `Tried local development artifacts and optional packages: ${candidates.map((id) => `@octocodeai/freellama-native-${id}`).join(", ")}.\n` +
      "Reinstall without --omit=optional. From a source checkout, run `yarn build:native`.\n" +
      `Load failures:\n  ${failures.join("\n  ")}`,
  );
}

module.exports = require(`./freellama.${found}.node`);
