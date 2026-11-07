# Inspecting model precision, placement and activations

Reviewed 1 November 2026 against Ollama's current API documentation and source. The locally installed CLI reports 0.40.13; this session could not reach its server, so no live model or activation was inspected. The commands below are instructions, not recorded observations.

## What “which bit/neuron was enabled” can mean

| Question | Observable through stock Ollama? | Interpretation |
|---|---|---|
| Is this model Q4, Q8, FP16 or another format? | Yes: `ollama show`, `/api/show`, and model details in `/api/tags` | Weight-storage quantization; not a neuron activation switch |
| What type and shape does each weight tensor have? | Some versions/runners return `/api/show` `tensors` | Static names, types and dimensions; no per-prompt activation values |
| Is a runner using CPU, GPU or mixed placement? | `ollama ps`, `/api/ps`, and engine logs | Residency/placement evidence; not a list of active neurons |
| How likely were generated tokens? | `logprobs` and `top_logprobs`, when supported | Output-token probabilities; not hidden-layer values |
| Which hidden units had large values for token T? | No documented standard HTTP activation endpoint | Requires instrumentation of the actual inference engine |
| Which MoE experts were selected for token T? | No documented standard HTTP routing trace | Requires router/expert instrumentation; static expert counts are insufficient |

Neurons in ordinary transformer computation produce numerical activations. There is no universal enabled/disabled bit for each neuron. For example, SiLU computes `x * sigmoid(x)`, with meaningful negative as well as positive values. A rule such as `abs(activation) > threshold` is an analyst's definition of activity, not proof that the engine skipped every other unit. Mixture-of-experts routing is a different case: a router selects experts per token. [1,2,3,4]

## Inspect precision and placement without generating

Choose an exact tag from the current installed list:

```bash
ollama list
MODEL_TAG='EXACT_INSTALLED_TAG'
ollama show "$MODEL_TAG"
ollama ps
```

For a structured, read-only inspection (requires `curl` and `jq`):

```bash
jq -n --arg model "$MODEL_TAG" '{model:$model, verbose:false}' |
  curl -fsS --max-time 15 http://127.0.0.1:11434/api/show \
    -H 'Content-Type: application/json' --data-binary @- |
  jq '{details, capabilities, model_info,
       tensors: ((.tensors // []) | map({name,type,shape}))}'

curl -fsS --max-time 5 http://127.0.0.1:11434/api/ps |
  jq '.models[] | {name,digest,size,size_vram,context_length,expires_at}'
```

Read `details.quantization_level`, `model_info["general.architecture"]`, and the architecture's block/embedding/feed-forward dimensions. Expert-count fields, where present, describe model structure. The current source defines tensor metadata as `name`, `type` and `shape`; missing tensor metadata is unknown, not an empty network. `verbose:true` can return much larger fields, including tokenizer data, and does not enable activation tracing. [1,2,5]

A label such as Q4_K_M is a quantization recipe, not a guarantee that every tensor uses exactly four bits. Mixed tensor types and block scales contribute storage overhead. Inspect reported tensor types for that artifact; also keep weight precision separate from KV-cache precision and execution arithmetic. [2,6]

On unified-memory hardware, `size_vram` describes GPU allocation within shared physical memory. Do not add it to `size` as though those were independent memory pools. A GPU-resident layer is not the same thing as an active neuron. FreeLlama's configured backend and observed placement are already separated in its receipts; see [CPU/GPU routing](CPU_GPU_ROUTING.md).

## Token probabilities are already accessible

An inference request can ask for:

```json
{
  "model": "EXACT_INSTALLED_TAG",
  "messages": [{"role": "user", "content": "Write one short sentence."}],
  "stream": false,
  "logprobs": true,
  "top_logprobs": 5
}
```

This is a generation request and can load a model. It is shown for reference and was not executed. FreeLlama's MCP `run_task` already accepts `logprobs:true` and `topLogprobs:5`; full response data is available through its saved receipt. These values describe the output distribution. Generated thinking text and embedding vectors are also not a trace of every internal neuron. [7]

## Inspect activations in a compatible GGUF model

The closest upstream starting point is llama.cpp's [`llama-eval-callback`](https://github.com/ggml-org/llama.cpp/blob/master/examples/eval-callback/README.md). Its public context parameters include `cb_eval` and `cb_eval_user_data`; the example demonstrates observing graph operations and tensor data during inference. [8,9]

