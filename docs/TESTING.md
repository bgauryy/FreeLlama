# Running the tests

Moved out of the README. Individual suite names and counts belong next to the code they cover, not
in the front door.


```bash
cargo test                              # 56 Rust tests across core + CLI contracts
cargo clippy --all-targets              # zero warnings expected
npm --prefix packages/mcp test          # protocol suite (69 assertions)

# behaviour suite — exercises every tool against the live system; needs serve + Ollama
node packages/mcp/test/validate-all.mjs
```

`validate-all.mjs` is the one that matters: it asserts what each tool *does*, not that its schema
parses — that `minConfidence` refuses **before** generating, that embedding vectors are withheld by
default, that a 143GB model is excluded on a 52GB machine, and that an unusable model is refused
without being run.

Formatting and lints:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

