// Hand-written glue: `napi build`'s auto-generated JS/TS binding step didn't emit output with the
// installed @napi-rs/cli version (3.8.6) against this project's napi-rs crate version (2.16) —
// the compiled native module itself is fully correct and verified working (see
// `mcp-server/README.md`), so this file just re-exports it directly rather than depending on a
// glue-generation step that isn't producing output. Regenerate the binary with `npm run build`
// from the repo root; this file only needs changing if the exported function names change.
//
// Platform resolution: `napi build --platform` names its output
// `freellama.<platform>-<arch>[-<abi>].node`, so a single hardcoded filename only ever works on
// the one machine that built it. This loader derives the candidate names the same way napi-rs
// does and reports precisely what it looked for when none are present — an unsupported platform
// has to say so, not surface as a bare MODULE_NOT_FOUND from a require() deep inside the server.
const { existsSync } = require("node:fs");
const path = require("node:path");

// Matches napi-rs's own target triple -> filename mapping for the platforms this crate can build
// (see the `napi.targets` field in the repo-root package.json). The `-gnu`/`-msvc` ABI suffixes
// are part of the filename on Linux/Windows but absent on macOS, which is why this is a lookup
// rather than a plain `${platform}-${arch}` template.
// Matches napi-rs's own target triple -> filename mapping for the platforms this crate can build
// (see the `napi.targets` field in the repo-root package.json). The ABI suffix is part of the
// filename on Linux/Windows but absent on macOS, which is why this is a candidate list rather
// than a plain `${platform}-${arch}` template. On Linux both libc flavours are tried regardless
// of what `glibcVersionRuntime` reports — that probe describes the *Node* build, which does not
// have to match how the addon was compiled, so preferring one flavour is fine but excluding the
// other is not.
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
      "(which runs `napi build --release --no-default-features --features napi -o " +
      "mcp-server/native`). This requires a Rust toolchain. Prebuilt binaries are currently " +
      "published for macOS only — see mcp-server/README.md.",
  );
}

module.exports = require(`./${found}`);
