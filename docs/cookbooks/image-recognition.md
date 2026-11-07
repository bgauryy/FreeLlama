# Understand what an image shows

**Use when** you need to know what a picture, chart or screen shows, not just its text.

**Skip it when** you only need the words: see [OCR](ocr.md).

**Model:** `lfm3-vl:2b-q4_K_M` (1.5 GB). See `models {view:"catalog", job:"image-recognition"}` for the tested alternatives.

```json
{"tool": "run_task", "arguments": {"task": "vision", "model": "lfm3-vl:2b-q4_K_M", "prompt": "Which label has the tallest bar? Answer briefly.", "files": ["charts/usage.png"]}}
```

CLI: `freellama task --task vision --model lfm3-vl:2b-q4_K_M --image chart.png "Which label has the tallest bar?"`

**Check it:** ask a specific question, not "describe this". Tested here on drawn images only; expect less on photos, and verify anything you act on.
