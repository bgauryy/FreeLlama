# Build from source and operating notes

Published npm installs need Node.js 22+ and the matching prebuilt platform package; they do not
compile Rust. Use the [installation guide](../README.md) for that path. The commands below are for
developing this checkout or building your own executable and addon.

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
  Ollama that `serve --spawn-cpu-backend` starts (`--cpu-backend-port`), 11437 `freellama gateway`
  (`--port`).
- Port 11435 is also Ollaya's default ([Ollaya API](https://github.com/ollaya-dev/ollaya/blob/main/docs/api.md)).
  To run both, use `serve --listen` on another port and set the MCP server's
  `FREELLAMA_SERVE_ENDPOINT` to match.

## Build from source

Requires Node.js 22.12+ with Corepack and the Rust compiler selected by `rust-toolchain.toml`
(currently 1.99.0). `Cargo.toml` declares a 1.89 minimum; that floor is separate from the pinned
development compiler and needs its own check before claiming minimum-version compatibility.

```bash
corepack enable && yarn install && yarn build
./target/release/freellama serve
node packages/mcp/dist/index.js        # MCP server from this checkout
node packages/cli/bin/freellama.js run --help   # the agent commands (run, chat, gateway…)
```

npm releases can lag this checkout. [Testing](TESTING.md) describes the checks; the release gate
is `yarn verify:production`. It includes live-system tiers and all-platform release artifacts;
building successfully on the current host does not satisfy that complete gate.
