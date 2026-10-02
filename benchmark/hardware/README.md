# Validate FreeLlama on real hardware

This matrix is a promotion gate, not a simulated benchmark. Run it only against prepared Ollama
and FreeLlama services on the hardware named by the receipt.

```bash
python3 benchmark/hardware/run_validation.py \
  --endpoint http://127.0.0.1:11435 \
  --auth-token-file ~/.local/share/freellama/auth.token \
  --gpu-model qwen3.8:27b-mlx \
  --cpu-model nomic-embed-text:latest \
  --output .octocode/hardware/apple-metal.json
```

The runner launches independent coding and embedding requests concurrently, requires verified
physical GPU and CPU receipts, validates admission and response shape, and records host and health
contracts. Add `--vision-model`, `--vision-image`, and `--vision-expected-text` to require an exact
normalized OCR transcription rather than accepting any nonempty visual response. Repeat
`--vision-stop` for model-specific repetition guards; pass `--vision-stop '\n'` for a one-line OCR
fixture.

Run this command on each prepared Apple Metal, NVIDIA Linux, AMD Linux, and NVIDIA Windows host
you intend to support. The repository has no hardware-qualification GitHub workflow; its hosted
CI and release builds do not exercise those accelerators. Each host needs Python 3, the services,
exact installed model tags, drivers, and authentication configured. Pass the host's token path
with `--auth-token-file`. A missing host, token, or model is not a pass.

Promote a row only when the uploaded JSON has `verdict: "accept"`. Results are machine- and
workload-specific; do not copy one accelerator's receipt into another row.
