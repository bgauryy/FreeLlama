# Read text from an image (OCR)

**Use when** a screenshot, scan or photo of text must become text. Name the image by path: base64 pasted into your call costs you tens of thousands of tokens.

**Skip it when** you need to know what an image shows rather than what it says: see [image recognition](image-recognition.md).

**Model:** `hf.co/openbmb/MiniCPM-OCR-1.0-gguf:Q4_K_M` (1.4 GB), or `glm-ocr:latest` (2.2 GB).

```json
{"tool": "run_task", "arguments": {"task": "vision", "model": "hf.co/openbmb/MiniCPM-OCR-1.0-gguf:Q4_K_M", "prompt": "Text Recognition:", "files": ["screenshots/error.png"]}}
```

Both models document the prompt `Text Recognition:`. Paths must be under the server's working directory, the temp directory or `FREELLAMA_FILE_ROOTS`.

CLI: `freellama task --task vision --model glm-ocr:latest --image shot.png "Text Recognition:"`

**Check it:** crop to the text first; a whole page with one line on it can read as garbage. A note that the model looped means low confidence, so check that text. Measured here on clean rendered text only, not scans.
