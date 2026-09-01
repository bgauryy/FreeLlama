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

The manual GitHub workflow targets labeled self-hosted runners for Apple Metal, NVIDIA Linux, AMD
Linux, and NVIDIA Windows. Each corresponding GitHub environment must define the endpoint and
exact model tags. The runner must already have Bash and Python 3, the services, models, drivers,
and authentication configured. Set `FREELLAMA_HARDWARE_AUTH_TOKEN_FILE` to the token path on each
runner. A missing runner, token, or model is not a pass.

Promote a row only when the uploaded JSON has `verdict: "accept"`. Results are machine- and
workload-specific; do not copy one accelerator's receipt into another row.
