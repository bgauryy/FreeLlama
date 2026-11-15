# FreeLlama cookbooks

Short recipes for one job each: when to use it, which model, the exact call, and how to check the result. To inspect installed candidates and memory previews, ask `models {view:"catalog"}` or run `freellama catalog`.

Job/workflow calls without `model` use [machine-local calibrated routing](../MODEL_SELECTION.md#turn-measurements-into-routing-evidence)
before the portable catalog fallback. Explicit model tags in recipes are examples, not proof of
installation or current fit. Read the [check-line and tool-calling evidence](../RELIABILITY.md)
when deciding how to verify a result.

| Job | Recipe |
|---|---|
| Classify, triage, yes/no | [classify](classify.md) |
| Find the code for an issue | [find-code](find-code.md) |
| Find duplicates | [duplicates](duplicates.md) |
| Read text from an image | [ocr](ocr.md) |
| Understand what an image shows | [image-recognition](image-recognition.md) |
| Find an image by describing it | [image-search](image-search.md) |
| Recording to text | [speech-to-text](speech-to-text.md) |
| Text to speech | [text-to-speech](text-to-speech.md) |
| Small coding tasks | [coding](coding.md) |
| Research a repository | [research-agent](research-agent.md) |
| Rewrite or summarize many texts | [batch-text](batch-text.md) |
| Check a code tree, log or site in small saved chunks | [sweep](sweep.md) |
| Let FreeLlama pick the model and prompt for a job | [jobs](jobs.md) |
| Memory, context and caching | [memory-and-caching](memory-and-caching.md) |

From a shell, each `run_task` job runs with `freellama run -j <job>` (or `-w <workflow>`); the speech, research and sweep recipes run with `freellama tool audio|delegate_research|sweep '<arguments>'` ([agent CLI](../AGENT_CLI.md)).

The [cookbook integration test](../../packages/mcp/test/integration/cookbooks.test.ts) checks the JSON calls against tool schemas; it does not execute their inference or verify model quality. Measured results come from one machine: an Apple M2 Pro with 32 GB. Measure on yours before you rely on them.
