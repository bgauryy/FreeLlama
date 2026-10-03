import { copyFileSync, mkdirSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { PLATFORM_PACKAGES } from './release-platforms.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const hero = path.join(root, 'assets/logo.jpg');
const nativeReadme = `# FreeLlama native package

Meet the agentic tool:

![A cartoon llama with a full halo and small wings holds a glowing wrench beneath an open golden gate.](assets/logo.jpg)

This package publishes one platform executable and its N-API addon. The CLI and MCP packages select it by optional dependency.
`;
for (const directory of ['packages/cli', 'packages/mcp', ...PLATFORM_PACKAGES.map(({ id }) => `packages/native/${id}`)]) {
  const destination = path.join(root, directory);
  mkdirSync(path.join(destination, 'assets'), { recursive: true });
  for (const file of ['LICENSE-APACHE', 'LICENSE-MIT']) copyFileSync(path.join(root, file), path.join(destination, file));
  copyFileSync(hero, path.join(destination, 'assets/logo.jpg'));
}
for (const { id } of PLATFORM_PACKAGES) {
  writeFileSync(path.join(root, 'packages/native', id, 'README.md'), nativeReadme);
}
