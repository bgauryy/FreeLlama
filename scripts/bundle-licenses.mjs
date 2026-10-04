import { copyFileSync, mkdirSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { PLATFORM_PACKAGES } from './release-platforms.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const hero = path.join(root, 'assets/logo.jpg');
const nativeReadme = (platform) => {
  const os = { darwin: 'macOS', linux: 'Linux', win32: 'Windows' }[platform.os];
  const abi = platform.libc ?? (platform.os === 'win32' ? 'MSVC' : undefined);
  const label = `${os} ${platform.cpu}${abi ? ` (${abi})` : ''}`;
  return `# FreeLlama native package for ${label}

![A brown cartoon llama stands on a pastel cloud background.](assets/logo.jpg)

This package supplies the FreeLlama executable and N-API addon for ${label}. The CLI and MCP
packages select the matching artifact through optional dependencies; install those entry packages
with optional dependencies enabled.

FreeLlama gives agents a managed way to offload tasks to local Ollama models. The native core
handles model routing, admission, memory checks, and execution evidence; Ollama runs inference.

Use the [CLI package](https://www.npmjs.com/package/@octocodeai/freellama) for terminal commands or
the [MCP server](https://www.npmjs.com/package/@octocodeai/freellama-mcp-server) for agent integration.
See the [project guide](https://github.com/bgauryy/FreeLlama#readme) for setup and resource controls.
`;
};
for (const directory of ['packages/cli', 'packages/mcp', ...PLATFORM_PACKAGES.map(({ id }) => `packages/native/${id}`)]) {
  const destination = path.join(root, directory);
  mkdirSync(path.join(destination, 'assets'), { recursive: true });
  for (const file of ['LICENSE-APACHE', 'LICENSE-MIT']) copyFileSync(path.join(root, file), path.join(destination, file));
  copyFileSync(hero, path.join(destination, 'assets/logo.jpg'));
}
for (const platform of PLATFORM_PACKAGES) {
  writeFileSync(path.join(root, 'packages/native', platform.id, 'README.md'), nativeReadme(platform));
}
