# Build from source and operating notes


## Good to know

- Local inference still costs memory, power and time; offloading shrinks the calling agent's
  context, not the total work.
- Searches and recommendations never download; a pull needs approval for one exact tag and size.
- Managed tasks return one result; raw Ollama passthrough keeps streaming.
- The MCP server saves every tool result except `read_response`'s, prompts included, to
  `~/.octocode/freellama/responses/` (owner-only, oldest dropped past 1 GiB). `FREELLAMA_RESPONSE_STORE_BYTES=0` keeps nothing on disk.
- Without CA certificates, as in slim containers, FreeLlama still serves plain-HTTP Ollama. Install
  `ca-certificates` for HTTPS upstreams.
- The service binds to loopback. For remote access, see [Production](PRODUCTION.md).
- Default loopback ports: 11434 Ollama, 11435 `freellama serve` (`--listen`), 11436 the CPU-only
  Ollama that `serve --spawn-cpu-backend` starts (`--cpu-backend-port`), 11437 `fllama gateway`
  (`--port`).
- Port 11435 is also Ollaya's default ([Ollaya API](https://github.com/ollaya-dev/ollaya/blob/main/docs/api.md)).
  To run both, use `serve --listen` on another port and set the MCP server's
  `FREELLAMA_SERVE_ENDPOINT` to match.

## Build from source

Requires Rust 1.89+ (`rust-toolchain.toml` pins the compiler) and Node.js 22.12+ with Corepack.

```bash
corepack enable && yarn install && yarn build
./target/release/freellama serve
node packages/mcp/dist/index.js        # MCP server from this checkout
node packages/mcp/dist/agent-cli.js --help   # fllama agent CLI and gateway
```

npm releases can lag this checkout. [Testing](TESTING.md) describes the checks; the release gate
is `yarn verify:production`.
