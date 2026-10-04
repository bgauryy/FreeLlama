# FreeLlama native package for Windows x64 (MSVC)

![A brown cartoon llama stands on a pastel cloud background.](assets/logo.jpg)

This package supplies the FreeLlama executable and N-API addon for Windows x64 (MSVC). The CLI and MCP
packages select the matching artifact through optional dependencies; install those entry packages
with optional dependencies enabled.

FreeLlama gives agents a managed way to offload tasks to local Ollama models. The native core
handles model routing, admission, memory checks, and execution evidence; Ollama runs inference.

Use the [CLI package](https://www.npmjs.com/package/@octocodeai/freellama) for terminal commands or
the [MCP server](https://www.npmjs.com/package/@octocodeai/freellama-mcp-server) for agent integration.
See the [project guide](https://github.com/bgauryy/FreeLlama#readme) for setup and resource controls.
