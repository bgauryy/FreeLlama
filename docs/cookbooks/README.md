# FreeLlama cookbooks

Short recipes for one job each: when to use it, which model, the exact call, and how to check the result. To see what is installed and fits right now, ask `models {view:"catalog"}` or run `freellama catalog`.

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
| Memory, context and caching | [memory-and-caching](memory-and-caching.md) |

Every call here is checked against the tool schemas by a test. Measured results come from one machine: an Apple M2 Pro with 32 GB. Measure on yours before you rely on them.
