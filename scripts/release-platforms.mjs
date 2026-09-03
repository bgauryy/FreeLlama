// The one release matrix for both the CLI executable and the MCP N-API addon.
// A platform package contains exactly these two Rust artifacts; the two public
// JavaScript packages select it through npm optionalDependencies at install time.
export const PLATFORM_PACKAGES = [
  { id: "darwin-arm64", os: "darwin", cpu: "arm64", rustTarget: "aarch64-apple-darwin" },
  { id: "darwin-x64", os: "darwin", cpu: "x64", rustTarget: "x86_64-apple-darwin" },
  { id: "linux-arm64-gnu", os: "linux", cpu: "arm64", libc: "glibc", rustTarget: "aarch64-unknown-linux-gnu" },
  { id: "linux-arm64-musl", os: "linux", cpu: "arm64", libc: "musl", rustTarget: "aarch64-unknown-linux-musl" },
  { id: "linux-x64-gnu", os: "linux", cpu: "x64", libc: "glibc", rustTarget: "x86_64-unknown-linux-gnu" },
  { id: "linux-x64-musl", os: "linux", cpu: "x64", libc: "musl", rustTarget: "x86_64-unknown-linux-musl" },
  { id: "win32-arm64-msvc", os: "win32", cpu: "arm64", rustTarget: "aarch64-pc-windows-msvc" },
  { id: "win32-x64-msvc", os: "win32", cpu: "x64", rustTarget: "x86_64-pc-windows-msvc" },
];

export const nativePackageName = (id) => `@octocodeai/freellama-native-${id}`;
export const addonName = (id) => `freellama.${id}.node`;
export const executableName = (id) => (id.startsWith("win32-") ? "freellama.exe" : "freellama");