For an already built example and an existing compatible GGUF file:

```bash
llama-eval-callback \
  --model /absolute/path/to/existing-model.gguf \
  --prompt 'Hello' \
  --seed 42 \
  -ngl 0
```

This starts a separate model execution. It does not attach to an existing Ollama HTTP session. The example's displayed values may be abbreviated; a complete analysis needs a callback that captures the selected tensor entries, token positions and layers. Reuse its callback mechanism instead of building a tensor evaluator in FreeLlama.

For a useful trace, record:

- Exact artifact digest, engine revision, quantization, backend, tokenizer and rendered prompt/token IDs.
- Layer, token position, tensor name and a precisely defined location such as MLP post-activation, attention output or residual stream.
- A bounded sample or top-k indices by absolute activation, retaining their signed values.
- For MoE models, the selected expert IDs and corresponding routing weights at each layer/token.

Intermediate tensor names and graph structure depend on architecture and engine version. Fused kernels may not expose the desired intermediate without further instrumentation. Fetching GPU tensors can introduce synchronization and transfers; do not use an instrumented trace's timing as the normal serving benchmark. Keep traces opt-in, with limits on tokens, layers, bytes and retention.

A trace of a separate llama.cpp run is not automatically an exact trace of Ollama. Match model bytes, tokenizer, prompt rendering, options and backend, then compare outputs. Some Ollama models use another runner or representation; a GGUF callback does not inspect those engines. Instrument the engine actually executing the model when exact provenance matters.

## Python alternative for compatible research models

For a model already loaded through Transformers/PyTorch, `output_hidden_states=True` exposes layer outputs. `torch.nn.Module.register_forward_hook` captures a module's output, and a pre-hook can inspect a selected module's input. [10,11]

```python
import torch

# model and inputs must already exist in this separate research runtime.
with torch.inference_mode():
    result = model(**inputs, output_hidden_states=True, use_cache=False)
values = result.hidden_states[8][0, -1].float()
indices = values.abs().topk(min(10, values.numel())).indices
print(list(zip(indices.tolist(), values[indices].tolist())))
```

This reports large residual-channel values at one layer output for the last input token. It does not enumerate the expanded MLP's individual neurons. To inspect those, hook the architecture-specific MLP activation or the input of its down-projection and explicitly identify that measurement point. MoE expert traces require the router's selection, not merely the residual output.

This example was not run. It assumes a compatible research model already loaded locally; it is not code to pass to the Ollama API. Different weights, quantization, kernels or tokenization can change activations. Activation magnitude also does not prove which unit caused an answer; causal claims need interventions such as ablation or activation patching.

## FreeLlama integration boundary

Keep ordinary routing and resource control on stock Ollama. A future inspection tool can expose quantization, static tensor metadata and residency through existing endpoints. Label activation tracing unsupported unless a specific backend adapter actually provides it.

If deeper tracing is added, use a separate explicitly enabled debug runner or an instrumented Ollama backend, with bounded capture and paged artifacts. Rust should manage jobs, limits, cancellation and trace metadata. It should not infer neuron activity from GPU utilization, memory use, token probabilities or generated reasoning text.

## Primary references

1. [Ollama show-model API](https://docs.ollama.com/api-reference/show-model-details)
2. [Ollama API types: ShowResponse, ModelDetails and Tensor](https://github.com/ollama/ollama/blob/main/api/types.go)
3. [PyTorch SiLU](https://docs.pytorch.org/docs/stable/generated/torch.nn.functional.silu.html)
4. [Transformers expert routing](https://huggingface.co/docs/transformers/main/experts_interface)
5. [Ollama running-model API](https://docs.ollama.com/api/ps)
6. [Unsloth quantization recipes in its implementation](https://github.com/unslothai/unsloth/blob/main/unsloth/save.py)
7. [Ollama chat API: logprobs](https://docs.ollama.com/api/chat)
8. [llama.cpp evaluation callback example](https://github.com/ggml-org/llama.cpp/blob/master/examples/eval-callback/README.md)
9. [llama.cpp public context API](https://github.com/ggml-org/llama.cpp/blob/master/include/llama.h)
10. [Transformers hidden-state outputs](https://huggingface.co/docs/transformers/main_classes/output)
11. [PyTorch module hooks](https://docs.pytorch.org/docs/stable/generated/torch.nn.Module.html)
